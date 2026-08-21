/// 托盘菜单与鼠标事件可触发的类型化动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Open,
    Hide,
    Restart,
    Exit,
}

/// 主窗口关闭请求应采取的纯策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseDecision {
    HideToTray,
    AllowExit,
}

/// 托盘动作所需的运行时生命周期边界。
pub trait TrayController {
    fn restart(&self) -> Result<(), RuntimeError>;
    fn request_exit(&self) -> Result<(), RuntimeError>;
}

impl TrayController for AppController {
    fn restart(&self) -> Result<(), RuntimeError> {
        self.restart()
    }

    fn request_exit(&self) -> Result<(), RuntimeError> {
        self.request_exit()
    }
}

/// 托盘策略可使用的最小桌面 UI 边界。
pub trait DesktopUi {
    fn show_main(&self) -> Result<(), RuntimeError>;
    fn focus_main(&self) -> Result<(), RuntimeError>;
    fn hide_main(&self) -> Result<(), RuntimeError>;
    fn exit(&self, code: i32);
}

struct TauriDesktopUi {
    app: AppHandle,
}

impl TauriDesktopUi {
    fn main_window(&self) -> Result<tauri::WebviewWindow, RuntimeError> {
        self.app
            .get_webview_window("main")
            .ok_or(RuntimeError::MainWindowMissing)
    }
}

impl DesktopUi for TauriDesktopUi {
    fn show_main(&self) -> Result<(), RuntimeError> {
        self.main_window()?
            .show()
            .map_err(|error| RuntimeError::Tauri(error.to_string()))
    }

    fn focus_main(&self) -> Result<(), RuntimeError> {
        self.main_window()?
            .set_focus()
            .map_err(|error| RuntimeError::Tauri(error.to_string()))
    }

    fn hide_main(&self) -> Result<(), RuntimeError> {
        self.main_window()?
            .hide()
            .map_err(|error| RuntimeError::Tauri(error.to_string()))
    }

    fn exit(&self, code: i32) {
        self.app.exit(code);
    }
}

/// 根据显式退出状态决定隐藏窗口还是允许关闭。
///
/// :param exit_requested: 仅在托盘 Exit 已成功停止运行时后为 true。
/// :return: 无退出请求时隐藏到托盘，否则允许退出。
/// :raises: 此纯函数不产生错误。
pub fn close_decision(exit_requested: bool) -> CloseDecision {
    if exit_requested {
        CloseDecision::AllowExit
    } else {
        CloseDecision::HideToTray
    }
}

/// 把稳定菜单 ID 映射为类型化动作，不依赖本地化显示文字。
///
/// :param id: Tauri 菜单事件中的稳定标识符。
/// :return: 已知 ID 的动作；未知 ID 返回 None。
/// :raises: 此纯映射不产生错误。
pub fn tray_action_from_id(id: &str) -> Option<TrayAction> {
    match id {
        "open" => Some(TrayAction::Open),
        "hide" => Some(TrayAction::Hide),
        "restart" => Some(TrayAction::Restart),
        "exit" => Some(TrayAction::Exit),
        _ => None,
    }
}

/// 执行一个类型化托盘动作。
///
/// :param action: 已由稳定菜单 ID 或左键事件解析的动作。
/// :param controller: 运行时重启与显式退出边界。
/// :param ui: 主窗口和应用退出边界。
/// :return: 动作完整执行时返回 `Ok(())`。
/// :raises RuntimeError: 窗口操作、停止或重启失败时返回。
pub fn handle_tray_action(
    action: TrayAction,
    controller: &dyn TrayController,
    ui: &dyn DesktopUi,
) -> Result<(), RuntimeError> {
    match action {
        TrayAction::Open => {
            ui.show_main()?;
            ui.focus_main()
        }
        TrayAction::Hide => ui.hide_main(),
        TrayAction::Restart => controller.restart(),
        TrayAction::Exit => {
            // app.exit 必须位于停止成功与退出标志提交之后。
            controller.request_exit()?;
            ui.exit(0);
            Ok(())
        }
    }
}

