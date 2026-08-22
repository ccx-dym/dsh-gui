use dsh_desktop_lib::skin::{
    MaskTone, SkinFit, SkinPosition, SkinSettings, adapter_for, adapter_script, cleanup_script,
    page_script,
};
use semver::Version;
use std::process::Command;

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
    let verified = Version::parse("0.1.1-rc.1").expect("version");
    let official = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("url");
    assert!(page_script(&verified, &official, &fixture_settings()).contains("dsh-skin://"));

    for url in [
        "http://localhost:43127/chat",
        "https://127.0.0.1:43127/chat",
        "http://127.0.0.1/chat",
        "http://127.0.0.1:1420/chat?view=appearance",
    ] {
        let script = page_script(
            &verified,
            &tauri::Url::parse(url).expect("url"),
            &fixture_settings(),
        );
        assert_eq!(script, cleanup_script());
    }
    let unknown = Version::parse("0.1.2").expect("version");
    assert_eq!(
        page_script(&unknown, &official, &fixture_settings()),
        cleanup_script()
    );
}

#[test]
fn supports_only_the_exact_verified_dsh_version() {
    let adapter = adapter_for(&Version::parse("0.1.1-rc.1").expect("version"))
        .expect("精确验证版本应有适配器");
    assert_eq!(adapter.version(), "dsh-0.1.1-rc.1-v1");
    assert!(adapter_for(&Version::parse("0.1.1-rc.2").expect("version")).is_none());
    assert!(adapter_for(&Version::parse("0.1.2").expect("version")).is_none());
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
