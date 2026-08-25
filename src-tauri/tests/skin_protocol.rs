use dsh_desktop_lib::skin::{
    MaskTone, SkinDraft, SkinFit, SkinPosition, SkinProtocol, SkinStore, skin_resource_url,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const IMAGE_BYTES: &[u8] = b"protocol-fixture-png-bytes";

struct ProtocolFixture {
    protocol: SkinProtocol,
    digest: String,
    image_path: PathBuf,
}

fn unique_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "dsh-desktop-skin-protocol-{name}-{}-{nonce}",
        std::process::id()
    ))
}

fn fixture_protocol(name: &str) -> ProtocolFixture {
    let root = unique_root(name);
    let settings_root = root.join("settings");
    let skins_root = root.join("skins");
    fs::create_dir_all(&settings_root).expect("settings root");
    fs::create_dir_all(&skins_root).expect("skins root");
    let digest = format!("{:x}", Sha256::digest(IMAGE_BYTES));
    let image_path = skins_root.join(format!("{digest}.png"));
    fs::write(&image_path, IMAGE_BYTES).expect("seed managed image");
    let store = SkinStore::new(settings_root, skins_root.clone());
    store
        .save(
            0,
            SkinDraft {
                immersive: true,
                image_digest: Some(digest.clone()),
                fit: SkinFit::Cover,
                position: SkinPosition::Center,
                blur_px: 0,
                glass_blur_px: 0,
                mask_tone: MaskTone::Light,
                mask_opacity_percent: 22,
                panel_opacity_percent: 88,
                conversation_surface_opacity_percent: 85,
            },
        )
        .expect("register active image");
    ProtocolFixture {
        protocol: SkinProtocol::new(store, skins_root),
        digest,
        image_path,
    }
}

#[test]
fn serves_only_the_registered_digest_with_fixed_headers_and_body() {
    let fixture = fixture_protocol("registered");
    let url = skin_resource_url(&fixture.digest).expect("canonical URL");

    let response = fixture.protocol.request(&url);

    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), IMAGE_BYTES);
    assert_eq!(response.headers()["Content-Type"], "image/png");
    assert_eq!(
        response.headers()["Cache-Control"],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(
        response.headers()["ETag"],
        format!("\"{}\"", fixture.digest)
    );
    assert_eq!(response.headers().len(), 3);
}

#[test]
fn rejects_every_noncanonical_uri_without_reflecting_request_text() {
    let fixture = fixture_protocol("deny");
    let other_digest = "f".repeat(64);
    let denied = [
        format!("dsh-skin://localhost/../{}", fixture.digest),
        format!("dsh-skin://localhost/%2e%2e/{}", fixture.digest),
        format!("dsh-skin://localhost/{}?path=C%3A%5Csecret", fixture.digest),
        format!("dsh-skin://localhost/{}#fragment", fixture.digest),
        format!("dsh-skin://user@localhost/{}", fixture.digest),
        format!("dsh-skin://localhost:80/{}", fixture.digest),
        format!("dsh-skin://evil/{}", fixture.digest),
        format!("http://dsh-skin.localhost/{}", fixture.digest),
        format!("dsh-skin://localhost/{}/extra", fixture.digest),
        format!("dsh-skin://localhost/{}", fixture.digest.to_uppercase()),
        format!("dsh-skin://localhost/{other_digest}"),
    ];

    for uri in denied {
        let response = fixture.protocol.request(&uri);
        assert_eq!(response.status(), 404, "URI 应被拒绝: {uri}");
        assert_eq!(response.body(), b"not found");
        assert!(!String::from_utf8_lossy(response.body()).contains(&uri));
    }
}

#[test]
fn corrupt_registered_content_returns_fixed_internal_error_without_bytes() {
    let fixture = fixture_protocol("digest-mismatch");
    fs::write(&fixture.image_path, b"changed-after-registration").expect("mutate fixture");

    let response = fixture
        .protocol
        .request(&skin_resource_url(&fixture.digest).expect("canonical registered resource URL"));

    assert_eq!(response.status(), 500);
    assert_eq!(response.body(), b"internal error");
    assert_eq!(response.headers().len(), 0);
}

