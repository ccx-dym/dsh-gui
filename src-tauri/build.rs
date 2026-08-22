fn main() {
    const COMMANDS: &[&str] = &[
        "get_runtime_status",
        "retry_runtime",
        "get_update_state",
        "check_updates",
        "install_compatible_update",
        "confirm_activation",
        "get_skin_state",
        "choose_skin_image",
        "save_skin_settings",
        "reset_skin_settings",
        "report_skin_adapter",
    ];
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));
    tauri_build::try_build(attributes).expect("Tauri 构建配置必须有效");
}
