use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use futures_util::StreamExt;
use semver::Version;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use super::manifest::{ManifestVerifier, VerifiedManifest};
use crate::diagnostics::DiagnosticContext;

const OFFICIAL_PACKAGE_PATH: &str = "@deepseek-ai%2Fdsh";

/// 官方 npm 元数据中可发现的版本身份，仅用于通知，不是安装授权。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfficialRelease {
    pub version: Version,
    pub integrity: String,
}

/// HTTP 响应的最小、可测试边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 传输层稳定错误，不携带 URL、响应正文或请求头。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    Network,
    Timeout,
    ResponseTooLarge,
}

/// 可注入的异步 HTTP transport，便于生产强制 HTTPS、测试精确模拟故障。
pub trait SourceHttpTransport: Send + Sync {
    /// 获取一个有大小和时间上限的响应。
    ///
    /// :param url: 已由 source 校验的地址。
    /// :param timeout: 单次请求总时限。
    /// :param max_response_bytes: 最大响应字节数。
    /// :return: 状态码与受限响应体。
    /// :raises TransportError: 网络、超时或响应超限时返回稳定类别。
    fn get<'a>(
        &'a self,
        url: &'a Url,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Pin<Box<dyn Future<Output = Result<SourceHttpResponse, TransportError>> + Send + 'a>>;
}

/// 使用系统 TLS 的生产 HTTP transport。
pub struct ReqwestSourceTransport {
    client: reqwest::Client,
}

impl ReqwestSourceTransport {
    /// 创建只接受 HTTPS 的 source transport。
    ///
    /// :param connect_timeout: TLS/连接建立时限。
    /// :return: 可共享 transport。
    /// :raises SourceError: 客户端参数或 TLS 配置失败。
    pub fn new(connect_timeout: Duration) -> Result<Self, SourceError> {
        if connect_timeout.is_zero() {
            return Err(SourceError::InvalidConfiguration);
        }
        let client = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|_| SourceError::InvalidConfiguration)?;
        Ok(Self { client })
    }
}

impl SourceHttpTransport for ReqwestSourceTransport {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Pin<Box<dyn Future<Output = Result<SourceHttpResponse, TransportError>> + Send + 'a>> {
        Box::pin(async move {
            let request = async {
                let response = self
                    .client
                    .get(url.clone())
                    .send()
                    .await
                    .map_err(classify_reqwest_error)?;
                if response
                    .content_length()
                    .is_some_and(|size| size > max_response_bytes as u64)
                {
                    return Err(TransportError::ResponseTooLarge);
                }
                let status = response.status().as_u16();
                let mut body = Vec::new();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(classify_reqwest_error)?;
                    if body.len().saturating_add(chunk.len()) > max_response_bytes {
                        return Err(TransportError::ResponseTooLarge);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(SourceHttpResponse { status, body })
            };
            tokio::time::timeout(timeout, request)
                .await
                .map_err(|_| TransportError::Timeout)?
        })
    }
}

fn classify_reqwest_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Network
    }
}

/// 版本 source 的响应和重试策略。
#[derive(Clone, Debug)]
pub struct SourcePolicy {
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_retries: u32,
    pub retry_backoff: Duration,
}

impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(15),
            max_response_bytes: 2 * 1024 * 1024,
            max_retries: 2,
            retry_backoff: Duration::from_millis(250),
        }
    }
}

/// 官方发现或兼容 source 的稳定失败类别。
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SourceError {
    #[error("更新 source 配置无效")]
    InvalidConfiguration,
    #[error("更新 source 网络失败")]
    Network,
    #[error("更新 source 请求超时")]
    Timeout,
    #[error("更新 source 响应超过上限")]
    ResponseTooLarge,
    #[error("更新 source 被限流")]
    RateLimited,
    #[error("更新 source 服务暂时不可用")]
    ServerUnavailable,
    #[error("更新 source 返回状态码 {status}")]
    HttpStatus { status: u16 },
    #[error("更新 source 响应结构无效")]
    InvalidResponse,
    #[error("官方版本不是严格 semver")]
    InvalidVersion,
    #[error("官方 npm integrity 无效")]
    InvalidIntegrity,
    #[error("兼容清单不存在")]
    CompatibilityUnavailable,
    #[error("兼容清单签名或字段验证失败")]
    CompatibilityVerification,
}

