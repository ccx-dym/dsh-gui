use dsh_desktop_lib::runtime::install_state::RuntimeSkinCompatibility;
use dsh_desktop_lib::skin::{
    MaskTone, SkinDisableReason, SkinFit, SkinPosition, SkinSettings, adapter_for, adapter_script,
    cleanup_script, page_script, skin_runtime_policy,
};
use dsh_desktop_lib::{
    paths::{AppPaths, RuntimeLayout},
    runtime::install_state::{
        ActiveDeployment, DataGeneration, InstallStateStore, InstalledRuntime,
    },
};
use semver::Version;
use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture_settings() -> SkinSettings {
    SkinSettings {
        immersive: true,
        image_digest: Some("a".repeat(64)),
        fit: SkinFit::Cover,
        position: SkinPosition::Center,
        blur_px: 12,
        mask_tone: MaskTone::Light,
        mask_opacity_percent: 22,
        panel_opacity_percent: 88,
    }
}

#[test]
fn page_plan_requires_exact_version_and_numeric_loopback_origin() {
    let verified = Version::parse("0.1.1-rc.2").expect("version");
    let official = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("url");
    assert!(
        page_script(
            &verified,
            RuntimeSkinCompatibility::Verified,
            &official,
            &fixture_settings()
        )
        .contains("dsh-skin://")
    );
    assert_eq!(
        page_script(
            &verified,
            RuntimeSkinCompatibility::Unverified,
            &official,
            &fixture_settings()
        ),
        cleanup_script()
    );

    for url in [
        "http://localhost:43127/chat",
        "https://127.0.0.1:43127/chat",
        "http://127.0.0.1/chat",
        "http://127.0.0.1:1420/chat?view=appearance",
    ] {
        let script = page_script(
            &verified,
            RuntimeSkinCompatibility::Verified,
            &tauri::Url::parse(url).expect("url"),
            &fixture_settings(),
        );
        assert_eq!(script, cleanup_script());
    }
    let unknown = Version::parse("0.1.2").expect("version");
    assert_eq!(
        page_script(
            &unknown,
            RuntimeSkinCompatibility::Verified,
            &official,
            &fixture_settings()
        ),
        cleanup_script()
    );
}

#[test]
fn supports_only_the_exact_verified_dsh_version() {
    let adapter = adapter_for(&Version::parse("0.1.1-rc.2").expect("version"))
        .expect("精确验证版本应有适配器");
    assert_eq!(adapter.version(), "dsh-0.1.1-rc.2-v1");
    assert!(adapter_for(&Version::parse("0.1.1-rc.1").expect("version")).is_none());
    assert!(adapter_for(&Version::parse("0.1.2").expect("version")).is_none());
}

#[test]
fn runtime_policy_requires_both_signed_skin_compatibility_and_an_exact_adapter() {
    let rc1 = Version::parse("0.1.1-rc.1").unwrap();
    let rc2 = Version::parse("0.1.1-rc.2").unwrap();

    let verified = skin_runtime_policy(&rc2, RuntimeSkinCompatibility::Verified);
    assert!(verified.enabled);
    assert_eq!(verified.reason, None);

    for policy in [
        skin_runtime_policy(&rc1, RuntimeSkinCompatibility::Verified),
        skin_runtime_policy(&rc1, RuntimeSkinCompatibility::Unverified),
        skin_runtime_policy(&rc2, RuntimeSkinCompatibility::Unverified),
    ] {
        assert!(!policy.enabled);
        assert_eq!(policy.reason, Some(SkinDisableReason::VersionUnverified));
    }
}

