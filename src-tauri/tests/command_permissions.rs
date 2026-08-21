use serde_json::Value;

#[test]
fn command_permissions_expose_updates_only_to_local_main() {
    let capability: Value = serde_json::from_str(include_str!("../capabilities/default.json"))
        .expect("capability 必须是 JSON");
    assert_eq!(capability["local"], true);
    assert!(capability.get("remote").is_none());
    assert_eq!(
        capability["windows"],
        serde_json::json!(["main", "updates"])
    );
    let permissions = capability["permissions"]
        .as_array()
        .expect("permissions array");
    for command in [
        "allow-get-update-state",
        "allow-check-updates",
        "allow-install-compatible-update",
        "allow-confirm-activation",
    ] {
        assert!(permissions.iter().any(|value| value == command));
    }
    assert!(!permissions.iter().any(|value| value == "core:default"));
    assert!(!permissions.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|name| name.starts_with("fs:") || name.starts_with("shell:"))
    }));
}

#[test]
fn build_manifest_registers_every_exposed_update_command() {
    let build_script = include_str!("../build.rs");
    for command in [
        "get_update_state",
        "check_updates",
        "install_compatible_update",
        "confirm_activation",
    ] {
        assert!(build_script.contains(&format!("\"{command}\"")));
    }
    assert!(build_script.contains("AppManifest::new().commands(COMMANDS)"));
}

#[test]
fn update_center_is_an_independent_hidden_local_window() {
    let config: Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri config 必须是 JSON");
    let updates = config["app"]["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .find(|window| window["label"] == "updates")
        .expect("必须存在独立更新窗口");
    assert_eq!(updates["url"], "index.html");
    assert_eq!(updates["visible"], false);
}