/// dyn-compatible 的官方 npm 版本发现边界。
pub trait OfficialVersionSource: Send + Sync {
    /// 发现官方 latest 及其 exact record。
    ///
    /// :return: 官方版本与 npm integrity；不得据此安装。
    /// :raises SourceError: 网络、限流、超限或元数据无效时返回。
    fn latest<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<OfficialRelease, SourceError>> + Send + 'a>>;
}

/// dyn-compatible 的项目签名兼容清单边界。
pub trait CompatibilitySource: Send + Sync {
    /// 获取并验证当前平台的最新兼容清单。
    ///
    /// :return: endpoint 返回 404 时为 `None`，验证成功时为受信清单。
    /// :raises SourceError: 网络、签名、平台、架构或最低桌面版本校验失败时返回。
    fn latest_compatible<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<VerifiedManifest>, SourceError>> + Send + 'a>>;

    /// 使用共享更新上下文获取并验证清单；默认实现保持第三方 source 兼容。
    ///
    /// :param diagnostics: 同一次检查共享的类型化上下文。
    /// :return: endpoint 不存在时为 None，验证成功时为受信清单。
    /// :raises SourceError: 网络或验证失败时返回稳定错误。
    fn latest_compatible_with_context<'a>(
        &'a self,
        _diagnostics: &'a DiagnosticContext,
    ) -> Pin<Box<dyn Future<Output = Result<Option<VerifiedManifest>, SourceError>> + Send + 'a>>
    {
        self.latest_compatible()
    }
}

/// 固定查询官方 `@deepseek-ai/dsh` 的 npm source。
pub struct NpmOfficialVersionSource {
    metadata_url: Url,
    transport: Arc<dyn SourceHttpTransport>,
    policy: SourcePolicy,
}

impl NpmOfficialVersionSource {
    /// 创建官方 npm 版本 source。
    ///
    /// :param registry_root: 可由构建配置注入的 HTTPS registry 根。
    /// :param transport: 可替换 transport。
    /// :param policy: 超时、响应上限与重试边界。
    /// :return: 固定官方包名的 source。
    /// :raises SourceError: URL 或策略不安全时返回。
    pub fn new(
        registry_root: Url,
        transport: Arc<dyn SourceHttpTransport>,
        policy: SourcePolicy,
    ) -> Result<Self, SourceError> {
        validate_https_url(&registry_root)?;
        validate_policy(&policy)?;
        let metadata_url = registry_root
            .join(OFFICIAL_PACKAGE_PATH)
            .map_err(|_| SourceError::InvalidConfiguration)?;
        Ok(Self {
            metadata_url,
            transport,
            policy,
        })
    }
}

impl OfficialVersionSource for NpmOfficialVersionSource {
    fn latest<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<OfficialRelease, SourceError>> + Send + 'a>> {
        Box::pin(async move {
            let body = fetch_bytes(
                self.transport.as_ref(),
                &self.metadata_url,
                &self.policy,
                false,
            )
            .await?
            .ok_or(SourceError::InvalidResponse)?;
            parse_npm_document(&body)
        })
    }
}

/// 从两个独立 HTTPS endpoint 获取 raw manifest 与 detached signature 的 source。
pub struct SignedCompatibilitySource {
    manifest_url: Url,
    signature_url: Url,
    transport: Arc<dyn SourceHttpTransport>,
    policy: SourcePolicy,
    verifier: ManifestVerifier,
}

impl SignedCompatibilitySource {
    /// 创建签名兼容 source。
    ///
    /// :param manifest_url: 原始 JSON 清单 HTTPS 地址。
    /// :param signature_url: detached signature HTTPS 地址。
    /// :param transport: 可注入 transport。
    /// :param policy: 请求边界。
    /// :param verifier: 内置发布公钥和当前桌面版本的验证器。
    /// :return: 尚未发起网络请求的 source。
    /// :raises SourceError: URL 或策略不安全时返回。
    pub fn new(
        manifest_url: Url,
        signature_url: Url,
        transport: Arc<dyn SourceHttpTransport>,
        policy: SourcePolicy,
        verifier: ManifestVerifier,
    ) -> Result<Self, SourceError> {
        validate_https_url(&manifest_url)?;
        validate_https_url(&signature_url)?;
        validate_policy(&policy)?;
        Ok(Self {
            manifest_url,
            signature_url,
            transport,
            policy,
            verifier,
        })
    }