#[test]
fn rejects_multilink_registered_file_before_returning_content() {
    let fixture = fixture_protocol("hardlink");
    fs::hard_link(
        &fixture.image_path,
        fixture.image_path.with_extension("alias"),
    )
    .expect("create fixture hard link");

    let response = fixture
        .protocol
        .request(&skin_resource_url(&fixture.digest).expect("canonical registered resource URL"));

    assert_eq!(response.status(), 500);
    assert_eq!(response.body(), b"internal error");
}

#[test]
fn rejects_registered_file_larger_than_the_protocol_read_bound() {
    let root = unique_root("bounded-read");
    let settings_root = root.join("settings");
    let skins_root = root.join("skins");
    fs::create_dir_all(&settings_root).expect("settings root");
    fs::create_dir_all(&skins_root).expect("skins root");
    let bytes = vec![7_u8; 20 * 1024 * 1024 + 1];
    let digest = format!("{:x}", Sha256::digest(&bytes));
    fs::write(skins_root.join(format!("{digest}.png")), bytes).expect("oversize fixture");
    let store = SkinStore::new(settings_root, skins_root.clone());
    store
        .save(
            0,
            SkinDraft {
                immersive: true,
                image_digest: Some(digest.clone()),
                fit: SkinFit::Cover,
                position: SkinPosition::Center,
                blur_px: 0,
                glass_blur_px: 0,
                mask_tone: MaskTone::Light,
                mask_opacity_percent: 22,
                panel_opacity_percent: 88,
                conversation_surface_opacity_percent: 85,
            },
        )
        .expect("register oversize fixture");

    let response = SkinProtocol::new(store, skins_root)
        .request(&skin_resource_url(&digest).expect("canonical URL"));

    assert_eq!(response.status(), 500);
    assert_eq!(response.body(), b"internal error");
}

#[cfg(windows)]
#[test]
fn rejects_reparse_registered_file_when_windows_can_create_fixture() {
    use std::os::windows::fs::symlink_file;

    let root = unique_root("reparse");
    let settings_root = root.join("settings");
    let skins_root = root.join("skins");
    fs::create_dir_all(&settings_root).expect("settings root");
    fs::create_dir_all(&skins_root).expect("skins root");
    let digest = format!("{:x}", Sha256::digest(IMAGE_BYTES));
    let external = root.join("outside.png");
    fs::write(&external, IMAGE_BYTES).expect("external fixture");
    if symlink_file(&external, skins_root.join(format!("{digest}.png"))).is_err() {
        // 未启用 Windows 开发者模式时普通用户可能无法创建文件符号链接；
        // 硬链接测试仍强制覆盖多入口身份，导入测试另有 junction reparse 强制门禁。
        return;
    }
    let settings = format!(
        concat!(
            "{{\"schema\":1,\"revision\":1,\"settings\":{{",
            "\"immersive\":true,\"image_digest\":\"{}\",",
            "\"fit\":\"cover\",\"position\":\"center\",\"blur_px\":0,",
            "\"mask_tone\":\"light\",\"mask_opacity_percent\":22,",
            "\"panel_opacity_percent\":88}}}}"
        ),
        digest
    );
    fs::write(settings_root.join("skin.json"), settings).expect("settings fixture");
    let protocol = SkinProtocol::new(
        SkinStore::new(settings_root, skins_root.clone()),
        skins_root,
    );

    let response = protocol.request(&skin_resource_url(&digest).expect("canonical URL"));

    assert_eq!(response.status(), 500);
    assert_eq!(response.body(), b"internal error");
}

#[test]
fn resource_url_accepts_only_canonical_lowercase_digest() {
    assert_eq!(
        skin_resource_url(&"a".repeat(64)),
        Some(format!("dsh-skin://localhost/{}", "a".repeat(64)))
    );
    assert_eq!(skin_resource_url(&"A".repeat(64)), None);
    assert_eq!(skin_resource_url("../settings/skin.json"), None);
}
