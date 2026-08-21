use crate::diagnostics::{DiagnosticContext, DiagnosticErrorKind, DiagnosticStage, TraceKind};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Instant;
use thiserror::Error;
use url::Url;

const MANIFEST_SCHEMA: u32 = 1;
const MAX_COMPATIBILITY_SUMMARY_CHARS: usize = 512;

/// 已通过签名与语义校验的兼容运行时清单。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilityManifestV1 {
    pub schema: u32,
    pub dsh_version: Version,
    pub node_version: Version,
    pub minimum_desktop_version: Version,
    pub platform: String,
    pub arch: String,
    pub artifact: RuntimeArtifact,
    pub verified_at: String,
    pub compatibility_summary: String,
}

/// 兼容运行时压缩包的受信下载约束。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeArtifact {
    pub url: Url,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// 同时携带类型化清单与原始清单摘要的验证结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedManifest {
    pub manifest: CompatibilityManifestV1,
    pub manifest_digest: String,
}

/// 兼容清单拒绝原因；错误正文刻意不携带输入、公钥或签名。
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("兼容清单公钥不是规范的 32-byte 小写 hex")]
    InvalidPublicKeyEncoding,
    #[error("兼容清单签名不是规范的 64-byte 小写 hex")]
    InvalidSignatureEncoding,
    #[error("兼容清单签名验证失败")]
    SignatureVerification,
    #[error("兼容清单 JSON 结构无效")]
    InvalidJson,
    #[error("不支持的兼容清单 schema: {schema}")]
    UnsupportedSchema { schema: u32 },
    #[error("当前桌面版本 {current} 低于清单要求的最低版本 {required}")]
    DesktopVersionTooOld { required: Version, current: Version },
    #[error("兼容清单字段无效: {field}")]
    InvalidField { field: &'static str },
}

/// 使用固定 Ed25519 公钥验证发布方提供的 detached signature。
#[derive(Clone, Debug)]
pub struct ManifestVerifier {
    verifying_key: VerifyingKey,
    current_desktop_version: Version,
}