    async fn fetch_compatible(
        &self,
        diagnostics: Option<&DiagnosticContext>,
    ) -> Result<Option<VerifiedManifest>, SourceError> {
        let Some(manifest) = fetch_bytes(
            self.transport.as_ref(),
            &self.manifest_url,
            &self.policy,
            true,
        )
        .await?
        else {
            return Ok(None);
        };
        let signature = fetch_bytes(
            self.transport.as_ref(),
            &self.signature_url,
            &self.policy,
            false,
        )
        .await?
        .ok_or(SourceError::InvalidResponse)?;
        if signature.len() > 128 {
            return Err(SourceError::InvalidResponse);
        }
        let signature = std::str::from_utf8(&signature)
            .map_err(|_| SourceError::InvalidResponse)?
            .trim();
        let verified = match diagnostics {
            Some(context) => self
                .verifier
                .verify_with_context(&manifest, signature, context),
            None => self.verifier.verify(&manifest, signature),
        };
        verified
            .map(Some)
            .map_err(|_| SourceError::CompatibilityVerification)
    }
}

impl CompatibilitySource for SignedCompatibilitySource {
    fn latest_compatible<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<VerifiedManifest>, SourceError>> + Send + 'a>>
    {
        Box::pin(async move { self.fetch_compatible(None).await })
    }

    fn latest_compatible_with_context<'a>(
        &'a self,
        diagnostics: &'a DiagnosticContext,
    ) -> Pin<Box<dyn Future<Output = Result<Option<VerifiedManifest>, SourceError>> + Send + 'a>>
    {
        Box::pin(async move { self.fetch_compatible(Some(diagnostics)).await })
    }
}

async fn fetch_bytes(
    transport: &dyn SourceHttpTransport,
    url: &Url,
    policy: &SourcePolicy,
    missing_is_none: bool,
) -> Result<Option<Vec<u8>>, SourceError> {
    for attempt in 0..=policy.max_retries {
        let response = transport
            .get(url, policy.request_timeout, policy.max_response_bytes)
            .await;
        match response {
            Ok(response) if response.status == 200 => {
                if response.body.len() > policy.max_response_bytes {
                    return Err(SourceError::ResponseTooLarge);
                }
                return Ok(Some(response.body));
            }
            Ok(response) if response.status == 404 && missing_is_none => return Ok(None),
            Ok(response) if response.status == 429 => {
                if attempt == policy.max_retries {
                    return Err(SourceError::RateLimited);
                }
            }
            Ok(response) if (500..=599).contains(&response.status) => {
                if attempt == policy.max_retries {
                    return Err(SourceError::ServerUnavailable);
                }
            }
            Ok(response) => {
                return Err(SourceError::HttpStatus {
                    status: response.status,
                });
            }
            Err(TransportError::ResponseTooLarge) => return Err(SourceError::ResponseTooLarge),
            Err(error) => {
                if attempt == policy.max_retries {
                    return Err(match error {
                        TransportError::Network => SourceError::Network,
                        TransportError::Timeout => SourceError::Timeout,
                        TransportError::ResponseTooLarge => SourceError::ResponseTooLarge,
                    });
                }
            }
        }
        if !policy.retry_backoff.is_zero() {
            tokio::time::sleep(policy.retry_backoff).await;
        }
    }
    Err(SourceError::Network)
}

#[derive(Debug, Deserialize)]
struct NpmDocument {
    #[serde(rename = "dist-tags")]
    dist_tags: DistTags,
    versions: BTreeMap<String, NpmVersionRecord>,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: String,
}

#[derive(Debug, Deserialize)]
struct NpmVersionRecord {
    version: String,
    dist: NpmDist,
}

#[derive(Debug, Deserialize)]
struct NpmDist {
    integrity: String,
}

