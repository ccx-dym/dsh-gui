use crate::skin::SkinAdapterController;
use serde::{Deserialize, Serialize};
use tauri::{State, WebviewWindow};

const CONTROL_DENIED: &str = "main_window_control_denied";
const CONTROL_FAILED: &str = "main_window_control_failed";

/// 自绘标题栏允许触发的封闭窗口动作。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MainWindowAction {
    StartDragging,
    Minimize,
    ToggleMaximize,
    Close,
}

/// 窗口动作完成后返回给标题栏的最小状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MainWindowControlState {
    pub maximized: bool,
}

trait WindowControl {
    fn start_dragging(&self) -> Result<(), ()>;
    fn minimize(&self) -> Result<(), ()>;
    fn is_maximized(&self) -> Result<bool, ()>;
    fn maximize(&self) -> Result<(), ()>;
    fn unmaximize(&self) -> Result<(), ()>;
    fn close(&self) -> Result<(), ()>;
}

impl WindowControl for WebviewWindow {
    fn start_dragging(&self) -> Result<(), ()> {
        WebviewWindow::start_dragging(self).map_err(|_| ())
    }

    fn minimize(&self) -> Result<(), ()> {
        WebviewWindow::minimize(self).map_err(|_| ())
    }

    fn is_maximized(&self) -> Result<bool, ()> {
        WebviewWindow::is_maximized(self).map_err(|_| ())
    }

    fn maximize(&self) -> Result<(), ()> {
        WebviewWindow::maximize(self).map_err(|_| ())
    }

    fn unmaximize(&self) -> Result<(), ()> {
        WebviewWindow::unmaximize(self).map_err(|_| ())
    }

    fn close(&self) -> Result<(), ()> {
        WebviewWindow::close(self).map_err(|_| ())
    }
}

fn execute_window_action(
    window: &impl WindowControl,
    action: MainWindowAction,
) -> Result<MainWindowControlState, &'static str> {
    match action {
        MainWindowAction::StartDragging => window.start_dragging(),
        MainWindowAction::Minimize => window.minimize(),
        MainWindowAction::ToggleMaximize => {
            if window.is_maximized().map_err(|_| CONTROL_FAILED)? {
                window.unmaximize()
            } else {
                window.maximize()
            }
        }
        MainWindowAction::Close => window.close(),
    }
    .map_err(|_| CONTROL_FAILED)?;

    Ok(MainWindowControlState {
        // 关闭动作可能已让平台句柄不可查询；响应只用于标题栏图标，失败时安全回退。
        maximized: window.is_maximized().unwrap_or(false),
    })
}

fn is_bundled_main_url(url: &tauri::Url) -> bool {
    let bundled = matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http", Some("tauri.localhost"))
    );
    #[cfg(debug_assertions)]
    let development =
        url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port() == Some(1420);
    #[cfg(not(debug_assertions))]
    let development = false;
    bundled || development
}

/// 判断调用页面是否可以控制当前主窗口。
///
/// :param label: Tauri 注入的当前窗口标签。
/// :param url: 当前 WebView 的完整 URL。
/// :param adapter: 原生侧绑定精确 DSH 来源的适配器状态。
/// :return: 仅主窗口的内置页面、开发页或当前绑定 DSH 来源返回 `true`。
/// :raises: 适配器状态锁异常时远程来源失败关闭为 `false`。
pub fn main_window_control_allowed(
    label: &str,
    url: &tauri::Url,
    adapter: &SkinAdapterController,
) -> bool {
    label == "main" && (is_bundled_main_url(url) || adapter.allows_page(url))
}

/// 执行主窗口自绘标题栏允许的一个封闭动作。
///
/// :param window: Tauri 注入的当前调用方 WebView 窗口。
/// :param adapter: 原生侧已绑定精确 DSH 来源的适配器状态。
/// :param action: 四种经过审核的窗口动作之一。
/// :return: 动作后的最大化状态；关闭动作可能在响应前隐藏窗口。
/// :raises str: 标签、来源或底层窗口调用失败时返回固定错误类别。
#[tauri::command]
pub fn control_main_window(
    window: WebviewWindow,
    adapter: State<'_, SkinAdapterController>,
    action: MainWindowAction,
) -> Result<MainWindowControlState, &'static str> {
    let url = window.url().map_err(|_| CONTROL_DENIED)?;
    if !main_window_control_allowed(window.label(), &url, adapter.inner()) {
        return Err(CONTROL_DENIED);
    }
    execute_window_action(&window, action)
}

#[cfg(test)]
mod tests {
    use super::{
        MainWindowAction, WindowControl, execute_window_action, main_window_control_allowed,
    };
    use crate::runtime::install_state::RuntimeSkinCompatibility;
    use crate::skin::SkinAdapterController;
    use semver::Version;
    use std::cell::Cell;

    #[derive(Default)]
    struct FakeWindow {
        dragging: Cell<bool>,
        minimized: Cell<bool>,
        maximized: Cell<bool>,
        closed: Cell<bool>,
    }

    impl WindowControl for FakeWindow {
        fn start_dragging(&self) -> Result<(), ()> {
            self.dragging.set(true);
            Ok(())
        }

        fn minimize(&self) -> Result<(), ()> {
            self.minimized.set(true);
            Ok(())
        }

        fn is_maximized(&self) -> Result<bool, ()> {
            Ok(self.maximized.get())
        }

        fn maximize(&self) -> Result<(), ()> {
            self.maximized.set(true);
            Ok(())
        }

        fn unmaximize(&self) -> Result<(), ()> {
            self.maximized.set(false);
            Ok(())
        }

        fn close(&self) -> Result<(), ()> {
            self.closed.set(true);
            Ok(())
        }
    }

    #[test]
    fn toggle_maximize_changes_real_window_state_in_both_directions() {
        let window = FakeWindow::default();

        let maximized =
            execute_window_action(&window, MainWindowAction::ToggleMaximize).expect("maximize");
        assert!(window.maximized.get());
        assert!(maximized.maximized);

        let restored =
            execute_window_action(&window, MainWindowAction::ToggleMaximize).expect("restore");
        assert!(!window.maximized.get());
        assert!(!restored.maximized);
    }

    #[test]
    fn close_uses_the_window_close_path_without_process_exit() {
        let window = FakeWindow::default();

        execute_window_action(&window, MainWindowAction::Close).expect("close");

        assert!(window.closed.get());
    }

    #[test]
    fn remote_control_requires_the_current_bound_numeric_origin() {
        let controller = SkinAdapterController::default();
        let official = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("official url");
        let other = tauri::Url::parse("http://127.0.0.1:43128/chat").expect("other url");
        assert!(controller.bind_navigation(
            &Version::parse("0.1.1-rc.2").expect("version"),
            RuntimeSkinCompatibility::Verified,
            &official,
        ));

        assert!(main_window_control_allowed("main", &official, &controller));
        assert!(!main_window_control_allowed("main", &other, &controller));
    }
}
