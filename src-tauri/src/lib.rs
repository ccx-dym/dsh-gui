pub mod app_controller;
pub mod domain;
pub mod paths;
pub mod runtime;

use app_controller::{AppController, get_runtime_status, retry_runtime};
use tauri::Manager;

/// 启动 DSH Desktop 的单一 Tauri 窗口。
///
/// # 返回
///
/// 应用正常退出时返回 `()`；Tauri 初始化或事件循环失败时终止进程并显示错误。
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_runtime_status, retry_runtime])
        .setup(|app| {
            let controller = AppController::new(app.handle().clone())?;
            #[cfg(debug_assertions)]
            controller.start_mock_runtime()?;
            app.manage(controller);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动 DSH Desktop 失败");
}
