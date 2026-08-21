use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use dsh_desktop_lib::update::manifest::{ManifestError, ManifestVerifier};
use ed25519_dalek::SigningKey;
use semver::Version;

const TEST_SEED: [u8; 32] = [7; 32];
const TEST_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIAcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcH\n-----END PRIVATE KEY-----\n";

fn canonical_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[test]
fn runtime_release_fixture_signs_raw_bytes_and_rejects_tampering() {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri 必须位于仓库根目录下")
        .to_path_buf();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间应晚于 Unix epoch")
        .as_nanos();
    let fixture_root =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("runtime-sign-{nonce}"));
    fs::create_dir_all(&fixture_root).expect("应能创建 target 内测试目录");
    let manifest_path =
        repository_root.join("src-tauri/tests/fixtures/runtime-manifest/valid.json");
    let signature_path = fixture_root.join("manifest.sig");
    let key_path = fixture_root.join("test-only-ed25519.pem");
    fs::write(&key_path, TEST_PRIVATE_PEM).expect("应能写入明确仅用于测试的临时 PEM");

    let output = Command::new("node")
        .arg(repository_root.join("scripts/sign-runtime.mjs"))
        .arg(&manifest_path)
        .arg(&signature_path)
        .env("DSH_RUNTIME_SIGNING_KEY_FILE", &key_path)
        .output()
        .expect("Node signer 应可启动");
    assert!(output.status.success(), "Node signer 应成功");
    let signature = fs::read_to_string(signature_path).expect("应生成 detached signature");

    let public_key = SigningKey::from_bytes(&TEST_SEED).verifying_key();
    let verifier = ManifestVerifier::new(
        &canonical_hex(public_key.as_bytes()),
        Version::parse("0.1.0").unwrap(),
    )
    .unwrap();
    let manifest = fs::read(&manifest_path).expect("应能读取清单原始 bytes");
    verifier
        .verify(&manifest, &signature)
        .expect("原始 bytes 应通过 Rust 验证");

    let mut tampered = manifest;
    tampered[0] ^= 1;
    assert!(matches!(
        verifier.verify(&tampered, &signature),
        Err(ManifestError::SignatureVerification)
    ));
}
