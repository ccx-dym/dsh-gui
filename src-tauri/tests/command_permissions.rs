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

#[test]
fn appearance_window_is_local_hidden_and_owns_every_skin_mutation_command() {
    let config: Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri config 必须是 JSON");
    let appearance = config["app"]["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .find(|window| window["label"] == "appearance")
        .expect("必须存在独立外观设置窗口");
    assert_eq!(appearance["url"], "index.html?view=appearance");
    assert_eq!(appearance["visible"], false);
    assert_eq!(appearance["width"], 760);
    assert_eq!(appearance["height"], 720);
    assert_eq!(appearance["minWidth"], 680);
    assert_eq!(appearance["minHeight"], 620);

    let capability: Value = serde_json::from_str(include_str!("../capabilities/appearance.json"))
        .expect("appearance capability 必须是 JSON");
    assert_eq!(capability["local"], true);
    assert!(capability.get("remote").is_none());
    assert_eq!(capability["windows"], serde_json::json!(["appearance"]));
    let permissions = capability["permissions"]
        .as_array()
        .expect("permissions array");
    assert_eq!(permissions.len(), 6, "设置窗口只能获得事件与四个皮肤命令");
    for permission in [
        "core:event:allow-listen",
        "core:event:allow-unlisten",
        "allow-get-skin-state",
        "allow-choose-skin-image",
        "allow-save-skin-settings",
        "allow-reset-skin-settings",
    ] {
        assert!(permissions.iter().any(|value| value == permission));
    }
    assert!(!permissions.iter().any(|permission| {
        permission
            .as_str()
            .is_some_and(|name| name.starts_with("dialog:"))
    }));
    assert!(
        include_str!("../src/lib.rs").contains(".plugin(tauri_plugin_dialog::init())"),
        "Rust 原生选择器仍需要注册 dialog 插件"
    );

    let local_main: Value = serde_json::from_str(include_str!("../capabilities/default.json"))
        .expect("local-main capability 必须是 JSON");
    let main_permissions = local_main["permissions"]
        .as_array()
        .expect("permissions array");
    assert!(!main_permissions.iter().any(|permission| {
        permission
            .as_str()
            .is_some_and(|name| name.contains("skin") || name.starts_with("dialog:"))
    }));
}

#[test]
fn skin_resource_csp_allows_only_the_custom_scheme_and_windows_transport_origin() {
    let config: Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri config 必须是 JSON");
    let csp = config["app"]["security"]["csp"]
        .as_str()
        .expect("CSP string");
    let image_sources = csp
        .split(';')
        .map(str::trim)
        .find(|directive| directive.starts_with("img-src "))
        .expect("img-src directive");
    assert_eq!(
        image_sources,
        "img-src 'self' data: dsh-skin: http://dsh-skin.localhost"
    );
}

#[test]
fn async_skin_picker_uses_callback_delivery_instead_of_blocking_the_command_worker() {
    let controller = include_str!("../src/skin/controller.rs");
    assert!(!controller.contains("blocking_pick_file"));
    assert!(controller.contains(".pick_file("));
    assert!(controller.contains("oneshot::channel"));
}

#[test]
fn build_manifest_registers_every_exposed_skin_command() {
    let build_script = include_str!("../build.rs");
    for command in [
        "get_skin_state",
        "choose_skin_image",
        "save_skin_settings",
        "reset_skin_settings",
    ] {
        assert!(build_script.contains(&format!("\"{command}\"")));
    }
}

#[test]
fn official_main_receives_only_the_fail_closed_adapter_report_command() {
    let config: Value =
        serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri config 必须是 JSON");
    assert!(
        config["app"]["security"]["capabilities"]
            .as_array()
            .expect("capabilities array")
            .iter()
            .any(|value| value == "official-skin-report"),
        "远程报告 capability 必须显式启用"
    );
    let capability: Value = serde_json::from_str(include_str!("../capabilities/skin-report.json"))
        .expect("skin-report capability 必须是 JSON");
    assert_eq!(capability["local"], false);
    assert_eq!(capability["windows"], serde_json::json!(["main"]));
    assert_eq!(
        capability["remote"]["urls"],
        serde_json::json!(["http://127.0.0.1:*"])
    );
    assert_eq!(
        capability["permissions"],
        serde_json::json!(["allow-report-skin-adapter"])
    );

    let serialized = capability.to_string();
    for forbidden in [
        "runtime",
        "update",
        "dialog",
        "filesystem",
        "fs:",
        "shell",
        "event",
        "get-skin",
        "save-skin",
        "reset-skin",
        "choose-skin",
    ] {
        assert!(!serialized.contains(forbidden), "禁止权限: {forbidden}");
    }
}

#[test]
fn build_manifest_registers_only_the_adapter_report_command_for_official_main() {
    let build_script = include_str!("../build.rs");
    assert!(build_script.contains("\"report_skin_adapter\""));
}

#[test]
fn successful_skin_mutations_refresh_main_without_exposing_more_commands() {
    let controller = include_str!("../src/skin/controller.rs");
    assert!(controller.contains("refresh_main_skin(&app, &state.settings)"));
    assert!(
        controller
            .matches("refresh_main_skin(&app, &state.settings)")
            .count()
            >= 2
    );
    assert!(controller.contains("record_skin_apply_diagnostic(&app)"));
    let adapter = include_str!("../src/skin/adapter.rs");
    assert!(adapter.contains("crate::record_skin_apply_diagnostic(&app)"));
}