fn parse_npm_document(body: &[u8]) -> Result<OfficialRelease, SourceError> {
    let document: NpmDocument =
        serde_json::from_slice(body).map_err(|_| SourceError::InvalidResponse)?;
    let version =
        Version::parse(&document.dist_tags.latest).map_err(|_| SourceError::InvalidVersion)?;
    let exact = document
        .versions
        .get(&document.dist_tags.latest)
        .ok_or(SourceError::InvalidResponse)?;
    if exact.version != document.dist_tags.latest {
        return Err(SourceError::InvalidResponse);
    }
    validate_integrity(&exact.dist.integrity)?;
    Ok(OfficialRelease {
        version,
        integrity: exact.dist.integrity.clone(),
    })
}

fn validate_integrity(value: &str) -> Result<(), SourceError> {
    let payload = value
        .strip_prefix("sha512-")
        .ok_or(SourceError::InvalidIntegrity)?;
    decode_canonical_sha512(payload)
        .map(|_| ())
        .ok_or(SourceError::InvalidIntegrity)
}

fn decode_canonical_sha512(payload: &str) -> Option<[u8; 64]> {
    // SHA-512 固定 64 bytes，规范 base64 必须恰好 88 字符，并以 `==` 收尾。
    if payload.len() != 88 || !payload.ends_with("==") || payload[..86].contains('=') {
        return None;
    }
    let mut decoded = [0_u8; 64];
    let bytes = payload.as_bytes();
    let mut output = 0_usize;
    for chunk_index in 0..22 {
        let offset = chunk_index * 4;
        let first = base64_value(bytes[offset])?;
        let second = base64_value(bytes[offset + 1])?;
        decoded[output] = (first << 2) | (second >> 4);
        output += 1;
        if chunk_index == 21 {
            // 单字节尾块的未使用低 4 bit 必须为零，否则存在多种非规范编码。
            if second & 0x0f != 0 || bytes[offset + 2] != b'=' || bytes[offset + 3] != b'=' {
                return None;
            }
            break;
        }
        let third = base64_value(bytes[offset + 2])?;
        let fourth = base64_value(bytes[offset + 3])?;
        decoded[output] = (second << 4) | (third >> 2);
        decoded[output + 1] = (third << 6) | fourth;
        output += 2;
    }
    (output == decoded.len()).then_some(decoded)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn validate_https_url(url: &Url) -> Result<(), SourceError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(SourceError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_policy(policy: &SourcePolicy) -> Result<(), SourceError> {
    if policy.request_timeout.is_zero()
        || policy.request_timeout > Duration::from_secs(5 * 60)
        || policy.max_response_bytes == 0
        || policy.max_response_bytes > 8 * 1024 * 1024
        || policy.max_retries > 5
        || policy.retry_backoff > Duration::from_secs(30)
    {
        return Err(SourceError::InvalidConfiguration);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use ed25519_dalek::{Signer, SigningKey};
    use semver::Version;
    use url::Url;

    use super::{
        CompatibilitySource, ManifestVerifier, SignedCompatibilitySource, SourceError,
        SourceHttpResponse, SourceHttpTransport, SourcePolicy, TransportError,
        decode_canonical_sha512, fetch_bytes, parse_npm_document,
    };

    struct ScriptedTransport {
        replies: Mutex<VecDeque<Result<SourceHttpResponse, TransportError>>>,
    }

    impl ScriptedTransport {
        fn new(replies: Vec<Result<SourceHttpResponse, TransportError>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
            }
        }
    }

    impl SourceHttpTransport for ScriptedTransport {
        fn get<'a>(
            &'a self,
            _url: &'a Url,
            _timeout: Duration,
            _max_response_bytes: usize,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<SourceHttpResponse, TransportError>>
                    + Send
                    + 'a,
            >,
        > {
            let reply = self
                .replies
                .lock()
                .expect("测试队列锁")
                .pop_front()
                .expect("预置响应足够");
            Box::pin(async move { reply })
        }
    }

    fn policy(max_retries: u32) -> SourcePolicy {
        SourcePolicy {
            request_timeout: Duration::from_millis(20),
            max_response_bytes: 64,
            max_retries,
            retry_backoff: Duration::ZERO,
        }
    }

    #[test]
    fn parses_latest_exact_version_and_integrity() {
        let body = br#"{
          "dist-tags":{"latest":"0.2.0"},
          "versions":{"0.2.0":{"version":"0.2.0","dist":{"integrity":"sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="}}}
        }"#;
        let release = parse_npm_document(body).expect("合法 npm 元数据应可解析");
        assert_eq!(release.version.to_string(), "0.2.0");
        assert_eq!(
            release.integrity,
            "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
        );
    }

    #[test]
    fn ignores_unrelated_registry_metadata_but_keeps_exact_record_strict() {
        let body = br#"{
          "name":"@deepseek-ai/dsh",
          "dist-tags":{"latest":"0.2.0","next":"0.3.0-beta.1"},
          "versions":{"0.2.0":{"name":"@deepseek-ai/dsh","version":"0.2.0",
            "dist":{"tarball":"https://registry.example.invalid/pkg.tgz","integrity":"sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="}}}
        }"#;
        let release = parse_npm_document(body).expect("无关 npm 元数据不应破坏所需字段解析");
        assert_eq!(release.version.to_string(), "0.2.0");
    }

    #[test]
    fn rejects_missing_exact_record_and_malformed_integrity() {
        let missing = br#"{"dist-tags":{"latest":"0.2.0"},"versions":{}}"#;
        assert!(matches!(
            parse_npm_document(missing),
            Err(SourceError::InvalidResponse)
        ));

        let malformed = br#"{
          "dist-tags":{"latest":"0.2.0"},
          "versions":{"0.2.0":{"version":"0.2.0","dist":{"integrity":"not-an-sri"}}}
        }"#;
        assert!(matches!(
            parse_npm_document(malformed),
            Err(SourceError::InvalidIntegrity)
        ));

        let short_digest = br#"{
          "dist-tags":{"latest":"0.2.0"},
          "versions":{"0.2.0":{"version":"0.2.0","dist":{"integrity":"sha512-YWJjZA=="}}}
        }"#;
        assert!(matches!(
            parse_npm_document(short_digest),
            Err(SourceError::InvalidIntegrity)
        ));

        let noncanonical_tail = br#"{
          "dist-tags":{"latest":"0.2.0"},
          "versions":{"0.2.0":{"version":"0.2.0","dist":{"integrity":"sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB=="}}}
        }"#;
        assert!(matches!(
            parse_npm_document(noncanonical_tail),
            Err(SourceError::InvalidIntegrity)
        ));

        let mismatched = br#"{
          "dist-tags":{"latest":"0.2.0"},
          "versions":{"0.2.0":{"version":"0.2.1","dist":{"integrity":"sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="}}}
        }"#;
        assert!(matches!(
            parse_npm_document(mismatched),
            Err(SourceError::InvalidResponse)
        ));
    }

    #[test]
    fn rejects_empty_and_invalid_latest_semver() {
        for body in [
            br#"{"dist-tags":{"latest":""},"versions":{}}"#.as_slice(),
            br#"{"dist-tags":{"latest":"newest"},"versions":{}}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_npm_document(body),
                Err(SourceError::InvalidVersion)
            ));
        }
    }

    #[test]
    fn canonical_sri_decoder_returns_all_sixty_four_digest_bytes() {
        let payload = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==";
        assert_eq!(
            decode_canonical_sha512(payload).expect("规范 SHA-512 base64"),
            std::array::from_fn(|index| index as u8)
        );
    }

    #[tokio::test]
    async fn retries_rate_limit_and_server_errors_with_a_bound() {
        let transport = ScriptedTransport::new(vec![
            Ok(SourceHttpResponse {
                status: 429,
                body: vec![],
            }),
            Ok(SourceHttpResponse {
                status: 503,
                body: vec![],
            }),
            Ok(SourceHttpResponse {
                status: 200,
                body: b"ok".to_vec(),
            }),
        ]);
        let url = Url::parse("https://registry.example.invalid/pkg").unwrap();
        let body = fetch_bytes(&transport, &url, &policy(2), false)
            .await
            .expect("第三次成功")
            .expect("200 有响应体");
        assert_eq!(body, b"ok");
    }

    #[tokio::test]
    async fn preserves_timeout_rate_limit_server_and_size_failure_categories() {
        let url = Url::parse("https://registry.example.invalid/pkg").unwrap();
        for (reply, expected) in [
            (Err(TransportError::Network), SourceError::Network),
            (Err(TransportError::Timeout), SourceError::Timeout),
            (
                Ok(SourceHttpResponse {
                    status: 429,
                    body: vec![],
                }),
                SourceError::RateLimited,
            ),
            (
                Ok(SourceHttpResponse {
                    status: 500,
                    body: vec![],
                }),
                SourceError::ServerUnavailable,
            ),
            (
                Err(TransportError::ResponseTooLarge),
                SourceError::ResponseTooLarge,
            ),
        ] {
            let transport = ScriptedTransport::new(vec![reply]);
            assert_eq!(
                fetch_bytes(&transport, &url, &policy(0), false).await,
                Err(expected)
            );
        }
        let transport = ScriptedTransport::new(vec![Ok(SourceHttpResponse {
            status: 200,
            body: vec![b'x'; 65],
        })]);
        assert_eq!(
            fetch_bytes(&transport, &url, &policy(0), false).await,
            Err(SourceError::ResponseTooLarge)
        );
    }

    #[tokio::test]
    async fn compatibility_source_verifies_raw_bytes_and_detached_signature() {
        let signing_key = SigningKey::from_bytes(&[9_u8; 32]);
        let public_key = lower_hex(&signing_key.verifying_key().to_bytes());
        let verifier =
            ManifestVerifier::new(&public_key, Version::parse("0.1.0").unwrap()).unwrap();
        let manifest = br#"{"schema":1,"dsh_version":"0.2.0","node_version":"24.15.0","minimum_desktop_version":"0.1.0","platform":"windows","arch":"x86_64","artifact":{"url":"https://downloads.example.invalid/runtime.zip","size":10,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"verified_at":"2026-08-22T00:00:00Z","compatibility_summary":"Windows x64 verified"}"#.to_vec();
        let signature = lower_hex(&signing_key.sign(&manifest).to_bytes());
        let transport = Arc::new(ScriptedTransport::new(vec![
            Ok(SourceHttpResponse {
                status: 200,
                body: manifest,
            }),
            Ok(SourceHttpResponse {
                status: 200,
                body: signature.into_bytes(),
            }),
        ]));
        let mut source_policy = policy(0);
        source_policy.max_response_bytes = 4096;
        let source = SignedCompatibilitySource::new(
            Url::parse("https://updates.example.invalid/manifest.json").unwrap(),
            Url::parse("https://updates.example.invalid/manifest.sig").unwrap(),
            transport,
            source_policy,
            verifier,
        )
        .unwrap();
        let verified = source
            .latest_compatible()
            .await
            .unwrap()
            .expect("存在兼容清单");
        assert_eq!(verified.manifest.dsh_version.to_string(), "0.2.0");
    }

    #[tokio::test]
    async fn compatibility_404_is_absent_but_bad_signature_is_failure() {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let verifier = ManifestVerifier::new(
            &lower_hex(&signing_key.verifying_key().to_bytes()),
            Version::parse("0.1.0").unwrap(),
        )
        .unwrap();
        let mut source_policy = policy(0);
        source_policy.max_response_bytes = 4096;
        let absent = SignedCompatibilitySource::new(
            Url::parse("https://updates.example.invalid/manifest.json").unwrap(),
            Url::parse("https://updates.example.invalid/manifest.sig").unwrap(),
            Arc::new(ScriptedTransport::new(vec![Ok(SourceHttpResponse {
                status: 404,
                body: vec![],
            })])),
            source_policy.clone(),
            verifier.clone(),
        )
        .unwrap();
        assert!(absent.latest_compatible().await.unwrap().is_none());

        let invalid = SignedCompatibilitySource::new(
            Url::parse("https://updates.example.invalid/manifest.json").unwrap(),
            Url::parse("https://updates.example.invalid/manifest.sig").unwrap(),
            Arc::new(ScriptedTransport::new(vec![
                Ok(SourceHttpResponse {
                    status: 200,
                    body: b"{}".to_vec(),
                }),
                Ok(SourceHttpResponse {
                    status: 200,
                    body: vec![b'0'; 128],
                }),
            ])),
            source_policy,
            verifier,
        )
        .unwrap();
        assert_eq!(
            invalid.latest_compatible().await,
            Err(SourceError::CompatibilityVerification)
        );
    }

    fn lower_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
