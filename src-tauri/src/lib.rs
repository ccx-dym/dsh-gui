pub mod app_controller;
pub mod diagnostics;
pub mod domain;
pub mod paths;
pub mod runtime;
pub mod tray;
pub mod update;

use app_controller::{AppController, get_runtime_status, retry_runtime};
use diagnostics::{
    DiagnosticErrorKind, DiagnosticEvent, DiagnosticLogger, DiagnosticPolicy, DiagnosticSink,
    DiagnosticStage, FileDiagnosticSink, OperationTrace, TraceKind,
};
use paths::AppPaths;
use tauri::{AppHandle, Manager};
use tray::{CloseDecision, close_decision, setup_tray};

/// 启动 DSH Desktop 的单一 Tauri 窗口。
///
/// # 返回
///
/// 应用正常退出时返回 `()`；Tauri 初始化或事件循环失败时终止进程并显示错误。
pub fn run() {
    let result = tauri::Builder::default()
        // single-instance 必须最先注册，第二进程在 setup/DSH 启动前即被拦截。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let Some(window) = app.get_webview_window("main") else {
                record_app_diagnostic(
                    app,
                    DiagnosticStage::SingleInstanceWindow,
                    DiagnosticErrorKind::MainWindowMissing,
                );
                return;
            };
            if window.show().is_err() {
                record_app_diagnostic(
                    app,
                    DiagnosticStage::SingleInstanceShow,
                    DiagnosticErrorKind::TauriError,
                );
                return;
            }
            if window.set_focus().is_err() {
                record_app_diagnostic(
                    app,
                    DiagnosticStage::SingleInstanceFocus,
                    DiagnosticErrorKind::TauriError,
                );
            }
        }))
        .invoke_handler(tauri::generate_handler![get_runtime_status, retry_runtime])
        .setup(|app| {
            let paths = AppPaths::resolve(app.handle())?;
            paths.ensure_exists()?;
            let logger = DiagnosticLogger::new(paths.logs.clone(), DiagnosticPolicy::default())?;
            app.manage(FileDiagnosticSink::new(logger, 256)?);
            let controller = AppController::new(app.handle().clone())?;
            #[cfg(debug_assertions)]
            controller.start_mock_runtime()?;
            app.manage(controller);
            setup_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            let tauri::WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            let controller = window.state::<AppController>();
            if close_decision(controller.exit_requested()) == CloseDecision::HideToTray {
                // 即使隐藏失败也阻止默认关闭，避免 DSH 在没有显式 Exit 的情况下被回收。
                api.prevent_close();
                if window.hide().is_err() {
                    record_app_diagnostic(
                        window.app_handle(),
                        DiagnosticStage::CloseToTray,
                        DiagnosticErrorKind::TauriError,
                    );
                }
            }
        })
        .run(tauri::generate_context!());
    if tauri_run_exit_code(result) != 0 {
        // 不格式化 Tauri setup/run 错误；其中可能带用户路径或 WebView URL。
        std::process::exit(1);
    }
}

fn tauri_run_exit_code<T, E>(result: Result<T, E>) -> i32 {
    if result.is_ok() { 0 } else { 1 }
}

/// 将桌面壳固定错误类别异步写入本地日志；写入失败不会反向影响 UI 状态机。
fn record_app_diagnostic(app: &AppHandle, stage: DiagnosticStage, error_kind: DiagnosticErrorKind) {
    let Some(sink) = app.try_state::<FileDiagnosticSink>() else {
        return;
    };
    let trace = OperationTrace::begin(TraceKind::Runtime);
    let event = DiagnosticEvent::new(
        &trace,
        stage,
        0,
        0,
        Some(std::process::id()),
        Some(error_kind),
    );
    sink.record(event);
}

#[cfg(test)]
mod tests {
    use super::tauri_run_exit_code;
    use std::fmt;
    use std::sync::atomic::{AtomicBool, Ordering};

    static FORMATTED: AtomicBool = AtomicBool::new(false);

    struct PoisonError;

    impl fmt::Debug for PoisonError {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            FORMATTED.store(true, Ordering::Release);
            Err(fmt::Error)
        }
    }

    #[test]
    fn tauri_failure_exit_code_does_not_format_dynamic_error_source() {
        FORMATTED.store(false, Ordering::Release);

        assert_eq!(tauri_run_exit_code(Err::<(), _>(PoisonError)), 1);
        assert!(!FORMATTED.load(Ordering::Acquire));
    }
}
