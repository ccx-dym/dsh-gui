fn main() {
    // Cargo 默认不会感知编译期发布通道变量；显式声明可避免切换通道后复用旧二进制。
    for key in [
        "DSH_DESKTOP_NPM_REGISTRY_ROOT",
        "DSH_DESKTOP_COMPAT_MANIFEST_URL",
        "DSH_DESKTOP_COMPAT_SIGNATURE_URL",
        "DSH_DESKTOP_COMPAT_PUBLIC_KEY",
        "DSH_DESKTOP_UPDATE_ENDPOINT",
        "DSH_DESKTOP_UPDATE_PUBLIC_KEY",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    const COMMANDS: &[&str] = &[
        "get_runtime_status",
        "retry_runtime",
        "get_update_state",
        "check_updates",
        "install_compatible_update",
        "confirm_activation",
        "get_desktop_update_state",
        "check_desktop_update",
        "install_desktop_update",
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
