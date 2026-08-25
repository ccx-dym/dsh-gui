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
        glass_blur_px: 0,
        mask_tone: MaskTone::Light,
        mask_opacity_percent: 22,
        panel_opacity_percent: 88,
        conversation_surface_opacity_percent: 85,
    }
}

fn execute_style_text(settings: &SkinSettings) -> String {
    let script = adapter_script(settings).expect("沉浸皮肤应生成脚本");
    let harness = format!(
        r#"const inserted=[];
const root={{}};
global.requestAnimationFrame=(callback)=>{{callback();return 1;}};
global.document={{
  getElementById:()=>null,
  querySelector:()=>root,
  createElement:(tag)=>({{id:'',style:{{cssText:''}},setAttribute:()=>{{}},textContent:'',tag}}),
  documentElement:{{prepend:(node)=>inserted.push(node)}},
  head:{{append:(node)=>inserted.push(node)}}
}};
global.getComputedStyle=()=>({{getPropertyValue:()=> '#151517'}});
global.location={{protocol:'http:',hostname:'127.0.0.1',port:'43127'}};
global.__TAURI_INTERNALS__={{invoke:()=>Promise.resolve()}};
{script}
const style=inserted.find((node)=>node.tag==='style');
console.log(style?.textContent??'NO_STYLE');"#
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("前端工具链必须提供 node");
    assert!(
        output.status.success(),
        "Node harness 执行失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("CSS 输出必须是 UTF-8")
        .trim()
        .to_owned()
}

#[test]
fn page_plan_requires_exact_version_and_numeric_loopback_origin() {
    let verified = Version::parse("0.1.1-rc.2").expect("version");
    let official = tauri::Url::parse("http://127.0.0.1:43127/chat").expect("url");
    let supported = page_script(
        &verified,
        RuntimeSkinCompatibility::Verified,
        &official,
        &fixture_settings(),
    );
    assert!(supported.contains("http://dsh-skin.localhost/"));
    assert!(!supported.contains("dsh-skin://localhost/"));
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
fn script_checks_dom_before_painting_the_page_canvas() {
    let script = adapter_script(&fixture_settings()).expect("script");
    assert!(script.contains("document.querySelector('#root')"));
    assert!(script.contains("--dsw-alias-bg-base"));
    assert!(script.contains("--dsw-alias-bg-layer-1"));
    assert!(script.contains("--dsw-alias-bg-layer-2"));
    assert!(script.contains("body{background-color:rgb(255,255,255) !important"));
    assert!(script.contains("background-image:none !important"));
    assert!(script.contains("body::before{content:\"\""));
    assert!(script.contains("body::after{content:\"\""));
    assert!(script.contains("filter:blur(12px)"));
    assert!(script.contains(":root,#root{--dsw-alias-bg-base:"));
    assert!(script.contains("--dsw-alias-bg-base:transparent !important"));
    assert!(script.contains("--dsw-specific-sidebar-fill:transparent !important"));
    assert!(script.contains(
        "[data-composer-card]{position:relative;isolation:isolate;overflow:visible !important;background:rgba(255,255,255,0.85)"
    ));
    assert!(!script.contains("--dsw-specific-input-major:transparent"));
    assert!(script.contains("--dsw-alias-bg-layer-1:rgba(255,255,255,0.88) !important"));
    assert!(script.contains("--dsw-alias-bg-layer-2:rgba(255,255,255,0.88) !important"));
    assert!(script.contains("dsh-desktop-skin-background"));
    assert_eq!(script.matches("createElement('div')").count(), 1);
    assert!(!script.contains("fetch("));
    assert!(!script.contains("XMLHttpRequest"));
    assert!(!script.contains("localStorage"));
    assert!(!script.contains("sessionStorage"));
    assert!(!script.contains("setTimeout"));
    assert!(script.contains("requestAnimationFrame"));
    assert!(!script.contains("addEventListener('click'"));
}

#[test]
fn executable_script_waits_for_the_dsh_theme_contract_before_applying() {
    let script = adapter_script(&fixture_settings()).expect("script");
    let harness = format!(
        r#"let frame=null;
let ready=false;
let reported=null;
const inserted=[];
const root={{}};
global.requestAnimationFrame=(callback)=>{{frame=callback;return 1;}};
global.document={{
  getElementById:()=>null,
  querySelector:()=>ready?root:null,
  createElement:(tag)=>({{id:'',style:{{cssText:''}},setAttribute:()=>{{}},textContent:'',tag}}),
  documentElement:{{prepend:(node)=>inserted.push(node)}},
  head:{{append:(node)=>inserted.push(node)}}
}};
global.getComputedStyle=()=>({{getPropertyValue:()=>ready?'#151517':''}});
global.location={{protocol:'http:',hostname:'127.0.0.1',port:'43127'}};
global.__TAURI_INTERNALS__={{invoke:(_name,args)=>{{reported=args.compatible;return Promise.resolve();}}}};
{script}
if(reported!==null||typeof frame!=='function'){{throw new Error('reported before DSH was ready')}}
ready=true;
frame();
setImmediate(()=>{{console.log(reported===true&&inserted.length===2?'RETRIED':'BAD')}});"#
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("前端工具链必须提供 node");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "RETRIED");
}

#[test]
fn executable_script_applies_opacity_only_to_the_wallpaper_layer() {
    let mut settings = fixture_settings();
    settings.blur_px = 0;
    settings.panel_opacity_percent = 37;
    let script = adapter_script(&settings).expect("script");
    let harness = format!(
        r#"const inserted=[];
const root={{}};
global.requestAnimationFrame=(callback)=>{{callback();return 1;}};
global.document={{
  getElementById:()=>null,
  querySelector:()=>root,
  createElement:(tag)=>({{id:'',style:{{cssText:''}},setAttribute:()=>{{}},textContent:'',tag}}),
  documentElement:{{prepend:(node)=>inserted.push(node)}},
  head:{{append:(node)=>inserted.push(node)}}
}};
global.getComputedStyle=()=>({{getPropertyValue:()=> '#151517'}});
global.location={{protocol:'http:',hostname:'127.0.0.1',port:'43127'}};
global.__TAURI_INTERNALS__={{invoke:()=>Promise.resolve()}};
{script}
const style=inserted.find((node)=>node.tag==='style');
console.log(style?.textContent??'NO_STYLE');"#
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("前端工具链必须提供 node");
    assert!(output.status.success());
    let css = String::from_utf8_lossy(&output.stdout);

    assert!(css.contains("body::before{content:\"\""));
    assert!(css.contains("opacity:0.37"));
    assert!(!css.contains("--dsw-alias-bg-layer-1:rgba(255,255,255,0.37)"));
    assert!(!css.contains("--dsw-alias-bg-layer-2:rgba(255,255,255,0.37)"));
}

#[test]
fn composer_and_user_messages_share_opacity_but_keep_independent_shapes() {
    let css = execute_style_text(&fixture_settings());

    assert!(css.contains(
        "[data-composer-card]{position:relative;isolation:isolate;overflow:visible !important;background:rgba(255,255,255,0.85)"
    ));
    assert!(css.contains("border-radius:22px !important"));
    assert!(css.contains(
        "[data-chat-flow-kind=\"user\"] [data-slot=\"conversation.message.images\"]+div{position:relative;isolation:isolate;overflow:visible !important;background:rgba(255,255,255,0.85)"
    ));
    assert!(css.contains("border-radius:18px 18px 6px 18px !important"));
    assert!(css.contains("0 18px 48px rgba(0,0,0,0.18)"));
    assert!(css.contains("0 10px 28px rgba(0,0,0,0.14)"));
}

#[test]
fn botanical_baroque_decorations_use_fixed_noninteractive_svg_layers() {
    let css = execute_style_text(&fixture_settings());

    assert!(css.contains("[data-composer-card]::before{content:\"\";position:absolute"));
    assert!(css.contains(
        "[data-chat-flow-kind=\"user\"] [data-slot=\"conversation.message.images\"]+div::before{content:\"\";position:absolute"
    ));
    assert_eq!(css.matches("data:image/svg+xml").count(), 6);
    assert_eq!(css.matches("pointer-events:none").count(), 4);
    assert!(css.contains("background-repeat:no-repeat"));
    assert!(css.contains("rgba(255,211,151,0.72)"));
    assert!(!css.contains("<script"));
    assert!(!css.contains("javascript:"));
}

#[test]
fn narrow_windows_hide_secondary_composer_ornaments_without_removing_corners() {
    let css = execute_style_text(&fixture_settings());

    assert!(css.contains(
        "@media(max-width:900px){[data-composer-card]::before{background-size:72px 64px,0 0,72px 64px,0 0}}"
    ));
    assert_eq!(css.matches("data:image/svg+xml").count(), 6);
}

#[test]
fn fixed_conversation_ornaments_do_not_depend_on_glass_blur() {
    for radius in [0, 16] {
        let mut settings = fixture_settings();
        settings.glass_blur_px = radius;
        let css = execute_style_text(&settings);

        assert!(css.contains("[data-composer-card]::before"));
        assert!(css.contains(
            "[data-chat-flow-kind=\"user\"] [data-slot=\"conversation.message.images\"]+div::before"
        ));
        assert_eq!(css.matches("data:image/svg+xml").count(), 6);
    }
}

#[test]
fn zero_glass_blur_keeps_one_wallpaper_without_backdrop_filter() {
    let mut settings = fixture_settings();
    settings.glass_blur_px = 0;
    let css = execute_style_text(&settings);

    assert_eq!(css.matches("http://dsh-skin.localhost/").count(), 1);
    assert!(!css.contains("backdrop-filter"));
    assert!(!css.contains(".pI_x6G_centerCol{"));
    assert!(css.contains("[data-chat-flow-kind=\"user\"]"));
    assert!(css.contains("--dsw-alias-bg-layer-1:rgba(255,255,255,0.88) !important"));
    assert!(css.contains("[data-composer-card]::before"));
    assert_eq!(css.matches("data:image/svg+xml").count(), 6);
}

#[test]
fn positive_glass_blur_uses_one_radius_for_every_verified_surface() {
    for radius in [1, 16, 32] {
        let mut settings = fixture_settings();
        settings.glass_blur_px = radius;
        let css = execute_style_text(&settings);

        for selector in [
            ".pI_x6G_centerCol",
            ".pI_x6G_sidebarCol",
            ".pI_x6G_detailsCol",
            "[data-composer-card]",
            "#dsh-desktop-titlebar",
        ] {
            assert!(css.contains(selector), "缺少选择器 {selector}");
        }
        let filter = format!("blur({radius}px) saturate(1.28)");
        assert_eq!(css.matches(&filter).count(), 2);
        assert_eq!(css.matches("http://dsh-skin.localhost/").count(), 1);
    }
}

#[test]
fn positive_glass_blur_keeps_fixed_overlays_out_of_clipped_layout_containers() {
    let mut settings = fixture_settings();
    settings.glass_blur_px = 16;
    let css = execute_style_text(&settings);
    let filter = "blur(16px) saturate(1.28)";

    // DSH 的布局列使用 overflow:hidden；滤镜必须放在其背后的全窗口层，
    // 否则浏览器会把列变为 fixed 子元素的包含块并挤压设置抽屉。
    let glass_layer =
        format!("body::after{{backdrop-filter:{filter};-webkit-backdrop-filter:{filter}}}");
    assert!(css.contains(&glass_layer));
    assert_eq!(css.matches("backdrop-filter:").count(), 2);
    assert!(!css.contains(
        ".pI_x6G_centerCol,.pI_x6G_sidebarCol,.pI_x6G_detailsCol,[data-composer-card],#dsh-desktop-titlebar{backdrop-filter"
    ));
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
    assert!(!cleanup_script().contains("dsh-desktop-titlebar"));
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
    assert!(script.contains("http://dsh-skin.localhost/aaaaaaaa"));
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
