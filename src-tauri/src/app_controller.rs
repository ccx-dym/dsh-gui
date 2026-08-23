use crate::diagnostics::{
    DiagnosticErrorKind, DiagnosticEvent, DiagnosticSink, DiagnosticStage, FileDiagnosticSink,
    OperationTrace, TraceKind,
};
use crate::domain::{AppPhase, RuntimeEvent, RuntimeStatus};
use crate::paths::AppPaths;
#[cfg(any(not(debug_assertions), test))]
use crate::paths::RuntimeLayout;
use crate::runtime::command::{RuntimeLaunchSpec, reserve_loopback_port};
#[cfg(not(debug_assertions))]
use crate::runtime::install_state::InstallStateStore;
use crate::runtime::install_state::{ActiveDeployment, RuntimeSkinCompatibility};
use crate::runtime::{RuntimeError, RuntimeEventSink, RuntimeSupervisor};
use crate::update::activation::{
    RuntimeBusyProvider, RuntimeBusyState, UnknownRuntimeBusyProvider,
};
#[cfg(debug_assertions)]
use std::env;
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
#[cfg(any(not(debug_assertions), test))]
use std::time::Instant;
use tauri::Emitter;
use tauri::{AppHandle, Manager};

#[cfg(debug_assertions)]
const MOCK_READY_TIMEOUT: Duration = Duration::from_secs(10);

const RUNTIME_STOP_GRACE: Duration = Duration::from_secs(2);

#[cfg(not(debug_assertions))]
const OFFICIAL_READY_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) trait RuntimeLifecycle: Send + Sync + 'static {
    fn start(&self) -> Result<(), RuntimeError>;
    fn stop(&self) -> Result<(), RuntimeError>;
    fn start_and_wait_ready(&self, deployment: &ActiveDeployment) -> Result<(), RuntimeError>;

    fn is_definitively_stopped(&self) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}

trait RuntimeUi: Send + Sync + 'static {
    fn emit_status(&self, event: &RuntimeEvent) -> Result<(), RuntimeError>;
    fn navigate_main(&self, url: &tauri::Url) -> Result<(), RuntimeError>;
}

struct TauriRuntimeUi {
    app: AppHandle,
    trace: OperationTrace,
    dsh_version: Option<semver::Version>,
    skin_compatibility: RuntimeSkinCompatibility,
}

#[cfg(debug_assertions)]
struct MockRuntimeLifecycle {
    supervisor: Arc<RuntimeSupervisor>,
    status: Arc<RwLock<RuntimeStatus>>,
    app: AppHandle,
    local_url: tauri::Url,
}

#[cfg(debug_assertions)]
impl RuntimeLifecycle for MockRuntimeLifecycle {
    fn start(&self) -> Result<(), RuntimeError> {
        let paths =
            AppPaths::resolve(&self.app).map_err(|error| RuntimeError::Tauri(error.to_string()))?;
        paths
            .ensure_exists()
            .map_err(|error| RuntimeError::Tauri(error.to_string()))?;
        // 每次启动都重新占用并立即释放一个动态端口，restart 不复用旧 spec/端口。
        let port = reserve_loopback_port()?;
        let node = env::var_os("DSH_DESKTOP_NODE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("node.exe"));
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/mock-dsh.mjs");
        let spec = RuntimeLaunchSpec::mock(node, script, paths.dsh_home, port);
        let ui: Arc<dyn RuntimeUi> = Arc::new(TauriRuntimeUi {
            app: self.app.clone(),
            trace: OperationTrace::begin(TraceKind::Runtime),
            dsh_version: None,
            skin_compatibility: RuntimeSkinCompatibility::Unverified,
        });
        let sink: Arc<dyn RuntimeEventSink> = Arc::new(ControllerEventSink::new(
            Arc::clone(&self.status),
            ui,
            self.local_url.clone(),
        ));
        self.supervisor.start(spec, MOCK_READY_TIMEOUT, sink)
    }

    fn stop(&self) -> Result<(), RuntimeError> {
        self.supervisor.stop(RUNTIME_STOP_GRACE)
    }

    fn start_and_wait_ready(&self, _deployment: &ActiveDeployment) -> Result<(), RuntimeError> {
        // 开发 mock 的异步启动不满足激活事务的同步“真实就绪”契约，必须失败关闭。
        Err(RuntimeError::MockRuntimeDisabled)
    }

    fn is_definitively_stopped(&self) -> Result<bool, RuntimeError> {
        self.supervisor.is_inactive()
    }
}

#[cfg(not(debug_assertions))]
struct OfficialRuntimeLifecycle {
    supervisor: Arc<RuntimeSupervisor>,
    status: Arc<RwLock<RuntimeStatus>>,
    app: AppHandle,
    local_url: tauri::Url,
    layout: RuntimeLayout,
}

#[cfg(not(debug_assertions))]
impl OfficialRuntimeLifecycle {
    fn start_exact(&self, deployment: &ActiveDeployment) -> Result<u16, RuntimeError> {
        let port = reserve_loopback_port()?;
        let spec = official_launch_spec(&self.layout, deployment, port)?;
        let ui: Arc<dyn RuntimeUi> = Arc::new(TauriRuntimeUi {
            app: self.app.clone(),
            trace: OperationTrace::begin(TraceKind::Runtime),
            dsh_version: Some(deployment.runtime.version.clone()),
            skin_compatibility: deployment.runtime.skin_compatibility,
        });
        let sink: Arc<dyn RuntimeEventSink> = Arc::new(ControllerEventSink::new(
            Arc::clone(&self.status),
            ui,
            self.local_url.clone(),
        ));
        self.supervisor.start(spec, OFFICIAL_READY_TIMEOUT, sink)?;
        Ok(port)
    }
}

#[cfg(not(debug_assertions))]
impl RuntimeLifecycle for OfficialRuntimeLifecycle {
    fn start(&self) -> Result<(), RuntimeError> {
        let deployment = InstallStateStore::new(self.layout.clone())
            .load()
            .map_err(|_| RuntimeError::DeploymentChanged)?;
        self.start_exact(&deployment).map(|_| ())
    }

    fn stop(&self) -> Result<(), RuntimeError> {
        self.supervisor.stop(RUNTIME_STOP_GRACE)
    }