impl ManifestVerifier {
    /// 从规范的 32-byte 小写 hex 公钥创建验证器。
    ///
    /// :param public_key_hex: 发布公钥的 64 个小写十六进制字符。
    /// :param current_desktop_version: 当前桌面端的严格 semver，用于强制最低版本门禁。
    /// :return: 只持有验证公钥的清单验证器。
    /// :raises ManifestError: 编码、长度或 Ed25519 公钥无效时返回
    ///   `InvalidPublicKeyEncoding`。
    pub fn new(
        public_key_hex: &str,
        current_desktop_version: Version,
    ) -> Result<Self, ManifestError> {
        let bytes = decode_canonical_hex::<32>(public_key_hex)
            .ok_or(ManifestError::InvalidPublicKeyEncoding)?;
        let verifying_key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| ManifestError::InvalidPublicKeyEncoding)?;
        Ok(Self {
            verifying_key,
            current_desktop_version,
        })
    }

    /// 验证原始清单字节的 detached signature，再执行严格解析与字段校验。
    ///
    /// 签名必须覆盖调用方收到的原始字节；验证前不解析、重排或重新序列化 JSON，
    /// 避免不同规范化方式造成签名边界歧义。
    ///
    /// :param manifest_bytes: 网络响应中的原始清单字节。
    /// :param signature_hex: 对原始字节签名的 64-byte 小写 hex。
    /// :return: 类型化清单及同一原始字节的 SHA-256 摘要。
    /// :raises ManifestError: 签名、JSON 结构或任一安全字段不符合约束时返回。
    pub fn verify(
        &self,
        manifest_bytes: &[u8],
        signature_hex: &str,
    ) -> Result<VerifiedManifest, ManifestError> {
        self.verify_with_context(
            manifest_bytes,
            signature_hex,
            &DiagnosticContext::noop(TraceKind::Update),
        )
    }

    /// 使用调用方操作上下文验证清单，使同一 trace 可贯穿后续下载与激活。
    ///
    /// :param manifest_bytes: 网络返回的原始清单字节，不进入诊断事件。
    /// :param signature_hex: detached signature，不进入诊断事件。
    /// :param diagnostics: 调用链共享的类型化诊断上下文。
    /// :return: 签名与字段均通过验证的清单。
    /// :raises ManifestError: 与 `verify` 相同的稳定验证错误。
    pub fn verify_with_context(
        &self,
        manifest_bytes: &[u8],
        signature_hex: &str,
        diagnostics: &DiagnosticContext,
    ) -> Result<VerifiedManifest, ManifestError> {
        let started = Instant::now();
        diagnostics.record(DiagnosticStage::ManifestVerify, 0, 0, None, None);
        let result = self.verify_inner(manifest_bytes, signature_hex);
        diagnostics.record(
            DiagnosticStage::ManifestVerify,
            started.elapsed().as_millis() as u64,
            0,
            None,
            result
                .as_ref()
                .err()
                .map(|_| DiagnosticErrorKind::UpdateFailure),
        );
        result
    }

    fn verify_inner(
        &self,
        manifest_bytes: &[u8],
        signature_hex: &str,
    ) -> Result<VerifiedManifest, ManifestError> {
        let signature_bytes = decode_canonical_hex::<64>(signature_hex)
            .ok_or(ManifestError::InvalidSignatureEncoding)?;
        let signature = Signature::from_bytes(&signature_bytes);
        self.verifying_key
            .verify_strict(manifest_bytes, &signature)
            .map_err(|_| ManifestError::SignatureVerification)?;

        let raw: RawManifest =
            serde_json::from_slice(manifest_bytes).map_err(|_| ManifestError::InvalidJson)?;
        let manifest = CompatibilityManifestV1::try_from(raw)?;
        if manifest.minimum_desktop_version > self.current_desktop_version {
            return Err(ManifestError::DesktopVersionTooOld {
                required: manifest.minimum_desktop_version,
                current: self.current_desktop_version.clone(),
            });
        }
        Ok(VerifiedManifest {
            manifest,
            manifest_digest: sha256_hex(manifest_bytes),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema: u32,
    dsh_version: String,
    node_version: String,
    minimum_desktop_version: String,
    platform: String,
    arch: String,
    artifact: RawArtifact,
    verified_at: String,
    compatibility_summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtifact {
    url: String,
    size: u64,
    sha256: String,
}

impl TryFrom<RawManifest> for CompatibilityManifestV1 {
    type Error = ManifestError;

    fn try_from(raw: RawManifest) -> Result<Self, Self::Error> {
        if raw.schema != MANIFEST_SCHEMA {
            return Err(ManifestError::UnsupportedSchema { schema: raw.schema });
        }
        let dsh_version = parse_version("dsh_version", &raw.dsh_version)?;
        let node_version = parse_version("node_version", &raw.node_version)?;
        let minimum_desktop_version =
            parse_version("minimum_desktop_version", &raw.minimum_desktop_version)?;
        if raw.platform != "windows" {
            return Err(ManifestError::InvalidField { field: "platform" });
        }
        if raw.arch != "x86_64" {
            return Err(ManifestError::InvalidField { field: "arch" });
        }
        let artifact = RuntimeArtifact::try_from(raw.artifact)?;
        if !is_utc_timestamp(&raw.verified_at) {
            return Err(ManifestError::InvalidField {
                field: "verified_at",
            });
        }
        let summary_length = raw.compatibility_summary.chars().count();
        if raw.compatibility_summary.trim().is_empty()
            || summary_length > MAX_COMPATIBILITY_SUMMARY_CHARS
            || raw.compatibility_summary.chars().any(char::is_control)
        {
            return Err(ManifestError::InvalidField {
                field: "compatibility_summary",
            });
        }

        Ok(Self {
            schema: raw.schema,
            dsh_version,
            node_version,
            minimum_desktop_version,
            platform: raw.platform,
            arch: raw.arch,
            artifact,
            verified_at: raw.verified_at,
            compatibility_summary: raw.compatibility_summary,
        })
    }
}

impl TryFrom<RawArtifact> for RuntimeArtifact {
    type Error = ManifestError;

    fn try_from(raw: RawArtifact) -> Result<Self, Self::Error> {
        if raw.size == 0 {
            return Err(ManifestError::InvalidField {
                field: "artifact.size",
            });
        }
        let sha256 =
            decode_canonical_hex::<32>(&raw.sha256).ok_or(ManifestError::InvalidField {
                field: "artifact.sha256",
            })?;
        let url = Url::parse(&raw.url).map_err(|_| ManifestError::InvalidField {
            field: "artifact.url",
        })?;
        let safe_https = url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none();
        if !safe_https {
            return Err(ManifestError::InvalidField {
                field: "artifact.url",
            });
        }
        Ok(Self {
            url,
            size: raw.size,
            sha256,
        })
    }
}

fn parse_version(field: &'static str, value: &str) -> Result<Version, ManifestError> {
    Version::parse(value).map_err(|_| ManifestError::InvalidField { field })
}

fn is_canonical_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn decode_canonical_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if !is_canonical_lower_hex(value, N * 2) {
        return None;
    }
    let mut decoded = [0_u8; N];
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (slot, pair) in decoded.iter_mut().zip(pairs) {
        *slot = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(decoded)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("向 String 写入不会失败");
    }
    encoded
}

fn is_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return false;
    }
    if bytes
        .iter()
        .enumerate()
        .any(|(index, byte)| !matches!(index, 4 | 7 | 10 | 13 | 16 | 19) && !byte.is_ascii_digit())
    {
        return false;
    }
    let number = |start: usize, end: usize| {
        value[start..end]
            .parse::<u32>()
            .expect("已确认时间字段仅含 ASCII 数字")
    };
    let year = number(0, 4);
    let month = number(5, 7);
    let day = number(8, 10);
    let hour = number(11, 13);
    let minute = number(14, 16);
    let second = number(17, 19);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => return false,
    };
    year > 0 && (1..=max_day).contains(&day) && hour < 24 && minute < 60 && second < 60
}