#[test]
fn signed_unverified_exact_adapter_stays_disabled_after_restart() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("dsh-skin-runtime-{nonce}"));
    let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
    let layout = RuntimeLayout::from_paths(&paths);
    let runtime = InstalledRuntime::with_skin_compatibility(
        "0.1.1-rc.2",
        "a".repeat(64),
        "24.15.0",
        RuntimeSkinCompatibility::Unverified,
    )
    .expect("runtime");
    let deployment = ActiveDeployment::with_project_workspace(
        runtime,
        DataGeneration::new("generation-unverified").expect("generation"),
        "2026-08-23T00:00:00Z".to_owned(),
        std::env::current_dir()
            .expect("cwd")
            .canonicalize()
            .expect("canonical cwd"),
    );
    fs::create_dir_all(layout.runtime_dir(&deployment.runtime)).expect("runtime dir");
    fs::create_dir_all(layout.generation_dir(&deployment.data)).expect("generation dir");
    InstallStateStore::new(layout.clone())
        .save(&deployment)
        .expect("save deployment");

    let restarted = InstallStateStore::new(layout).load().expect("restart load");
    assert!(adapter_for(&restarted.runtime.version).is_some());
    assert!(
        !skin_runtime_policy(
            &restarted.runtime.version,
            restarted.runtime.skin_compatibility
        )
        .enabled
    );
}

#[test]
fn script_checks_dom_before_inserting_one_pointer_transparent_layer() {
    let script = adapter_script(&fixture_settings()).expect("script");
    assert!(script.contains("document.querySelector('#root')"));
    assert!(script.contains("--dsw-alias-bg-base"));
    assert!(script.contains("--dsw-alias-bg-layer-1"));
    assert!(script.contains("--dsw-alias-bg-layer-2"));
    assert!(script.contains("pointer-events:none"));
    assert!(script.contains("dsh-desktop-skin-background"));
    assert_eq!(script.matches("createElement('div')").count(), 1);
    assert!(!script.contains("fetch("));
    assert!(!script.contains("XMLHttpRequest"));
    assert!(!script.contains("localStorage"));
    assert!(!script.contains("sessionStorage"));
    assert!(!script.contains("setTimeout"));
    assert!(!script.contains("addEventListener('click'"));
}

#[test]
fn unsupported_or_disabled_state_generates_only_cleanup_script() {
    let script = cleanup_script();
    assert!(script.contains("dsh-desktop-skin-style"));
    assert!(script.contains("dsh-desktop-skin-background"));
    assert!(!script.contains("dsh-skin://"));

    let mut disabled = fixture_settings();
    disabled.immersive = false;
    assert_eq!(adapter_script(&disabled), None);
}

#[test]
fn cleanup_script_removes_existing_skin_nodes() {
    let harness = format!(
        r#"const removed=[];
global.document={{getElementById:(id)=>({{remove:()=>removed.push(id)}})}};
{}
console.log(JSON.stringify(removed));"#,
        cleanup_script()
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("前端工具链必须提供 node");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"["dsh-desktop-skin-style","dsh-desktop-skin-background"]"#
    );
}

#[test]
fn generated_values_are_closed_and_cannot_inject_script_text() {
    let script = adapter_script(&fixture_settings()).expect("script");
    assert!(script.contains("dsh-skin://localhost/aaaaaaaa"));
    assert!(!script.contains("</script>"));

    let mut invalid = fixture_settings();
    invalid.image_digest = Some("a');globalThis.pwned=true;//".to_owned());
    assert_eq!(adapter_script(&invalid), None);
}

#[test]
fn executable_script_cleans_and_reports_false_when_dom_inspection_throws() {
    let script = adapter_script(&fixture_settings()).expect("script");
    let harness = format!(
        r#"let reported=null;
global.document={{getElementById:()=>null,querySelector:()=>({{}}),documentElement:{{prepend:()=>{{}}}},head:{{append:()=>{{}}}}}};
global.getComputedStyle=()=>{{throw new Error('dom changed')}};
global.location={{protocol:'http:',hostname:'127.0.0.1',port:'43127'}};
global.__TAURI_INTERNALS__={{invoke:(_name,args)=>{{reported=args.compatible;return Promise.resolve();}}}};
{script}
setImmediate(()=>{{console.log(reported===false?'FAIL_CLOSED':'BAD')}});"#
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("前端工具链必须提供 node");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "FAIL_CLOSED"
    );
}
