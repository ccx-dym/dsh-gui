use dsh_desktop_lib::{
    diagnostics::{DiagnosticContext, TraceKind},
    update::manifest::{ManifestError, ManifestVerifier},
};
use ed25519_dalek::{Signer, SigningKey};
use semver::Version;

const TEST_SIGNING_KEY: [u8; 32] = [7; 32];
const VALID_MANIFEST: &[u8] = include_bytes!("fixtures/runtime-manifest/valid.json");

fn canonical_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn verifier() -> ManifestVerifier {
    verifier_for("0.1.0")
}

fn diagnostics() -> DiagnosticContext {
    DiagnosticContext::noop(TraceKind::Update)
}

fn verifier_for(current_desktop_version: &str) -> ManifestVerifier {
    let public_key = SigningKey::from_bytes(&TEST_SIGNING_KEY).verifying_key();
    ManifestVerifier::new(
        &canonical_hex(public_key.as_bytes()),
        Version::parse(current_desktop_version).expect("测试桌面版本应有效"),
    )
    .expect("测试公钥应有效")
}

fn sign(bytes: &[u8]) -> String {
    canonical_hex(
        &SigningKey::from_bytes(&TEST_SIGNING_KEY)
            .sign(bytes)
            .to_bytes(),
    )
}

fn signed_json(
    json: String,
) -> Result<dsh_desktop_lib::update::manifest::VerifiedManifest, ManifestError> {
    verifier().verify(json.as_bytes(), &sign(json.as_bytes()), &diagnostics())
}

fn valid_json() -> String {
    String::from_utf8(VALID_MANIFEST.to_vec()).expect("夹具必须是 UTF-8")
}

fn replace_once(from: &str, to: &str) -> String {
    valid_json().replacen(from, to, 1)
}

#[test]
fn valid_signed_manifest_returns_typed_fields_and_raw_digest() {
    let verified = verifier()
        .verify(VALID_MANIFEST, &sign(VALID_MANIFEST), &diagnostics())
        .expect("规范且签名正确的清单应通过");

    assert_eq!(verified.manifest.schema, 1);
    assert_eq!(
        verified.manifest.dsh_version,
        Version::parse("0.1.1-rc.1").unwrap()
    );
    assert_eq!(
        verified.manifest.node_version,
        Version::parse("24.15.0").unwrap()
    );
    assert_eq!(
        verified.manifest.minimum_desktop_version,
        Version::parse("0.1.0").unwrap()
    );
    assert_eq!(verified.manifest.platform, "windows");
    assert_eq!(verified.manifest.arch, "x86_64");
    assert_eq!(verified.manifest.artifact.size, 1_048_576);
    assert_eq!(verified.manifest.artifact.url.scheme(), "https");
    assert_eq!(
        verified.manifest.artifact.sha256,
        [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xab, 0xcd, 0xef,
        ]
    );
    assert_eq!(verified.manifest.verified_at, "2026-08-21T09:30:00Z");
    assert_eq!(
        verified.manifest.compatibility_summary,
        "已验证 Windows 10/11 x64 桌面运行与隔离数据升级。"
    );
    assert_eq!(
        verified.manifest_digest,
        "0986f8c636d4224f01e80b0bdb7437f4d6d31dd253531858db9c35d01b67196b"
    );
}

#[test]
fn signature_is_checked_against_exact_raw_bytes_before_parsing() {
    let signature = sign(VALID_MANIFEST);
    let mut tampered = VALID_MANIFEST.to_vec();
    let index = tampered.iter().position(|byte| *byte == b'w').unwrap();
    tampered[index] = b'W';

    assert!(matches!(
        verifier().verify(&tampered, &signature, &diagnostics()),
        Err(ManifestError::SignatureVerification)
    ));
}

#[test]
fn strict_verification_rejects_weak_key_and_small_order_signature_points() {
    // 压缩单位点是有效编码但属于小阶点；普通验证方程可能接受 R=identity、s=0。
    let mut identity = [0_u8; 32];
    identity[0] = 1;
    let weak_verifier =
        ManifestVerifier::new(&canonical_hex(&identity), Version::parse("0.1.0").unwrap())
            .expect("弱点仍是可解析的 Ed25519 公钥编码");
    let mut weak_signature = [0_u8; 64];
    weak_signature[0] = 1;

    assert!(matches!(
        weak_verifier.verify(
            VALID_MANIFEST,
            &canonical_hex(&weak_signature),
            &diagnostics()
        ),
        Err(ManifestError::SignatureVerification)
    ));
}

#[test]
fn wrong_key_and_malformed_key_or_signature_are_rejected_without_echoing_secrets() {
    let wrong_key = SigningKey::from_bytes(&[8; 32]).verifying_key();
    let wrong_verifier = ManifestVerifier::new(
        &canonical_hex(wrong_key.as_bytes()),
        Version::parse("0.1.0").unwrap(),
    )
    .unwrap();
    assert!(matches!(
        wrong_verifier.verify(VALID_MANIFEST, &sign(VALID_MANIFEST), &diagnostics()),
        Err(ManifestError::SignatureVerification)
    ));

    for invalid_key in ["00".repeat(31), "AA".repeat(32), "gg".repeat(32)] {
        let error = ManifestVerifier::new(&invalid_key, Version::parse("0.1.0").unwrap())
            .expect_err("非规范公钥必须失败");
        assert!(matches!(error, ManifestError::InvalidPublicKeyEncoding));
        assert!(!error.to_string().contains(&invalid_key));
    }
    for invalid_signature in ["00".repeat(63), "AA".repeat(64), "gg".repeat(64)] {
        let error = verifier()
            .verify(VALID_MANIFEST, &invalid_signature, &diagnostics())
            .expect_err("非规范签名必须失败");
        assert!(matches!(error, ManifestError::InvalidSignatureEncoding));
        assert!(!error.to_string().contains(&invalid_signature));
    }
}

