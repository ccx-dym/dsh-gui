pub mod app_controller;
pub mod domain;
pub mod paths;
pub mod runtime;
pub mod tray;
pub mod update;

use app_controller::{AppController, get_runtime_status, retry_runtime};
use tauri::Manager;
use tray::{CloseDecision, close_decision, setup_tray};

/// 启动 DSH Desktop 的单一 Tauri 窗口。
///
/// # 返回
///
/// 应用正常退出时返回 `()`；Tauri 初始化或事件循环失败时终止进程并显示错误。
pub fn run() {
    tauri::Builder::default()
        // single-instance 必须最先注册，第二进程在 setup/DSH 启动前即被拦截。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let Some(window) = app.get_webview_window("main") else {
                eprintln!("第二实例唤醒失败: 缺少 main 窗口");
                return;
            };
            if let Err(error) = window.show() {
                eprintln!("第二实例恢复 main 窗口失败: {error}");
                return;
            }
            if let Err(error) = window.set_focus() {
                eprintln!("第二实例聚焦 main 窗口失败: {error}");
            }
        }))
        .invoke_handler(tauri::generate_handler![get_runtime_status, retry_runtime])
        .setup(|app| {
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
                if let Err(error) = window.hide() {
                    eprintln!("关闭到托盘时隐藏 main 窗口失败: {error}");
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("启动 DSH Desktop 失败");
}