    fn start_and_wait_ready(&self, deployment: &ActiveDeployment) -> Result<(), RuntimeError> {
        let authoritative = InstallStateStore::new(self.layout.clone())
            .load()
            .map_err(|_| RuntimeError::DeploymentChanged)?;
        if &authoritative != deployment {
            return Err(RuntimeError::DeploymentChanged);
        }
        let port = self.start_exact(deployment)?;
        // supervisor 自己拥有 60 秒 readiness 截止；外层只作为清理兜底，避免两个
        // 相同截止互相竞态并留下 Starting 子进程。
        let watchdog = OFFICIAL_READY_TIMEOUT + RUNTIME_STOP_GRACE + Duration::from_secs(1);
        wait_for_official_ready(&self.status, port, watchdog, || {
            self.supervisor
                .abort_startup(RUNTIME_STOP_GRACE + Duration::from_secs(1))
        })
    }

    fn is_definitively_stopped(&self) -> Result<bool, RuntimeError> {
        self.supervisor.is_inactive()
    }
}

#[cfg(any(not(debug_assertions), test))]
fn wait_for_official_ready<F>(
    status: &Arc<RwLock<RuntimeStatus>>,
    port: u16,
    watchdog: Duration,
    mut stop_on_timeout: F,
) -> Result<(), RuntimeError>
where
    F: FnMut() -> Result<(), RuntimeError>,
{
    let deadline = Instant::now() + watchdog;
    loop {
        match status
            .read()
            .map_err(|_| RuntimeError::StatePoisoned)?
            .phase
        {
            AppPhase::Ready => return Ok(()),
            AppPhase::Failed => {
                return Err(RuntimeError::Tauri(
                    "official runtime failed before readiness".to_owned(),
                ));
            }
            AppPhase::Idle | AppPhase::Starting | AppPhase::Stopping => {}
        }
        if Instant::now() >= deadline {
            stop_on_timeout()?;
            return Err(RuntimeError::HealthTimeout {
                port,
                timeout_ms: watchdog.as_millis() as u64,
            });
        }
        std::thread::sleep(Duration::from_millis(25).min(watchdog));
    }
}

#[cfg(any(not(debug_assertions), test))]
fn official_launch_spec(
    layout: &RuntimeLayout,
    deployment: &ActiveDeployment,
    port: u16,
) -> Result<RuntimeLaunchSpec, RuntimeError> {
    let node_version = &deployment.runtime.node_version;
    let project_workspace =
        deployment
            .project_workspace
            .as_ref()
            .ok_or(RuntimeError::InvalidLaunchPath {
                field: "project_workspace",
                reason: "missing descriptor",
            })?;
    let runtime_dir = layout.runtime_dir(&deployment.runtime);
    RuntimeLaunchSpec::official(
        runtime_dir.clone(),
        runtime_dir
            .join(format!("node-v{node_version}-win-x64"))
            .join("node.exe"),
        runtime_dir.join("app/node_modules/@deepseek-ai/dsh/lib/bin.js"),
        project_workspace.clone(),
        layout.generation_dir(&deployment.data),
        port,
    )
}

impl RuntimeUi for TauriRuntimeUi {
    fn emit_status(&self, event: &RuntimeEvent) -> Result<(), RuntimeError> {
        record_runtime_diagnostic(&self.app, &self.trace, event);
        self.app
            .emit("runtime-status", event)
            .map_err(|error| RuntimeError::Tauri(error.to_string()))
    }