#[test]
fn parsing_happens_only_after_signature_and_denies_unknown_fields() {
    let unsigned_invalid = br#"{"schema":1,"secret":"DO_NOT_ECHO"}"#;
    let error = verifier()
        .verify(unsigned_invalid, &"00".repeat(64), &diagnostics())
        .expect_err("签名错误必须先于 JSON 错误");
    assert!(matches!(error, ManifestError::SignatureVerification));

    let unknown = replace_once("\"schema\":1", "\"schema\":1,\"secret\":\"DO_NOT_ECHO\"");
    let error = signed_json(unknown).expect_err("未知字段必须失败");
    assert!(matches!(error, ManifestError::InvalidJson));
    assert!(!error.to_string().contains("DO_NOT_ECHO"));
}

#[test]
fn schema_platform_arch_and_size_are_strict() {
    for (json, expected_field) in [
        (
            replace_once("\"platform\":\"windows\"", "\"platform\":\"linux\""),
            "platform",
        ),
        (
            replace_once("\"arch\":\"x86_64\"", "\"arch\":\"aarch64\""),
            "arch",
        ),
        (
            replace_once("\"size\":1048576", "\"size\":0"),
            "artifact.size",
        ),
    ] {
        assert!(matches!(
            signed_json(json),
            Err(ManifestError::InvalidField { field }) if field == expected_field
        ));
    }

    assert!(matches!(
        signed_json(replace_once("\"schema\":1", "\"schema\":2")),
        Err(ManifestError::UnsupportedSchema { schema: 2 })
    ));
}

#[test]
fn versions_must_be_strict_semver() {
    for (field, valid, invalid) in [
        ("dsh_version", "0.1.1-rc.1", "v0.1.1"),
        ("node_version", "24.15.0", "24.15"),
        ("minimum_desktop_version", "0.1.0", " 0.1.0"),
    ] {
        let json = replace_once(
            &format!("\"{field}\":\"{valid}\""),
            &format!("\"{field}\":\"{invalid}\""),
        );
        assert!(matches!(
            signed_json(json),
            Err(ManifestError::InvalidField { field: actual }) if actual == field
        ));
    }
}

#[test]
fn minimum_desktop_version_rejects_older_stable_and_prerelease_clients() {
    let future = replace_once(
        "\"minimum_desktop_version\":\"0.1.0\"",
        "\"minimum_desktop_version\":\"99.0.0\"",
    );
    assert!(matches!(
        signed_json(future),
        Err(ManifestError::DesktopVersionTooOld { required, current })
            if required == Version::parse("99.0.0").unwrap()
                && current == Version::parse("0.1.0").unwrap()
    ));

    let stable_minimum = replace_once(
        "\"minimum_desktop_version\":\"0.1.0\"",
        "\"minimum_desktop_version\":\"0.2.0\"",
    );
    let bytes = stable_minimum.as_bytes();
    assert!(matches!(
        verifier_for("0.2.0-rc.1").verify(bytes, &sign(bytes), &diagnostics()),
        Err(ManifestError::DesktopVersionTooOld { required, current })
            if required == Version::parse("0.2.0").unwrap()
                && current == Version::parse("0.2.0-rc.1").unwrap()
    ));
}

#[test]
fn artifact_digest_must_be_canonical_lowercase_sha256() {
    let valid = "0123456789abcdef".repeat(4);
    for invalid in ["a".repeat(63), "A".repeat(64), "g".repeat(64)] {
        let json = replace_once(&valid, &invalid);
        assert!(matches!(
            signed_json(json),
            Err(ManifestError::InvalidField {
                field: "artifact.sha256"
            })
        ));
    }
}

#[test]
fn artifact_url_requires_unambiguous_https_without_credentials_or_fragment() {
    let valid = "https://downloads.example.com/dsh/runtime-0.1.1-rc.1.zip";
    for invalid in [
        "http://downloads.example.com/runtime.zip",
        "https:javascript:alert(1)",
        "https://user:pass@downloads.example.com/runtime.zip",
        "https://downloads.example.com/runtime.zip#fragment",
    ] {
        let json = replace_once(valid, invalid);
        assert!(matches!(
            signed_json(json),
            Err(ManifestError::InvalidField {
                field: "artifact.url"
            })
        ));
    }
}

#[test]
fn verification_time_and_compatibility_summary_are_bounded() {
    for invalid_time in [
        "2026-08-21",
        "2026-13-21T09:30:00Z",
        "2026-08-21T09:30:00+08:00",
    ] {
        let json = replace_once("2026-08-21T09:30:00Z", invalid_time);
        assert!(matches!(
            signed_json(json),
            Err(ManifestError::InvalidField {
                field: "verified_at"
            })
        ));
    }

    let valid_summary = "已验证 Windows 10/11 x64 桌面运行与隔离数据升级。";
    for invalid_summary in ["   ".to_owned(), "兼".repeat(513)] {
        let json = replace_once(valid_summary, &invalid_summary);
        assert!(matches!(
            signed_json(json),
            Err(ManifestError::InvalidField {
                field: "compatibility_summary"
            })
        ));
    }
}
