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
use std::sync::{Arc, RwLock};
#[cfg(debug_assertions)]
use std::time::Duration;
#[cfg(debug_assertions)]
use tauri::Emitter;
use tauri::{AppHandle, Manager};

#[cfg(debug_assertions)]
const MOCK_READY_TIMEOUT: Duration = Duration::from_secs(10);

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
            RuntimeEvent::Failed { .. } => self.ui.navigate_main(&self.local_url),
            RuntimeEvent::Starting { .. } | RuntimeEvent::Stopping { .. } => Ok(()),
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

/// 协调运行时状态、后台生命周期任务与单一 Tauri 主窗口。
pub struct AppController {
    #[cfg(debug_assertions)]
    supervisor: Arc<RuntimeSupervisor>,
    status: Arc<RwLock<RuntimeStatus>>,
    #[cfg(debug_assertions)]
    app: AppHandle,
    #[cfg(debug_assertions)]
    local_url: tauri::Url,
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
        #[cfg(not(debug_assertions))]
        let _ = (app, local_url);
        Ok(Self {
            #[cfg(debug_assertions)]
            supervisor: Arc::new(RuntimeSupervisor::new()),
            status: Arc::new(RwLock::new(RuntimeStatus::default())),
            #[cfg(debug_assertions)]
            app,
            #[cfg(debug_assertions)]
            local_url,
        })
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
    #[cfg(debug_assertions)]
    pub fn start_mock_runtime(&self) -> Result<(), RuntimeError> {
        let paths =
            AppPaths::resolve(&self.app).map_err(|error| RuntimeError::Tauri(error.to_string()))?;
        paths
            .ensure_exists()
            .map_err(|error| RuntimeError::Tauri(error.to_string()))?;
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

    #[cfg(not(debug_assertions))]
    pub fn start_mock_runtime(&self) -> Result<(), RuntimeError> {
        Err(RuntimeError::MockRuntimeDisabled)
    }

    /// 失败后重新构造端口与模拟启动参数并提交后台启动。
    ///
    /// :return: 新启动任务成功提交时返回 `Ok(())`。
    /// :raises RuntimeError: 与 `start_mock_runtime` 相同。
    pub fn retry(&self) -> Result<(), RuntimeError> {
        self.start_mock_runtime()
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
    use super::{ControllerEventSink, RuntimeUi};
    use crate::domain::{AppPhase, RuntimeEvent, RuntimeStatus};
    use crate::runtime::{RuntimeError, RuntimeEventSink};
    use std::sync::{Arc, Mutex, RwLock};

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
}