    fn navigate_main(&self, url: &tauri::Url) -> Result<(), RuntimeError> {
        if let Some(adapter) = self.app.try_state::<crate::skin::SkinAdapterController>() {
            if let Some(version) = self.dsh_version.as_ref() {
                adapter.bind_navigation(version, self.skin_compatibility, url);
            } else {
                adapter.clear();
            }
        }
        let Some(main) = self.app.get_webview_window("main") else {
            if let Some(adapter) = self.app.try_state::<crate::skin::SkinAdapterController>() {
                adapter.clear();
            }
            return Err(RuntimeError::MainWindowMissing);
        };
        let result = main
            .navigate(url.clone())
            .map_err(|error| RuntimeError::Tauri(error.to_string()));
        if result.is_err()
            && let Some(adapter) = self.app.try_state::<crate::skin::SkinAdapterController>()
        {
            adapter.clear();
        }
        result
    }
}

fn record_runtime_diagnostic(
    app: &AppHandle,
    trace: &OperationTrace,
    runtime_event: &RuntimeEvent,
) {
    let Some(sink) = app.try_state::<FileDiagnosticSink>() else {
        return;
    };
    let (stage, elapsed_ms, error_kind) = match runtime_event {
        RuntimeEvent::Starting { .. } => (DiagnosticStage::RuntimeStart, 0, None),
        RuntimeEvent::Ready { elapsed_ms, .. } => {
            (DiagnosticStage::RuntimeReady, *elapsed_ms, None)
        }
        RuntimeEvent::Failed { .. } => (
            DiagnosticStage::RuntimeFailed,
            0,
            Some(DiagnosticErrorKind::RuntimeFailure),
        ),
        RuntimeEvent::Stopping { .. } => (DiagnosticStage::RuntimeStopping, 0, None),
    };
    sink.record(DiagnosticEvent::new(
        trace,
        stage,
        elapsed_ms,
        0,
        Some(std::process::id()),
        error_kind,
    ));
}

struct ControllerEventSink {
    status: Arc<RwLock<RuntimeStatus>>,
    ui: Arc<dyn RuntimeUi>,
    local_url: tauri::Url,
}

impl ControllerEventSink {
    fn new(
        status: Arc<RwLock<RuntimeStatus>>,
        ui: Arc<dyn RuntimeUi>,
        local_url: tauri::Url,
    ) -> Self {
        Self {
            status,
            ui,
            local_url,
        }
    }
}

impl RuntimeEventSink for ControllerEventSink {
    fn emit(&self, event: RuntimeEvent) -> Result<(), RuntimeError> {
        let next_status = status_from_event(&event);
        {
            // 状态快照更新与 UI I/O 分离，避免 WebView 事件或导航期间持有读写锁。
            let mut status = self
                .status
                .write()
                .map_err(|_| RuntimeError::StatePoisoned)?;
            *status = next_status;
        }

        self.ui.emit_status(&event)?;
        match event {
            RuntimeEvent::Ready { url, .. } => {
                let url = strict_loopback_url(&url)?;
                self.ui.navigate_main(&url)
            }
            RuntimeEvent::Failed { .. } | RuntimeEvent::Stopping { .. } => {
                self.ui.navigate_main(&self.local_url)
            }
            RuntimeEvent::Starting { .. } => Ok(()),
        }
    }
}

fn status_from_event(event: &RuntimeEvent) -> RuntimeStatus {
    match event {
        RuntimeEvent::Starting { message } => RuntimeStatus {
            phase: AppPhase::Starting,
            message: message.clone(),
            ..RuntimeStatus::default()
        },
        RuntimeEvent::Ready { url, elapsed_ms } => RuntimeStatus {
            phase: AppPhase::Ready,
            message: "DSH 已就绪".to_owned(),
            url: Some(url.clone()),
            elapsed_ms: Some(*elapsed_ms),
            error_code: None,
        },
        RuntimeEvent::Failed { code, message } => RuntimeStatus {
            phase: AppPhase::Failed,
            message: message.clone(),
            error_code: Some(code.clone()),
            ..RuntimeStatus::default()
        },
        RuntimeEvent::Stopping { message } => RuntimeStatus {
            phase: AppPhase::Stopping,
            message: message.clone(),
            ..RuntimeStatus::default()
        },
    }
}

fn strict_loopback_url(value: &str) -> Result<tauri::Url, RuntimeError> {
    let url =
        tauri::Url::parse(value).map_err(|error| RuntimeError::InvalidUrl(error.to_string()))?;
    let valid = url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none();
    if valid {
        Ok(url)
    } else {
        Err(RuntimeError::InvalidUrl(value.to_owned()))
    }
}

fn initial_runtime_status(mock_runtime_enabled: bool) -> RuntimeStatus {
    if mock_runtime_enabled {
        RuntimeStatus {
            phase: crate::domain::AppPhase::Idle,
            message: "等待启动".to_owned(),
            ..RuntimeStatus::default()
        }
    } else {
        // 正式构建在阶段 2 接入兼容运行时前必须明确保持 Idle，避免把“没有
        // 运行时”误报成仍在启动，也为后续本地启动页查询提供稳定提示。
        RuntimeStatus {
            phase: crate::domain::AppPhase::Idle,
            message: "尚未安装兼容运行时".to_owned(),
            ..RuntimeStatus::default()
        }
    }
}

/// 协调运行时状态、后台生命周期任务与单一 Tauri 主窗口。
pub struct AppController {
    runtime: Arc<dyn RuntimeLifecycle>,
    status: Arc<RwLock<RuntimeStatus>>,
    exit_requested: AtomicBool,
    operation_gate: Arc<AtomicBool>,
    busy_provider: Arc<dyn RuntimeBusyProvider>,
}

/// 一次激活事务持有的不可伪造生命周期会话。
pub struct ActivationSession {
    runtime: Arc<dyn RuntimeLifecycle>,
    status: Arc<RwLock<RuntimeStatus>>,
    lease: ProbeLease,
    transaction_claimed: AtomicBool,
}

impl ActivationSession {
    /// 原子声明本 session 的唯一 activation/recovery 事务。
    ///
    /// :return: 首次声明成功时返回。
    /// :raises RuntimeError: session 已被消费时返回稳定的门禁错误。
    pub(crate) fn claim_transaction(&self) -> Result<(), RuntimeError> {
        self.transaction_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| RuntimeError::ProbeOperationInProgress)
    }
    /// 返回供隔离探活绑定的 lease；会话仍持有原 lease 直到事务终态。
    ///
    /// :return: 共享同一 operation guard 的 probe lease。
    /// :raises: 克隆值对象不产生错误。
    pub fn probe_lease(&self) -> ProbeLease {
        self.lease.clone()
    }

    /// 从权威 pointer 重读并比对精确配对后，同步等待真实 runtime 就绪。
    ///
    /// :param store: 权威 deployment pointer 存储。
    /// :param expected: journal 正在提交的 runtime/data 配对。
    /// :return: pointer 未变化且 runtime 已真实就绪时返回。
    /// :raises RuntimeError: pointer 不匹配、读取失败或 runtime 首启失败时返回。
    pub(crate) async fn start_and_wait_ready(
        &self,
        actual: &ActiveDeployment,
        expected: &ActiveDeployment,
    ) -> Result<(), RuntimeError> {
        if actual != expected {
            return Err(RuntimeError::DeploymentChanged);
        }
        let runtime = Arc::clone(&self.runtime);
        let status = Arc::clone(&self.status);
        let expected = expected.clone();
        tokio::task::spawn_blocking(move || {
            runtime.start_and_wait_ready(&expected)?;
            let mut status = status.write().map_err(|_| RuntimeError::StatePoisoned)?;
            *status = RuntimeStatus {
                phase: crate::domain::AppPhase::Ready,
                message: "DSH 已就绪".to_owned(),
                ..RuntimeStatus::default()
            };
            Ok(())
        })
        .await
        .map_err(|_| RuntimeError::Tauri("activation lifecycle worker failed".to_owned()))?
    }

    /// 在首启失败或回滚前停止会话内 runtime，不重新获取 operation gate。
    ///
    /// :return: runtime 已停止且控制器状态回到 Idle 时返回。
    /// :raises RuntimeError: 停止或状态锁失败时返回。
    pub(crate) async fn stop(&self) -> Result<(), RuntimeError> {
        let runtime = Arc::clone(&self.runtime);
        let status = Arc::clone(&self.status);
        tokio::task::spawn_blocking(move || {
            runtime.stop()?;
            let mut status = status.write().map_err(|_| RuntimeError::StatePoisoned)?;
            *status = RuntimeStatus {
                phase: crate::domain::AppPhase::Idle,
                message: "DSH 已停止".to_owned(),
                ..RuntimeStatus::default()
            };
            Ok(())
        })
        .await
        .map_err(|_| RuntimeError::Tauri("activation lifecycle worker failed".to_owned()))?
    }
}

/// 独占一次已停止 runtime 的更新/探活窗口。
///
/// 值只能由 `AppController` 在原子确认 Idle/Failed 后签发；离开作用域时自动
/// 释放门禁，避免调用方伪造“已停止”快照或在异步探活中途启动第二个 DSH。
#[derive(Clone, Debug)]
pub struct ProbeLease {
    inner: Arc<ProbeLeaseInner>,
}

#[derive(Debug)]
struct ProbeLeaseInner {
    _guard: OperationGuard,
    probe_active: AtomicBool,
}

/// 单个 lease 上一次独占 probe execution 的所有权。
#[derive(Clone, Debug)]
pub(crate) struct ProbeExecutionPermit {
    _ownership: Arc<ProbeExecutionOwnership>,
}

