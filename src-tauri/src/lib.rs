pub mod app_controller;
pub mod diagnostics;
pub mod domain;
pub mod paths;
pub mod runtime;
pub mod skin;
pub mod tray;
pub mod update;
pub mod update_ui;

use app_controller::{AppController, get_runtime_status, retry_runtime};
use diagnostics::{
    DiagnosticErrorKind, DiagnosticEvent, DiagnosticLogger, DiagnosticPolicy, DiagnosticSink,
    DiagnosticStage, FileDiagnosticSink, OperationTrace, TraceKind,
};
use paths::AppPaths;
use tauri::{AppHandle, Manager};
use tray::{CloseDecision, close_decision, setup_tray};
use update_ui::{
    UpdateUiController, check_updates, confirm_activation, get_update_state,
    install_compatible_update,
};

pub const UPDATE_COMMAND_NAMES: [&str; 4] = [
    "get_update_state",
    "check_updates",
    "install_compatible_update",
    "confirm_activation",
];

/// 更新命令的进程内纵深来源校验；ACL 仍是第一道强制边界。
///
/// :param value: 当前 WebView 的完整 URL。
/// :return: 仅内置 Tauri 启动页来源返回 true。
/// :raises: URL 解析失败时返回 false，不传播动态错误。
pub fn update_command_allowed_for_url(value: &str) -> bool {
    let Ok(url) = tauri::Url::parse(value) else {
        return false;
    };
    let bundled_origin = matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http", Some("tauri.localhost"))
    );
    #[cfg(debug_assertions)]
    let development_origin =
        url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port() == Some(1420);
    #[cfg(not(debug_assertions))]
    let development_origin = false;
    bundled_origin || development_origin
}

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
            // Windows toast activation 可能在没有可信 payload 的情况下重新启动已注册程序；
            // single-instance 因而只恢复固定本地更新窗口，不执行任何导航。
            if let Some(updates) = app.get_webview_window("updates") {
                let _ = updates.show();
                let _ = updates.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            get_runtime_status,
            retry_runtime,
            get_update_state,
            check_updates,
            install_compatible_update,
            confirm_activation
        ])
        .setup(|app| {
            let paths = AppPaths::resolve(app.handle())?;
            paths.ensure_exists()?;
            let logger = DiagnosticLogger::new(paths.logs.clone(), DiagnosticPolicy::default())?;
            let diagnostic_sink = FileDiagnosticSink::new(logger, 256)?;
            let update_controller = UpdateUiController::new(paths.clone());
            let controller = AppController::new(app.handle().clone())?;
            #[cfg(debug_assertions)]
            controller.start_mock_runtime()?;
            app.manage(diagnostic_sink);
            app.manage(update_controller);
            app.manage(controller);
            update_ui::spawn_scheduled_update_checks(app.handle().clone());
            #[cfg(not(debug_assertions))]
            {
                let app_handle = app.handle().clone();
                // release 没有其他自动启动入口；这个任务独占 update/lifecycle gate，因而
                // supervisor 的第一次 start 必然发生在 recover/pending activation 之后。
                tauri::async_runtime::spawn(async move {
                    let sink = app_handle.state::<FileDiagnosticSink>();
                    let diagnostics = diagnostics::DiagnosticContext::begin(
                        TraceKind::Update,
                        std::sync::Arc::new(sink.inner().clone()),
                    );
                    let controller = app_handle.state::<AppController>();
                    let updates = app_handle.state::<UpdateUiController>();
                    let _ =
                        update_ui::cold_bootstrap(&app_handle, &controller, &updates, &diagnostics)
                            .await;
                });
            }
            setup_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if !matches!(window.label(), "main" | "updates") {
                return;
            }
            let tauri::WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            if window.label() == "updates" {
                api.prevent_close();
                let _ = window.hide();
                return;
            }
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

    #[test]
    fn command_permissions_allow_only_local_startup_origin() {
        assert!(super::update_command_allowed_for_url(
            "tauri://localhost/index.html"
        ));
        assert!(super::update_command_allowed_for_url(
            "http://tauri.localhost/"
        ));
        assert!(super::update_command_allowed_for_url(
            "http://127.0.0.1:1420/"
        ));
        assert!(!super::update_command_allowed_for_url(
            "http://127.0.0.1:43127/"
        ));
        assert!(!super::update_command_allowed_for_url(
            "https://example.invalid/"
        ));
    }

    #[test]
    fn command_permissions_are_strictly_bounded() {
        assert_eq!(
            super::UPDATE_COMMAND_NAMES,
            [
                "get_update_state",
                "check_updates",
                "install_compatible_update",
                "confirm_activation",
            ]
        );
    }
}