/// 创建使用稳定 ID 的系统托盘菜单并注册事件。
///
/// :param app: 已创建 main 窗口与 `AppController` 状态的 Tauri 应用。
/// :return: 托盘图标成功注册时返回 `Ok(())`。
/// :raises RuntimeError: 菜单、图标或系统托盘创建失败时返回。
pub fn setup_tray(app: &AppHandle) -> Result<(), RuntimeError> {
    let menu = MenuBuilder::new(app)
        .text("open", "打开 DSH Desktop")
        .text("hide", "隐藏")
        .text("restart", "重启 DSH")
        .text("exit", "退出")
        .build()
        .map_err(|error| RuntimeError::Tauri(error.to_string()))?;
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| RuntimeError::Tauri("缺少默认窗口图标".to_owned()))?;

    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let Some(action) = tray_action_from_id(event.id().as_ref()) else {
                return;
            };
            let controller = app.state::<AppController>();
            let ui = TauriDesktopUi { app: app.clone() };
            if let Err(error) = handle_tray_action(action, controller.inner(), &ui) {
                eprintln!("托盘动作 {action:?} 失败: {error}");
            }
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                let app = tray.app_handle();
                let controller = app.state::<AppController>();
                let ui = TauriDesktopUi { app: app.clone() };
                if let Err(error) = handle_tray_action(TrayAction::Open, controller.inner(), &ui) {
                    eprintln!("托盘左键恢复主窗口失败: {error}");
                }
            }
        })
        .build(app)
        .map_err(|error| RuntimeError::Tauri(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CloseDecision, DesktopUi, TrayAction, TrayController, close_decision, handle_tray_action,
        tray_action_from_id,
    };
    use crate::runtime::RuntimeError;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CallLog(Arc<Mutex<Vec<&'static str>>>);

    impl CallLog {
        fn push(&self, value: &'static str) {
            self.0.lock().expect("调用记录锁不应中毒").push(value);
        }

        fn values(&self) -> Vec<&'static str> {
            self.0.lock().expect("调用记录锁不应中毒").clone()
        }
    }

    struct RecordingController {
        calls: CallLog,
        fail_exit: bool,
    }

    impl TrayController for RecordingController {
        fn restart(&self) -> Result<(), RuntimeError> {
            self.calls.push("restart");
            Ok(())
        }

        fn request_exit(&self) -> Result<(), RuntimeError> {
            self.calls.push("stop");
            if self.fail_exit {
                Err(RuntimeError::Tauri("停止失败".to_owned()))
            } else {
                self.calls.push("mark_exit");
                Ok(())
            }
        }
    }

    struct RecordingUi {
        calls: CallLog,
    }

    impl DesktopUi for RecordingUi {
        fn show_main(&self) -> Result<(), RuntimeError> {
            self.calls.push("show");
            Ok(())
        }

        fn focus_main(&self) -> Result<(), RuntimeError> {
            self.calls.push("focus");
            Ok(())
        }

        fn hide_main(&self) -> Result<(), RuntimeError> {
            self.calls.push("hide");
            Ok(())
        }

        fn exit(&self, _code: i32) {
            self.calls.push("exit");
        }
    }

    #[test]
    fn window_close_hides_instead_of_exiting_without_an_explicit_exit_request() {
        assert_eq!(close_decision(false), CloseDecision::HideToTray);
    }

    #[test]
    fn window_close_is_allowed_only_after_an_explicit_exit_request() {
        assert_eq!(close_decision(true), CloseDecision::AllowExit);
    }

    #[test]
    fn fixed_menu_ids_map_to_typed_actions_independent_of_labels() {
        assert_eq!(tray_action_from_id("open"), Some(TrayAction::Open));
        assert_eq!(tray_action_from_id("hide"), Some(TrayAction::Hide));
        assert_eq!(tray_action_from_id("restart"), Some(TrayAction::Restart));
        assert_eq!(tray_action_from_id("exit"), Some(TrayAction::Exit));
        assert_eq!(tray_action_from_id("退出"), None);
    }

    #[test]
    fn open_action_restores_then_focuses_the_main_window() {
        let calls = CallLog::default();
        let controller = RecordingController {
            calls: calls.clone(),
            fail_exit: false,
        };
        let ui = RecordingUi {
            calls: calls.clone(),
        };

        handle_tray_action(TrayAction::Open, &controller, &ui).expect("主窗口应恢复");

        assert_eq!(calls.values(), vec!["show", "focus"]);
    }

    #[test]
    fn restart_action_delegates_without_touching_window_visibility() {
        let calls = CallLog::default();
        let controller = RecordingController {
            calls: calls.clone(),
            fail_exit: false,
        };
        let ui = RecordingUi {
            calls: calls.clone(),
        };

        handle_tray_action(TrayAction::Restart, &controller, &ui).expect("运行时应重启");

        assert_eq!(calls.values(), vec!["restart"]);
    }

    #[test]
    fn hide_action_hides_the_main_window() {
        let calls = CallLog::default();
        let controller = RecordingController {
            calls: calls.clone(),
            fail_exit: false,
        };
        let ui = RecordingUi {
            calls: calls.clone(),
        };

        handle_tray_action(TrayAction::Hide, &controller, &ui).expect("主窗口应隐藏");

        assert_eq!(calls.values(), vec!["hide"]);
    }

    #[test]
    fn exit_action_exits_only_after_stop_and_exit_flag_succeed() {
        let calls = CallLog::default();
        let controller = RecordingController {
            calls: calls.clone(),
            fail_exit: false,
        };
        let ui = RecordingUi {
            calls: calls.clone(),
        };

        handle_tray_action(TrayAction::Exit, &controller, &ui).expect("应完成显式退出");

        assert_eq!(calls.values(), vec!["stop", "mark_exit", "exit"]);
    }

    #[test]
    fn exit_action_does_not_exit_when_runtime_stop_fails() {
        let calls = CallLog::default();
        let controller = RecordingController {
            calls: calls.clone(),
            fail_exit: true,
        };
        let ui = RecordingUi {
            calls: calls.clone(),
        };

        assert!(handle_tray_action(TrayAction::Exit, &controller, &ui).is_err());

        assert_eq!(calls.values(), vec!["stop"]);
    }
}
use crate::app_controller::AppController;
use crate::runtime::RuntimeError;
use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