#[derive(Debug)]
struct ProbeExecutionOwnership {
    inner: Arc<ProbeLeaseInner>,
}

impl Drop for ProbeExecutionOwnership {
    fn drop(&mut self) {
        self.inner.probe_active.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
struct OperationGuard {
    gate: Arc<AtomicBool>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.gate.store(false, Ordering::Release);
    }
}

impl ProbeLease {
    /// 创建仅供 debug/integration 测试使用的独立 lease。
    ///
    /// 正式 release 构建不会编译此入口，生产 lease 只能由 `AppController` 签发。
    ///
    /// :return: 与独立测试门禁绑定的 lease。
    /// :raises: 此测试构造器不产生错误。
    #[cfg(debug_assertions)]
    pub fn for_test() -> Self {
        let gate = Arc::new(AtomicBool::new(true));
        Self {
            inner: Arc::new(ProbeLeaseInner {
                _guard: OperationGuard { gate },
                probe_active: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn claim_probe(&self) -> Result<ProbeExecutionPermit, RuntimeError> {
        self.inner
            .probe_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RuntimeError::ProbeOperationInProgress)?;
        Ok(ProbeExecutionPermit {
            _ownership: Arc::new(ProbeExecutionOwnership {
                inner: Arc::clone(&self.inner),
            }),
        })
    }
}

impl AppController {
    /// 创建连接单一主窗口的应用控制器。
    ///
    /// :param app: Tauri 应用句柄，用于解析目录、发事件和导航主窗口。
    /// :return: 保存当前本地启动页 URL 的控制器。
    /// :raises RuntimeError: `main` 窗口不存在或无法读取本地 URL 时返回。
    pub fn new(app: AppHandle) -> Result<Self, RuntimeError> {
        Self::new_with_busy_provider(app, Arc::new(UnknownRuntimeBusyProvider))
    }

    /// 使用可信 Agent quiesce 能力构造控制器；仅供桌面端正式接线调用。
    ///
    /// :param app: Tauri 应用句柄与主窗口。
    /// :param busy_provider: 能原子冻结新任务并确认空闲的可信 provider。
    /// :return: 已接线 runtime lifecycle 与 busy provider 的控制器。
    /// :raises RuntimeError: 主窗口或本地启动 URL 无效时返回。
    pub(crate) fn new_with_busy_provider(
        app: AppHandle,
        busy_provider: Arc<dyn RuntimeBusyProvider>,
    ) -> Result<Self, RuntimeError> {
        let local_url = app
            .get_webview_window("main")
            .ok_or(RuntimeError::MainWindowMissing)?
            .url()
            .map_err(|error| RuntimeError::Tauri(error.to_string()))?;
        let status = Arc::new(RwLock::new(initial_runtime_status(cfg!(debug_assertions))));
        #[cfg(debug_assertions)]
        let runtime: Arc<dyn RuntimeLifecycle> = Arc::new(MockRuntimeLifecycle {
            supervisor: Arc::new(RuntimeSupervisor::new()),
            status: Arc::clone(&status),
            app,
            local_url,
        });
        #[cfg(not(debug_assertions))]
        let runtime: Arc<dyn RuntimeLifecycle> = {
            let paths =
                AppPaths::resolve(&app).map_err(|error| RuntimeError::Tauri(error.to_string()))?;
            paths
                .ensure_exists()
                .map_err(|error| RuntimeError::Tauri(error.to_string()))?;
            Arc::new(OfficialRuntimeLifecycle {
                supervisor: Arc::new(RuntimeSupervisor::new()),
                status: Arc::clone(&status),
                app,
                local_url,
                layout: RuntimeLayout::from_paths(&paths),
            })
        };
        Ok(Self {
            runtime,
            status,
            exit_requested: AtomicBool::new(false),
            operation_gate: Arc::new(AtomicBool::new(false)),
            busy_provider,
        })
    }

    #[cfg(test)]
    fn for_test(runtime: Arc<dyn RuntimeLifecycle>) -> Self {
        Self {
            runtime,
            status: Arc::new(RwLock::new(initial_runtime_status(true))),
            exit_requested: AtomicBool::new(false),
            operation_gate: Arc::new(AtomicBool::new(false)),
            busy_provider: Arc::new(UnknownRuntimeBusyProvider),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_busy(
        runtime: Arc<dyn RuntimeLifecycle>,
        busy_provider: Arc<dyn RuntimeBusyProvider>,
    ) -> Self {
        Self {
            runtime,
            status: Arc::new(RwLock::new(initial_runtime_status(true))),
            exit_requested: AtomicBool::new(false),
            operation_gate: Arc::new(AtomicBool::new(false)),
            busy_provider,
        }
    }

    /// 返回与前端 TypeScript `RuntimeStatus` 字段完全对齐的当前快照。
    ///
    /// :return: 当前状态的独立克隆；锁中毒时返回可恢复的最后值。
    /// :raises: 此只读接口不向 Tauri command 暴露锁错误。
    pub fn status(&self) -> RuntimeStatus {
        self.status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 启动仅开发构建可用的模拟 DSH。
    ///
    /// :return: 启动任务成功提交到后台线程时返回 `Ok(())`。
    /// :raises RuntimeError: 正式构建调用、目录创建、端口申请或重复启动时返回。
    pub fn start_mock_runtime(&self) -> Result<(), RuntimeError> {
        let _guard = self.acquire_operation()?;
        self.runtime.start()
    }

    /// 从权威 deployment pointer 启动正式运行时。
    ///
    /// :return: 启动任务成功提交时返回。
    /// :raises RuntimeError: 当前已有生命周期操作或 pointer/运行时无效时返回。
    pub fn start_active_runtime(&self) -> Result<(), RuntimeError> {
        let _guard = self.acquire_operation()?;
        self.runtime.start()
    }

    /// 仅在权威 pointer 仍精确等于指定 prior deployment 时恢复并等待就绪。
    ///
    /// :param expected: precommit 失败前读取的完整 runtime/data/workspace 配对。
    /// :return: pointer 未变化且旧 runtime 已真实就绪时返回。
    /// :raises RuntimeError: 生命周期门禁、pointer 复核或启动就绪失败时返回。
    #[cfg(not(debug_assertions))]
    pub(crate) async fn start_exact_active_runtime(
        &self,
        expected: &ActiveDeployment,
    ) -> Result<(), RuntimeError> {
        let _guard = self.acquire_operation()?;
        let runtime = Arc::clone(&self.runtime);
        let status = Arc::clone(&self.status);
        let expected = expected.clone();
        tokio::task::spawn_blocking(move || {
            runtime.start_and_wait_ready(&expected)?;
            let mut status = status.write().map_err(|_| RuntimeError::StatePoisoned)?;
            *status = RuntimeStatus {
                phase: crate::domain::AppPhase::Ready,
                message: "DSH 已就绪".to_owned(),
                ..RuntimeStatus::default()
            };
            Ok(())
        })
        .await
        .map_err(|_| RuntimeError::Tauri("activation lifecycle worker failed".to_owned()))?
    }

    /// 失败后重新构造端口与模拟启动参数并提交后台启动。
    ///
    /// :return: 新启动任务成功提交时返回 `Ok(())`。
    /// :raises RuntimeError: 与 `start_mock_runtime` 相同。
    pub fn retry(&self) -> Result<(), RuntimeError> {
        self.start_mock_runtime()
    }

    /// 同步停止当前运行时，阻塞操作由运行时实现负责且不持有控制器状态锁。
    ///
    /// :return: 运行时已完整停止时返回 `Ok(())`。
    /// :raises RuntimeError: 运行时停止失败时原样返回。
    pub fn stop(&self) -> Result<(), RuntimeError> {
        let _guard = self.acquire_operation()?;
        self.stop_under_gate()
    }

    fn stop_under_gate(&self) -> Result<(), RuntimeError> {
        self.runtime.stop()?;
        let mut status = self
            .status
            .write()
            .map_err(|_| RuntimeError::StatePoisoned)?;
        *status = RuntimeStatus {
            phase: crate::domain::AppPhase::Idle,
            message: "DSH 已停止".to_owned(),
            ..RuntimeStatus::default()
        };
        Ok(())
    }

    /// 先完整停止旧运行时，再重新生成启动参数并启动。
    ///
    /// :return: 新启动任务成功提交时返回 `Ok(())`。
    /// :raises RuntimeError: 停止或重新启动任一步失败时返回，后一步不会越过前一步。
    pub fn restart(&self) -> Result<(), RuntimeError> {
        let _guard = self.acquire_operation()?;
        self.stop_under_gate()?;
        self.runtime.start()
    }

    /// 请求显式退出；只有运行时停止成功后才开放窗口/应用退出。
    ///
    /// :return: 运行时已停止且退出标志已提交时返回 `Ok(())`。
    /// :raises RuntimeError: 停止失败时返回且退出标志保持 false。
    pub fn request_exit(&self) -> Result<(), RuntimeError> {
        let _guard = self.acquire_operation()?;
        self.stop_under_gate()?;
        self.exit_requested.store(true, Ordering::Release);
        Ok(())
    }

    /// 返回托盘 Exit 是否已完成运行时停止阶段。
    ///
    /// :return: 仅 `request_exit` 成功后为 true。
    /// :raises: 原子只读操作不产生错误。
    pub fn exit_requested(&self) -> bool {
        self.exit_requested.load(Ordering::Acquire)
    }

    /// 原子确认当前控制器状态并独占隔离 runtime 探活窗口。
    ///
    /// Task 9 的激活器负责先执行 stop；此门禁只阻止在 Starting、Ready 或 Stopping
    /// 状态下误启第二个 DSH，不会自行停止进程，也不会触发 WebView 导航。
    ///
    /// :return: Idle/Failed 快照返回已确认停止的值对象。
    /// :raises RuntimeError: 当前 runtime 可能正在启动、运行或停止时拒绝探活。
    pub fn runtime_stopped_for_probe(&self) -> Result<ProbeLease, RuntimeError> {
        let guard = self.acquire_operation()?;
        match self.status().phase {
            crate::domain::AppPhase::Idle | crate::domain::AppPhase::Failed => Ok(ProbeLease {
                inner: Arc::new(ProbeLeaseInner {
                    _guard: guard,
                    probe_active: AtomicBool::new(false),
                }),
            }),
            crate::domain::AppPhase::Starting
            | crate::domain::AppPhase::Ready
            | crate::domain::AppPhase::Stopping => Err(RuntimeError::ProbeRequiresStoppedRuntime),
        }
    }

    /// 原子确认 Agent 空闲并将 Ready runtime 受控停止后创建独占激活会话。
    ///
    /// lifecycle 与 Agent busy 分开判断：Starting/Stopping 以及 ActiveTask/UnknownBusy
    /// 均失败关闭；Ready 只在 ConfirmedIdle 时执行一次 stop。lease 生命周期覆盖后续
    /// candidate、probe、指针提交、首启与回滚，防止其他生命周期操作插入事务。
    ///
    /// :return: 已确认 runtime 停止且独占操作门禁的会话。
    /// :raises RuntimeError: Agent 非空闲、生命周期处于转换态、停止失败或已有操作时返回。
    pub fn begin_activation(&self) -> Result<ActivationSession, RuntimeError> {
        let guard = self.acquire_operation()?;
        let phase = self.status().phase;
        let inactive = matches!(
            phase,
            crate::domain::AppPhase::Idle | crate::domain::AppPhase::Failed
        ) && self.runtime.is_definitively_stopped()?;
        if !inactive && self.busy_provider.quiesce() != RuntimeBusyState::ConfirmedIdle {
            return Err(RuntimeError::ActivationBusy);
        }
        match phase {
            crate::domain::AppPhase::Ready => self.stop_under_gate()?,
            crate::domain::AppPhase::Idle | crate::domain::AppPhase::Failed => {}
            crate::domain::AppPhase::Starting | crate::domain::AppPhase::Stopping => {
                return Err(RuntimeError::ProbeRequiresStoppedRuntime);
            }
        }
        let lease = ProbeLease {
            inner: Arc::new(ProbeLeaseInner {
                _guard: guard,
                probe_active: AtomicBool::new(false),
            }),
        };
        Ok(ActivationSession {
            runtime: Arc::clone(&self.runtime),
            status: Arc::clone(&self.status),
            lease,
            transaction_claimed: AtomicBool::new(false),
        })
    }

    fn acquire_operation(&self) -> Result<OperationGuard, RuntimeError> {
        self.operation_gate
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RuntimeError::ProbeOperationInProgress)?;
        Ok(OperationGuard {
            gate: Arc::clone(&self.operation_gate),
        })
    }
}

#[tauri::command]
/// 获取供本地启动页渲染的运行时状态快照。
///
/// :param controller: Tauri 管理的应用控制器。
/// :return: 与 TypeScript 字段对齐的完整状态。
/// :raises: 此命令不返回错误。
pub fn get_runtime_status(controller: tauri::State<'_, AppController>) -> RuntimeStatus {
    controller.status()
}

#[tauri::command]
/// 请求重新启动开发模拟运行时。
///
/// :param controller: Tauri 管理的应用控制器。
/// :return: 后台启动成功调度时返回空结果。
/// :raises String: 重复启动、目录或正式构建限制等错误的可显示文本。
pub fn retry_runtime(controller: tauri::State<'_, AppController>) -> Result<(), String> {
    controller
        .retry()
        .map_err(|error| error.safe_user_message().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        AppController, ControllerEventSink, RuntimeLifecycle, RuntimeUi, initial_runtime_status,
        wait_for_official_ready,
    };
    use crate::domain::{AppPhase, RuntimeEvent, RuntimeStatus};
    use crate::runtime::install_state::{ActiveDeployment, DataGeneration, InstalledRuntime};
    use crate::runtime::{RuntimeError, RuntimeEventSink};
    use crate::update::activation::{RuntimeBusyProvider, RuntimeBusyState};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingRuntime {
        calls: Mutex<Vec<&'static str>>,
        fail_stop: bool,
    }

    struct FixedBusyProvider(RuntimeBusyState);

    struct DefinitivelyStoppedRuntime;

    impl RuntimeLifecycle for DefinitivelyStoppedRuntime {
        fn start(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn stop(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn start_and_wait_ready(&self, _deployment: &ActiveDeployment) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn is_definitively_stopped(&self) -> Result<bool, RuntimeError> {
            Ok(true)
        }
    }

    impl RuntimeBusyProvider for FixedBusyProvider {
        fn quiesce(&self) -> RuntimeBusyState {
            self.0
        }
    }

    #[test]
    fn activation_gate_stops_ready_runtime_only_when_agent_is_confirmed_idle() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test_with_busy(
            runtime.clone(),
            Arc::new(FixedBusyProvider(RuntimeBusyState::ConfirmedIdle)),
        );
        controller.status.write().expect("状态锁不应中毒").phase = AppPhase::Ready;

        let session = controller
            .begin_activation()
            .expect("Ready 且确认空闲应受控停止并签发 lease");

        assert_eq!(
            runtime.calls.lock().expect("调用记录锁不应中毒").as_slice(),
            ["stop"]
        );
        assert!(matches!(controller.status().phase, AppPhase::Idle));
        assert!(matches!(
            controller.start_mock_runtime(),
            Err(RuntimeError::ProbeOperationInProgress)
        ));
        drop(session);
        controller
            .start_mock_runtime()
            .expect("lease 释放后应恢复生命周期操作");
    }

    #[test]
    fn activation_gate_rejects_busy_unknown_and_transitioning_runtime_without_stopping() {
        for busy in [RuntimeBusyState::ActiveTask, RuntimeBusyState::UnknownBusy] {
            let runtime = Arc::new(RecordingRuntime::default());
            let controller = AppController::for_test_with_busy(
                runtime.clone(),
                Arc::new(FixedBusyProvider(busy)),
            );
            controller.status.write().expect("状态锁不应中毒").phase = AppPhase::Ready;

            assert!(controller.begin_activation().is_err());
            assert!(runtime.calls.lock().expect("调用记录锁不应中毒").is_empty());
        }

        for phase in [AppPhase::Starting, AppPhase::Stopping] {
            let runtime = Arc::new(RecordingRuntime::default());
            let controller = AppController::for_test_with_busy(
                runtime.clone(),
                Arc::new(FixedBusyProvider(RuntimeBusyState::ConfirmedIdle)),
            );
            controller.status.write().expect("状态锁不应中毒").phase = phase;

            assert!(controller.begin_activation().is_err());
            assert!(runtime.calls.lock().expect("调用记录锁不应中毒").is_empty());
        }

        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test_with_busy(
            runtime.clone(),
            Arc::new(FixedBusyProvider(RuntimeBusyState::UnknownBusy)),
        );
        assert!(matches!(
            controller.begin_activation(),
            Err(RuntimeError::ActivationBusy)
        ));
        assert!(runtime.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn trusted_inactive_lifecycle_allows_fresh_activation_but_not_ready() {
        let controller = AppController::for_test_with_busy(
            Arc::new(DefinitivelyStoppedRuntime),
            Arc::new(FixedBusyProvider(RuntimeBusyState::UnknownBusy)),
        );
        let session = controller
            .begin_activation()
            .expect("fresh inactive session");
        drop(session);

        controller.status.write().expect("status").phase = AppPhase::Ready;
        assert!(matches!(
            controller.begin_activation(),
            Err(RuntimeError::ActivationBusy)
        ));
    }

    #[test]
    fn activation_session_claim_is_single_use_for_the_entire_transaction() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test_with_busy(
            runtime,
            Arc::new(FixedBusyProvider(RuntimeBusyState::ConfirmedIdle)),
        );
        let session = controller.begin_activation().expect("session");

        session.claim_transaction().expect("first claim");
        assert!(matches!(
            session.claim_transaction(),
            Err(RuntimeError::ProbeOperationInProgress)
        ));
    }

    #[test]
    fn official_ready_watchdog_stops_a_stuck_starting_runtime() {
        let status = Arc::new(RwLock::new(RuntimeStatus {
            phase: AppPhase::Starting,
            ..RuntimeStatus::default()
        }));
        let stopped = AtomicBool::new(false);

        let error = wait_for_official_ready(&status, 43123, Duration::from_millis(5), || {
            stopped.store(true, Ordering::SeqCst);
            Ok(())
        })
        .expect_err("watchdog must time out");

        assert!(stopped.load(Ordering::SeqCst));
        assert!(matches!(
            error,
            RuntimeError::HealthTimeout { port: 43123, .. }
        ));
    }

    #[test]
    fn official_activation_spec_uses_only_persisted_runtime_descriptor_fields() {
        let root = std::env::temp_dir().join("dsh-official-spec-descriptor");
        let paths = crate::paths::AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
        let layout = crate::paths::RuntimeLayout::from_paths(&paths);
        let runtime = InstalledRuntime::with_node_version("0.1.2", "a".repeat(64), "24.15.0")
            .expect("runtime");
        let data = DataGeneration::new("generation-001").expect("generation");
        let workspace = root.join("workspace");
        let deployment = ActiveDeployment::with_project_workspace(
            runtime.clone(),
            data.clone(),
            "2026-08-22T00:00:00Z".to_owned(),
            workspace.clone(),
        );
        std::fs::create_dir_all(layout.runtime_dir(&runtime).join("node-v24.15.0-win-x64"))
            .expect("node dir");
        std::fs::create_dir_all(
            layout
                .runtime_dir(&runtime)
                .join("app/node_modules/@deepseek-ai/dsh/lib"),
        )
        .expect("cli dir");
        std::fs::create_dir_all(layout.generation_dir(&data)).expect("generation dir");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(
            layout
                .runtime_dir(&runtime)
                .join("node-v24.15.0-win-x64/node.exe"),
            b"node",
        )
        .expect("node");
        std::fs::write(
            layout
                .runtime_dir(&runtime)
                .join("app/node_modules/@deepseek-ai/dsh/lib/bin.js"),
            b"cli",
        )
        .expect("cli");

        let spec = super::official_launch_spec(&layout, &deployment, 43127).expect("spec");

        assert_eq!(
            spec.program,
            layout
                .runtime_dir(&runtime)
                .join("node-v24.15.0-win-x64/node.exe")
                .canonicalize()
                .expect("canonical node")
        );
        assert_eq!(spec.cwd, workspace);
        let expected_home = layout.generation_dir(&data).to_string_lossy().into_owned();
        assert_eq!(
            spec.env.get("DSH_HOME").map(String::as_str),
            Some(expected_home.as_str())
        );
    }

    impl RuntimeLifecycle for RecordingRuntime {
        fn start(&self) -> Result<(), RuntimeError> {
            self.calls.lock().expect("调用记录锁不应中毒").push("start");
            Ok(())
        }

        fn stop(&self) -> Result<(), RuntimeError> {
            self.calls.lock().expect("调用记录锁不应中毒").push("stop");
            if self.fail_stop {
                Err(RuntimeError::Tauri("停止失败".to_owned()))
            } else {
                Ok(())
            }
        }

        fn start_and_wait_ready(&self, _deployment: &ActiveDeployment) -> Result<(), RuntimeError> {
            self.calls
                .lock()
                .expect("调用记录锁不应中毒")
                .push("start_exact");
            Ok(())
        }
    }

    #[test]
    fn stop_delegates_to_runtime_without_holding_controller_state() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime.clone());

        controller.stop().expect("运行时应停止");

        assert_eq!(
            *runtime.calls.lock().expect("调用记录锁不应中毒"),
            vec!["stop"]
        );
    }

    #[test]
    fn restart_stops_before_requesting_a_fresh_start() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime.clone());

        controller.restart().expect("运行时应重启");

        assert_eq!(
            *runtime.calls.lock().expect("调用记录锁不应中毒"),
            vec!["stop", "start"]
        );
    }

    #[test]
    fn successful_exit_request_stops_before_marking_exit_allowed() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime.clone());

        controller.request_exit().expect("停止成功后应允许退出");

        assert_eq!(
            *runtime.calls.lock().expect("调用记录锁不应中毒"),
            vec!["stop"]
        );
        assert!(controller.exit_requested());
    }

    #[test]
    fn failed_stop_does_not_mark_exit_allowed() {
        let runtime = Arc::new(RecordingRuntime {
            fail_stop: true,
            ..RecordingRuntime::default()
        });
        let controller = AppController::for_test(runtime.clone());

        assert!(controller.request_exit().is_err());

        assert_eq!(
            *runtime.calls.lock().expect("调用记录锁不应中毒"),
            vec!["stop"]
        );
        assert!(!controller.exit_requested());
    }

    #[test]
    fn initial_status_distinguishes_debug_mock_from_release_without_runtime() {
        assert_eq!(
            initial_runtime_status(true),
            RuntimeStatus {
                phase: AppPhase::Idle,
                message: "等待启动".to_owned(),
                url: None,
                elapsed_ms: None,
                error_code: None,
            }
        );
        assert_eq!(
            initial_runtime_status(false),
            RuntimeStatus {
                phase: AppPhase::Idle,
                message: "尚未安装兼容运行时".to_owned(),
                url: None,
                elapsed_ms: None,
                error_code: None,
            }
        );
    }

    #[test]
    fn probe_gate_allows_only_a_stopped_runtime_snapshot() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime);
        let lease = controller
            .runtime_stopped_for_probe()
            .expect("idle is stopped");
        drop(lease);

