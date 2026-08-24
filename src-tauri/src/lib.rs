pub mod app_controller;
pub mod desktop_update;
pub mod diagnostics;
pub mod domain;
pub mod network_proxy;
pub mod paths;
pub mod runtime;
pub mod skin;
pub mod tray;
pub mod update;
pub mod update_ui;
pub mod window_chrome;

use app_controller::{AppController, get_runtime_status, retry_runtime};
use desktop_update::{
    DesktopUpdateService, check_desktop_update, get_desktop_update_state, install_desktop_update,
};
use diagnostics::{
    DiagnosticErrorKind, DiagnosticEvent, DiagnosticLogger, DiagnosticPolicy, DiagnosticSink,
    DiagnosticStage, FileDiagnosticSink, OperationTrace, TraceKind,
};
use paths::AppPaths;
use skin::{
    SkinAdapterController, SkinController, choose_skin_image, get_skin_state, report_skin_adapter,
    reset_skin_settings, save_skin_settings,
};
use tauri::{AppHandle, Manager};
use tray::{CloseDecision, close_decision_for, close_hide_failure_stage, setup_tray};
use update_ui::{
    UpdateUiController, check_updates, confirm_activation, get_update_state,
    install_compatible_update,
};
use window_chrome::control_main_window;

pub const UPDATE_COMMAND_NAMES: [&str; 7] = [
    "get_update_state",
    "check_updates",
    "install_compatible_update",
    "confirm_activation",
    "get_desktop_update_state",
    "check_desktop_update",
    "install_desktop_update",
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 自定义协议必须在 setup 创建 WebView 前注册；回调只读取固定设置与托管图片目录。
        .register_uri_scheme_protocol("dsh-skin", |context, request| {
            skin::protocol::handle_tauri_skin_request(
                context.app_handle(),
                context.webview_label(),
                &request.uri().to_string(),
            )
        })
        .invoke_handler(tauri::generate_handler![
            get_runtime_status,
            retry_runtime,
            get_update_state,
            check_updates,
            install_compatible_update,
            confirm_activation,
            get_desktop_update_state,
            check_desktop_update,
            install_desktop_update,
            get_skin_state,
            choose_skin_image,
            save_skin_settings,
            reset_skin_settings,
            report_skin_adapter,
            control_main_window
        ])
        .on_page_load(|webview, payload| {
            if webview.label() != "main" {
                return;
            }
            let app = webview.app_handle();
            let Some(adapter) = app.try_state::<SkinAdapterController>() else {
                return;
            };
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                record_skin_stage(app, DiagnosticStage::SkinPageStarted, None);
                adapter.navigation_started(payload.url());
                return;
            }
            record_skin_stage(app, DiagnosticStage::SkinPageFinished, None);
            let Some(skins) = app.try_state::<SkinController>() else {
                return;
            };
            if skin::adapter::apply_to_main(webview, payload.url(), &adapter, &skins).is_err() {
                record_app_diagnostic(
                    app,
                    DiagnosticStage::SkinApply,
                    DiagnosticErrorKind::TauriError,
                );
            }
        })
        .setup(|app| {
            let paths = AppPaths::resolve(app.handle())?;
            paths.ensure_exists()?;
            let logger = DiagnosticLogger::new(paths.logs.clone(), DiagnosticPolicy::default())?;
            let diagnostic_sink = FileDiagnosticSink::new(logger, 256)?;
            let update_controller = UpdateUiController::new(paths.clone());
            let desktop_update_service = DesktopUpdateService::new(
                paths.settings.clone(),
                app.package_info().version.clone(),
                app.handle().clone(),
            );
            let skin_previews = skin::protocol::SkinPreviewRegistry::new(paths.skins.clone());
            let skin_controller = SkinController::new(
                skin::SkinStore::new(paths.settings.clone(), paths.skins.clone()),
                paths.skins.clone(),
                skin_previews.clone(),
            );
            let controller = AppController::new(app.handle().clone())?;
            #[cfg(debug_assertions)]
            controller.start_mock_runtime()?;
            app.manage(diagnostic_sink);
            app.manage(update_controller);
            app.manage(desktop_update_service);
            app.manage(skin_previews);
            app.manage(skin_controller);
            app.manage(SkinAdapterController::default());
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
            if !matches!(window.label(), "main" | "updates" | "appearance") {
                return;
            }
            let tauri::WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            let controller = window.state::<AppController>();
            let decision = close_decision_for(window.label(), controller.exit_requested());
            if matches!(
                decision,
                CloseDecision::HideWindow | CloseDecision::HideToTray
            ) {
                // 辅助窗口与 main 都必须阻止默认关闭；隐藏失败只记录固定阶段和
                // 固定错误类别，避免把底层窗口错误正文写入诊断。
                api.prevent_close();
                if let Some(stage) = close_hide_failure_stage(decision, window.hide().is_err()) {
                    record_app_diagnostic(
                        window.app_handle(),
                        stage,
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

/// 记录不含动态错误正文的固定皮肤应用失败事件。
///
/// :param app: 当前 Tauri 应用句柄，用于访问受控诊断 sink。
/// :return: 无返回数据；记录操作不影响运行时和窗口状态。
/// :raises: sink 缺失或异步写入失败时静默失败，不向调用方传播错误。
pub(crate) fn record_skin_apply_diagnostic(app: &AppHandle) {
    record_app_diagnostic(
        app,
        DiagnosticStage::SkinApply,
        DiagnosticErrorKind::TauriError,
    );
}

/// 记录皮肤链路中的固定阶段，不包含 URL、路径或用户设置。
///
/// :param app: 当前应用句柄，用于访问固定诊断 sink。
/// :param stage: 来自封闭枚举的皮肤链路阶段。
/// :param error_kind: 可选的固定错误类别，不包含动态正文。
/// :return: 无返回数据。
/// :raises: sink 缺失或异步写入失败时静默失败。
pub(crate) fn record_skin_stage(
    app: &AppHandle,
    stage: DiagnosticStage,
    error_kind: Option<DiagnosticErrorKind>,
) {
    let Some(sink) = app.try_state::<FileDiagnosticSink>() else {
        return;
    };
    let trace = OperationTrace::begin(TraceKind::Runtime);
    sink.record(DiagnosticEvent::new(
        &trace,
        stage,
        0,
        0,
        Some(std::process::id()),
        error_kind,
    ));
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
                "get_desktop_update_state",
                "check_desktop_update",
                "install_desktop_update",
            ]
        );
    }
}
