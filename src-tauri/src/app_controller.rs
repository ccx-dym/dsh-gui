use crate::domain::RuntimeStatus;
#[cfg(any(debug_assertions, test))]
use crate::domain::{AppPhase, RuntimeEvent};
#[cfg(debug_assertions)]
use crate::paths::AppPaths;
use crate::runtime::RuntimeError;
#[cfg(any(debug_assertions, test))]
use crate::runtime::RuntimeEventSink;
#[cfg(debug_assertions)]
use crate::runtime::RuntimeSupervisor;
#[cfg(debug_assertions)]
use crate::runtime::command::{RuntimeLaunchSpec, reserve_loopback_port};
#[cfg(debug_assertions)]
use std::env;
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
#[cfg(debug_assertions)]
use std::time::Duration;
#[cfg(debug_assertions)]
use tauri::Emitter;
use tauri::{AppHandle, Manager};

#[cfg(debug_assertions)]
const MOCK_READY_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(debug_assertions)]
const RUNTIME_STOP_GRACE: Duration = Duration::from_secs(2);

trait RuntimeLifecycle: Send + Sync + 'static {
    fn start(&self) -> Result<(), RuntimeError>;
    fn stop(&self) -> Result<(), RuntimeError>;
}

#[cfg(any(debug_assertions, test))]
trait RuntimeUi: Send + Sync + 'static {
    fn emit_status(&self, event: &RuntimeEvent) -> Result<(), RuntimeError>;
    fn navigate_main(&self, url: &tauri::Url) -> Result<(), RuntimeError>;
}

#[cfg(debug_assertions)]
struct TauriRuntimeUi {
    app: AppHandle,
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
}

#[cfg(not(debug_assertions))]
struct UnavailableRuntime;

#[cfg(not(debug_assertions))]
impl RuntimeLifecycle for UnavailableRuntime {
    fn start(&self) -> Result<(), RuntimeError> {
        Err(RuntimeError::MockRuntimeDisabled)
    }

    fn stop(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[cfg(debug_assertions)]
impl RuntimeUi for TauriRuntimeUi {
    fn emit_status(&self, event: &RuntimeEvent) -> Result<(), RuntimeError> {
        self.app
            .emit("runtime-status", event)
            .map_err(|error| RuntimeError::Tauri(error.to_string()))
    }

    fn navigate_main(&self, url: &tauri::Url) -> Result<(), RuntimeError> {
        self.app
            .get_webview_window("main")
            .ok_or(RuntimeError::MainWindowMissing)?
            .navigate(url.clone())
            .map_err(|error| RuntimeError::Tauri(error.to_string()))
    }
}

#[cfg(any(debug_assertions, test))]
struct ControllerEventSink {
    status: Arc<RwLock<RuntimeStatus>>,
    ui: Arc<dyn RuntimeUi>,
    local_url: tauri::Url,
}

#[cfg(any(debug_assertions, test))]
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

#[cfg(any(debug_assertions, test))]
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

#[cfg(any(debug_assertions, test))]
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

#[cfg(any(debug_assertions, test))]
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
}

/// 独占一次已停止 runtime 的更新/探活窗口。
///
/// 值只能由 `AppController` 在原子确认 Idle/Failed 后签发；离开作用域时自动
/// 释放门禁，避免调用方伪造“已停止”快照或在异步探活中途启动第二个 DSH。
#[derive(Clone, Debug)]
pub struct ProbeLease {
    _guard: Arc<OperationGuard>,
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
            _guard: Arc::new(OperationGuard { gate }),
        }
    }
}

impl AppController {
    /// 创建连接单一主窗口的应用控制器。
    ///
    /// :param app: Tauri 应用句柄，用于解析目录、发事件和导航主窗口。
    /// :return: 保存当前本地启动页 URL 的控制器。
    /// :raises RuntimeError: `main` 窗口不存在或无法读取本地 URL 时返回。
    pub fn new(app: AppHandle) -> Result<Self, RuntimeError> {
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
            let _ = (app, local_url);
            Arc::new(UnavailableRuntime)
        };
        Ok(Self {
            runtime,
            status,
            exit_requested: AtomicBool::new(false),
            operation_gate: Arc::new(AtomicBool::new(false)),
        })
    }

    #[cfg(test)]
    fn for_test(runtime: Arc<dyn RuntimeLifecycle>) -> Self {
        Self {
            runtime,
            status: Arc::new(RwLock::new(initial_runtime_status(true))),
            exit_requested: AtomicBool::new(false),
            operation_gate: Arc::new(AtomicBool::new(false)),
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
                _guard: Arc::new(guard),
            }),
            crate::domain::AppPhase::Starting
            | crate::domain::AppPhase::Ready
            | crate::domain::AppPhase::Stopping => Err(RuntimeError::ProbeRequiresStoppedRuntime),
        }
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
    controller.retry().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AppController, ControllerEventSink, RuntimeLifecycle, RuntimeUi, initial_runtime_status,
    };
    use crate::domain::{AppPhase, RuntimeEvent, RuntimeStatus};
    use crate::runtime::{RuntimeError, RuntimeEventSink};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, RwLock};

    #[derive(Default)]
    struct RecordingRuntime {
        calls: Mutex<Vec<&'static str>>,
        fail_stop: bool,
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
        }
        let gate = Arc::new(AtomicBool::new(false));
        let controller = AppController {
            runtime: Arc::new(InspectingRuntime {
                gate: Arc::clone(&gate),
            }),
            status: Arc::new(RwLock::new(initial_runtime_status(true))),
            exit_requested: AtomicBool::new(false),
            operation_gate: Arc::clone(&gate),
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