        controller.status.write().expect("status lock").phase = AppPhase::Ready;
        assert!(matches!(
            controller.runtime_stopped_for_probe(),
            Err(RuntimeError::ProbeRequiresStoppedRuntime)
        ));
    }

    #[test]
    fn successful_stop_changes_ready_snapshot_to_probe_safe_idle() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime);
        controller.status.write().expect("status lock").phase = AppPhase::Ready;

        controller.stop().expect("stop succeeds");

        assert_eq!(controller.status().phase, AppPhase::Idle);
        assert!(controller.runtime_stopped_for_probe().is_ok());
    }

    #[test]
    fn active_probe_lease_blocks_start_and_releases_on_drop() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime.clone());
        let lease = controller
            .runtime_stopped_for_probe()
            .expect("idle controller should issue one lease");

        assert!(matches!(
            controller.start_mock_runtime(),
            Err(RuntimeError::ProbeOperationInProgress)
        ));
        assert!(matches!(
            controller.runtime_stopped_for_probe(),
            Err(RuntimeError::ProbeOperationInProgress)
        ));
        assert!(runtime.calls.lock().expect("calls").is_empty());

        let retained_for_activation = lease.clone();
        drop(lease);
        assert!(matches!(
            controller.start_mock_runtime(),
            Err(RuntimeError::ProbeOperationInProgress)
        ));
        drop(retained_for_activation);
        controller
            .start_mock_runtime()
            .expect("dropping lease must reopen lifecycle operations");
        assert_eq!(*runtime.calls.lock().expect("calls"), vec!["start"]);
    }

    #[test]
    fn cloned_lease_allows_only_one_active_probe_permit() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime);
        let lease = controller.runtime_stopped_for_probe().expect("lease");
        let clone = lease.clone();
        let first = lease.claim_probe().expect("first probe claim");

        assert!(matches!(
            clone.claim_probe(),
            Err(RuntimeError::ProbeOperationInProgress)
        ));
        drop(first);
        clone.claim_probe().expect("claim is released on drop");
    }

    #[test]
    fn restart_cannot_stop_a_runtime_while_probe_lease_is_active() {
        let runtime = Arc::new(RecordingRuntime::default());
        let controller = AppController::for_test(runtime.clone());
        let _lease = controller.runtime_stopped_for_probe().expect("lease");

        assert!(matches!(
            controller.restart(),
            Err(RuntimeError::ProbeOperationInProgress)
        ));
        assert!(runtime.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn exit_request_holds_operation_gate_until_exit_flag_is_committed() {
        struct InspectingRuntime {
            gate: Arc<AtomicBool>,
        }
        impl RuntimeLifecycle for InspectingRuntime {
            fn start(&self) -> Result<(), RuntimeError> {
                Ok(())
            }
            fn stop(&self) -> Result<(), RuntimeError> {
                assert!(self.gate.load(Ordering::Acquire));
                Ok(())
            }
            fn start_and_wait_ready(
                &self,
                _deployment: &ActiveDeployment,
            ) -> Result<(), RuntimeError> {
                Ok(())
            }
        }
        let gate = Arc::new(AtomicBool::new(false));
        let controller = AppController {
            runtime: Arc::new(InspectingRuntime {
                gate: Arc::clone(&gate),
            }),
            status: Arc::new(RwLock::new(initial_runtime_status(true))),
            exit_requested: AtomicBool::new(false),
            operation_gate: Arc::clone(&gate),
            busy_provider: Arc::new(FixedBusyProvider(RuntimeBusyState::UnknownBusy)),
        };

        controller.request_exit().expect("exit");
        assert!(controller.exit_requested());
        assert!(!gate.load(Ordering::Acquire));
    }

    #[derive(Default)]
    struct FakeUi {
        emitted: Mutex<Vec<RuntimeEvent>>,
        navigated: Mutex<Vec<String>>,
    }

    impl RuntimeUi for FakeUi {
        fn emit_status(&self, event: &RuntimeEvent) -> Result<(), RuntimeError> {
            self.emitted
                .lock()
                .expect("事件记录锁不应中毒")
                .push(event.clone());
            Ok(())
        }

        fn navigate_main(&self, url: &tauri::Url) -> Result<(), RuntimeError> {
            self.navigated
                .lock()
                .expect("导航记录锁不应中毒")
                .push(url.as_str().to_owned());
            Ok(())
        }
    }

    fn sink(ui: Arc<FakeUi>) -> (ControllerEventSink, Arc<RwLock<RuntimeStatus>>) {
        let status = Arc::new(RwLock::new(RuntimeStatus::default()));
        (
            ControllerEventSink::new(
                Arc::clone(&status),
                ui,
                tauri::Url::parse("http://tauri.localhost/").expect("本地 URL 应合法"),
            ),
            status,
        )
    }

    #[test]
    fn ready_event_updates_snapshot_and_navigates_only_to_numeric_loopback() {
        let ui = Arc::new(FakeUi::default());
        let (sink, status) = sink(Arc::clone(&ui));

        sink.emit(RuntimeEvent::Ready {
            url: "http://127.0.0.1:43127".to_owned(),
            elapsed_ms: 820,
        })
        .expect("严格回环 URL 应可导航");

        assert_eq!(
            *status.read().expect("状态锁不应中毒"),
            RuntimeStatus {
                phase: AppPhase::Ready,
                message: "DSH 已就绪".to_owned(),
                url: Some("http://127.0.0.1:43127".to_owned()),
                elapsed_ms: Some(820),
                error_code: None,
            }
        );
        assert_eq!(
            *ui.navigated.lock().expect("导航记录锁不应中毒"),
            vec!["http://127.0.0.1:43127/".to_owned()]
        );
    }

    #[test]
    fn non_loopback_ready_url_is_rejected_without_navigation() {
        let ui = Arc::new(FakeUi::default());
        let (sink, _status) = sink(Arc::clone(&ui));

        let error = sink
            .emit(RuntimeEvent::Ready {
                url: "https://example.com/".to_owned(),
                elapsed_ms: 1,
            })
            .expect_err("远程 URL 必须拒绝");

        assert!(matches!(error, RuntimeError::InvalidUrl(_)));
        assert!(ui.navigated.lock().expect("导航记录锁不应中毒").is_empty());
    }

    #[test]
    fn failed_event_clears_ready_fields_and_returns_to_local_page() {
        let ui = Arc::new(FakeUi::default());
        let (sink, status) = sink(Arc::clone(&ui));
        *status.write().expect("状态锁不应中毒") = RuntimeStatus {
            phase: AppPhase::Ready,
            message: "旧状态".to_owned(),
            url: Some("http://127.0.0.1:1".to_owned()),
            elapsed_ms: Some(10),
            error_code: None,
        };

        sink.emit(RuntimeEvent::Failed {
            code: "health_timeout".to_owned(),
            message: "启动超时".to_owned(),
        })
        .expect("失败状态应可发回本地页");

        assert_eq!(
            *status.read().expect("状态锁不应中毒"),
            RuntimeStatus {
                phase: AppPhase::Failed,
                message: "启动超时".to_owned(),
                url: None,
                elapsed_ms: None,
                error_code: Some("health_timeout".to_owned()),
            }
        );
        assert_eq!(
            *ui.navigated.lock().expect("导航记录锁不应中毒"),
            vec!["http://tauri.localhost/".to_owned()]
        );
    }

    #[test]
    fn stopping_event_updates_snapshot_and_returns_to_local_page_before_runtime_stops() {
        let ui = Arc::new(FakeUi::default());
        let (sink, status) = sink(Arc::clone(&ui));
        *status.write().expect("状态锁不应中毒") = RuntimeStatus {
            phase: AppPhase::Ready,
            message: "DSH 已就绪".to_owned(),
            url: Some("http://127.0.0.1:43127".to_owned()),
            elapsed_ms: Some(820),
            error_code: None,
        };

        sink.emit(RuntimeEvent::Stopping {
            message: "正在停止 DSH".to_owned(),
        })
        .expect("停止事件应在旧服务退出前返回本地页");

        assert_eq!(
            *status.read().expect("状态锁不应中毒"),
            RuntimeStatus {
                phase: AppPhase::Stopping,
                message: "正在停止 DSH".to_owned(),
                url: None,
                elapsed_ms: None,
                error_code: None,
            }
        );
        assert_eq!(
            *ui.navigated.lock().expect("导航记录锁不应中毒"),
            vec!["http://tauri.localhost/".to_owned()]
        );
    }
}
