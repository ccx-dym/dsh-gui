use dsh_desktop_lib::skin::SkinAdapterController;
use dsh_desktop_lib::window_chrome::{MainWindowAction, main_window_control_allowed};

#[test]
fn main_window_action_schema_accepts_only_the_four_reviewed_actions() {
    for (json, expected) in [
        (r#""start_dragging""#, MainWindowAction::StartDragging),
        (r#""minimize""#, MainWindowAction::Minimize),
        (r#""toggle_maximize""#, MainWindowAction::ToggleMaximize),
        (r#""close""#, MainWindowAction::Close),
    ] {
        let parsed: MainWindowAction = serde_json::from_str(json).expect("reviewed action");
        assert_eq!(parsed, expected);
    }

    assert!(serde_json::from_str::<MainWindowAction>(r#""maximize""#).is_err());
    assert!(serde_json::from_str::<MainWindowAction>(r#""exit""#).is_err());
}

#[test]
fn window_control_rejects_auxiliary_and_unbound_remote_pages() {
    let controller = SkinAdapterController::default();
    let unbound = tauri::Url::parse("http://127.0.0.1:43128/chat").expect("unbound url");

    assert!(!main_window_control_allowed(
        "updates",
        &unbound,
        &controller
    ));
    assert!(!main_window_control_allowed("main", &unbound, &controller));
    assert!(!main_window_control_allowed(
        "main",
        &tauri::Url::parse("https://example.invalid/").expect("remote url"),
        &controller,
    ));
}

#[test]
fn bundled_main_origin_remains_controllable_before_runtime_navigation() {
    let controller = SkinAdapterController::default();
    for url in ["tauri://localhost/", "http://tauri.localhost/"] {
        assert!(main_window_control_allowed(
            "main",
            &tauri::Url::parse(url).expect("bundled url"),
            &controller,
        ));
    }
}

#[cfg(debug_assertions)]
#[test]
fn vite_main_origin_remains_controllable_in_debug_builds() {
    assert!(main_window_control_allowed(
        "main",
        &tauri::Url::parse("http://127.0.0.1:1420/").expect("vite url"),
        &SkinAdapterController::default(),
    ));
}
