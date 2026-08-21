use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::Notify,
    time::Instant,
};
use url::Url;

use super::manifest::RuntimeArtifact;
use crate::diagnostics::{DiagnosticContext, DiagnosticErrorKind, DiagnosticStage};

/// 下载器的资源与重试边界。
#[derive(Clone, Debug)]
pub struct DownloadPolicy {
    pub connect_timeout: Duration,
    pub response_timeout: Duration,
    pub total_timeout: Duration,
    pub max_artifact_bytes: u64,
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub max_jitter: Duration,
    pub max_retry_after: Duration,
}

impl Default for DownloadPolicy {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            response_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(15 * 60),
            max_artifact_bytes: 2 * 1024 * 1024 * 1024,
            max_retries: 3,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(4),
            max_jitter: Duration::from_millis(100),
            max_retry_after: Duration::from_secs(30),
        }
    }
}

/// 可由界面或应用退出流程触发的下载取消句柄。
#[derive(Clone, Debug, Default)]
pub struct DownloadCancellation {
    inner: Arc<CancellationInner>,
}

#[derive(Debug, Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl DownloadCancellation {
    /// 请求取消所有共享该句柄的等待与下载操作。
    ///
    /// :return: `()`；重复调用保持幂等。
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// 一次受验证下载所需的受信元数据与隔离路径。
pub struct DownloadRequest<'a> {
    pub artifact: &'a RuntimeArtifact,
    pub updates_dir: &'a Path,
    pub cancellation: DownloadCancellation,
}

/// 已完成大小与摘要双校验、可交给解压器的暂存文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedArtifact {
    pub verified_path: PathBuf,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// 下载被拒绝或未能安全完成的稳定原因。
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("下载地址必须是 HTTPS")]
    InsecureUrl,
    #[error("无法创建 HTTPS 客户端")]
    ClientConfiguration,
    #[error("下载策略字段无效: {field}")]
    InvalidPolicy { field: &'static str },
    #[error("trace id 不符合隔离目录规则")]
    InvalidTraceId,
    #[error("目标 verified 文件已存在")]
    VerifiedAlreadyExists,
    #[error("下载内容超过资源上限")]
    SizeLimitExceeded,
    #[error("下载大小与兼容清单不一致")]
    SizeMismatch,
    #[error("下载摘要与兼容清单不一致")]
    DigestMismatch,
    #[error("下载网络连接中断")]
    Network,
    #[error("下载超过时间限制")]
    Timeout,
    #[error("下载已取消")]
    Cancelled,
    #[error("下载服务器返回不可接受的状态码 {status}")]
    HttpStatus { status: u16 },
    #[error("无法安全暂存下载文件")]
    FileSystem,
    #[error("trace 暂存目录超出 updates 根目录")]
    UnsafeTraceDirectory,
}

/// 可替换的异步资源下载边界；返回 boxed future 以保持 trait object 兼容。
pub trait ArtifactDownloader: Send + Sync {
    /// 下载并验证一个清单资源。
    ///
    /// :param request: 已验证清单给出的资源、隔离根目录与取消句柄。
    /// :param diagnostics: 提供内部生成 trace 的类型化更新上下文。
    /// :return: 成功时返回 `.verified` 文件元数据。
    /// :raises DownloadError: URL、网络、资源上限或完整性校验失败时返回。
    fn download<'a>(
        &'a self,
        request: DownloadRequest<'a>,
        diagnostics: &'a DiagnosticContext,
    ) -> Pin<Box<dyn Future<Output = Result<DownloadedArtifact, DownloadError>> + Send + 'a>>;
}

/// 基于系统 TLS 信任库的受限 HTTPS 流式下载器。
pub struct HttpsDownloader {
    client: reqwest::Client,
    policy: DownloadPolicy,
    #[cfg(test)]
    allow_http: bool,
}

impl HttpsDownloader {
    /// 构建仅允许 HTTPS 的生产下载器。
    ///
    /// :param policy: 超时、大小与重试边界。
    /// :return: 配置完成的下载器。
    /// :raises DownloadError: 策略越界或 TLS 客户端无法构建时返回。
    pub fn new(policy: DownloadPolicy) -> Result<Self, DownloadError> {
        validate_download_policy(&policy)?;
        let client = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(policy.connect_timeout)
            .build()
            .map_err(|_| DownloadError::ClientConfiguration)?;
        Ok(Self {
            client,
            policy,
            #[cfg(test)]
            allow_http: false,
        })
    }

    /// 仅供当前模块的本地 HTTP server 测试使用；不会编译进生产构建。
    #[cfg(test)]
    fn new_for_test(policy: DownloadPolicy) -> Result<Self, DownloadError> {
        validate_download_policy(&policy)?;
        let client = reqwest::Client::builder()
            .connect_timeout(policy.connect_timeout)
            .build()
            .map_err(|_| DownloadError::ClientConfiguration)?;
        Ok(Self {
            client,
            policy,
            allow_http: true,
        })
    }

    fn validate_url(&self, value: &str) -> Result<Url, DownloadError> {
        let url = Url::parse(value).map_err(|_| DownloadError::InsecureUrl)?;
        #[cfg(test)]
        let allowed_scheme = url.scheme() == "https" || (self.allow_http && url.scheme() == "http");
        #[cfg(not(test))]
        let allowed_scheme = url.scheme() == "https";
        if !allowed_scheme
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(DownloadError::InsecureUrl);
        }
        Ok(url)
    }

    async fn download_inner(
        &self,
        request: &DownloadRequest<'_>,
        diagnostics: &DiagnosticContext,
    ) -> Result<DownloadedArtifact, DownloadError> {
        self.validate_url(request.artifact.url.as_str())?;
        let trace_id = diagnostics.trace_str();
        validate_trace_id(trace_id)?;
        if request.artifact.size > self.policy.max_artifact_bytes {
            return Err(DownloadError::SizeLimitExceeded);
        }

        let trace_dir = request.updates_dir.join(trace_id);
        fs::create_dir_all(&trace_dir)
            .await
            .map_err(|_| DownloadError::FileSystem)?;
        // `trace_dir` 可能在运行前已被替换为 symlink/junction；只使用经过
        // canonicalize 且仍严格等于根目录直属 trace 子目录的路径继续写入。
        let canonical_updates = fs::canonicalize(request.updates_dir)
            .await
            .map_err(|_| DownloadError::FileSystem)?;
        let canonical_trace = fs::canonicalize(&trace_dir)
            .await
            .map_err(|_| DownloadError::FileSystem)?;
        validate_canonical_trace_path(&canonical_updates, &canonical_trace, trace_id)?;
        let part_path = canonical_trace.join("artifact.part");
        let verified_path = canonical_trace.join("artifact.verified");
        if fs::try_exists(&verified_path)
            .await
            .map_err(|_| DownloadError::FileSystem)?
        {
            return Err(DownloadError::VerifiedAlreadyExists);
        }

        let deadline = Instant::now()
            .checked_add(self.policy.total_timeout)
            .ok_or(DownloadError::InvalidPolicy {
                field: "total_timeout",
            })?;
        let result = tokio::select! {
            biased;
            _ = request.cancellation.cancelled() => Err(DownloadError::Cancelled),
            result = self.retry_download(request, &part_path, &verified_path, deadline, diagnostics) => result,
        };
        result
    }

    async fn retry_download(
        &self,
        request: &DownloadRequest<'_>,
        part_path: &Path,
        verified_path: &Path,
        deadline: Instant,
        diagnostics: &DiagnosticContext,
    ) -> Result<DownloadedArtifact, DownloadError> {
        let mut retry_index = 0_u32;
        loop {
            if Instant::now() >= deadline {
                return Err(DownloadError::Timeout);
            }
            diagnostics.record(DiagnosticStage::DownloadAttempt, 0, retry_index, None, None);
            let attempt_started = std::time::Instant::now();
            match self
                .download_attempt(request, part_path, verified_path, deadline)
                .await
            {
                Ok(downloaded) => {
                    diagnostics.record(
                        DiagnosticStage::DownloadComplete,
                        attempt_started.elapsed().as_millis() as u64,
                        retry_index,
                        None,
                        None,
                    );
                    return Ok(downloaded);
                }
                Err(AttemptFailure::Final(error)) => {
                    diagnostics.record(
                        DiagnosticStage::DownloadAttempt,
                        attempt_started.elapsed().as_millis() as u64,
                        retry_index,
                        None,
                        Some(DiagnosticErrorKind::UpdateFailure),
                    );
                    return Err(error);
                }
                Err(AttemptFailure::Retry { error, retry_after }) => {
                    diagnostics.record(
                        DiagnosticStage::DownloadAttempt,
                        attempt_started.elapsed().as_millis() as u64,
                        retry_index,
                        None,
                        Some(DiagnosticErrorKind::UpdateFailure),
                    );
                    if retry_index >= self.policy.max_retries {
                        return Err(error);
                    }
                    let delay = select_retry_delay(
                        retry_after,
                        self.policy.max_retry_after,
                        self.backoff(retry_index),
                    );
                    retry_index += 1;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if delay >= remaining {
                        return Err(DownloadError::Timeout);
                    }
                    tokio::select! {
                        biased;
                        _ = request.cancellation.cancelled() => return Err(DownloadError::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    async fn download_attempt(
        &self,
        request: &DownloadRequest<'_>,
        part_path: &Path,
        verified_path: &Path,
        deadline: Instant,
    ) -> Result<DownloadedArtifact, AttemptFailure> {
        // 每次尝试都 truncate 同一 `.part`，避免把上一次中断的字节拼接进新响应。
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(part_path)
            .await
            .map_err(|_| AttemptFailure::Final(DownloadError::FileSystem))?;

        let response = self
            .wait_response(request, deadline)
            .await
            .map_err(|error| AttemptFailure::Retry {
                error,
                retry_after: None,
            })?;
        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            let retry_after = if status.as_u16() == 429 {
                parse_retry_after(&response, self.policy.max_retry_after)
            } else {
                None
            };
            return Err(AttemptFailure::Retry {
                error: DownloadError::HttpStatus {
                    status: status.as_u16(),
                },
                retry_after,
            });
        }
        if !status.is_success() {
            return Err(AttemptFailure::Final(DownloadError::HttpStatus {
                status: status.as_u16(),
            }));
        }

        if let Some(declared) = response.content_length() {
            if declared > self.policy.max_artifact_bytes {
                return Err(AttemptFailure::Final(DownloadError::SizeLimitExceeded));
            }
            if declared != request.artifact.size {
                return Err(AttemptFailure::Final(DownloadError::SizeMismatch));
            }
        }

        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        loop {
            let next = tokio::select! {
                biased;
                _ = request.cancellation.cancelled() => {
                    return Err(AttemptFailure::Final(DownloadError::Cancelled));
                }
                result = tokio::time::timeout(self.response_wait(deadline), stream.next()) => {
                    result.map_err(|_| AttemptFailure::Retry {
                        error: DownloadError::Timeout,
                        retry_after: None,
                    })?
                }
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(|_| AttemptFailure::Retry {
                error: DownloadError::Network,
                retry_after: None,
            })?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or(AttemptFailure::Final(DownloadError::SizeLimitExceeded))?;
            if size > self.policy.max_artifact_bytes {
                return Err(AttemptFailure::Final(DownloadError::SizeLimitExceeded));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|_| AttemptFailure::Final(DownloadError::FileSystem))?;
        }

        if size != request.artifact.size {
            return Err(AttemptFailure::Final(DownloadError::SizeMismatch));
        }
        let digest: [u8; 32] = hasher.finalize().into();
        if digest != request.artifact.sha256 {
            return Err(AttemptFailure::Final(DownloadError::DigestMismatch));
        }
        file.flush()
            .await
            .map_err(|_| AttemptFailure::Final(DownloadError::FileSystem))?;
        file.sync_all()
            .await
            .map_err(|_| AttemptFailure::Final(DownloadError::FileSystem))?;
        drop(file);
        if fs::try_exists(verified_path)
            .await
            .map_err(|_| AttemptFailure::Final(DownloadError::FileSystem))?
        {
            return Err(AttemptFailure::Final(DownloadError::VerifiedAlreadyExists));
        }
        fs::rename(part_path, verified_path)
            .await
            .map_err(|_| AttemptFailure::Final(DownloadError::FileSystem))?;
        Ok(DownloadedArtifact {
            verified_path: verified_path.to_path_buf(),
            size,
            sha256: digest,
        })
    }

    async fn wait_response(
        &self,
        request: &DownloadRequest<'_>,
        deadline: Instant,
    ) -> Result<reqwest::Response, DownloadError> {
        let wait = self.response_wait(deadline);
        tokio::select! {
            biased;
            _ = request.cancellation.cancelled() => Err(DownloadError::Cancelled),
            result = tokio::time::timeout(wait, self.client.get(request.artifact.url.clone()).send()) => {
                match result {
                    Ok(Ok(response)) => Ok(response),
                    Ok(Err(_)) => Err(DownloadError::Network),
                    Err(_) => Err(DownloadError::Timeout),
                }
            }
        }
    }

    fn response_wait(&self, deadline: Instant) -> Duration {
        self.policy
            .response_timeout
            .min(deadline.saturating_duration_since(Instant::now()))
    }

    fn backoff(&self, retry_index: u32) -> Duration {
        let multiplier = 1_u32.checked_shl(retry_index.min(31)).unwrap_or(u32::MAX);
        let base = self
            .policy
            .initial_backoff
            .saturating_mul(multiplier)
            .min(self.policy.max_backoff);
        base.saturating_add(jitter(self.policy.max_jitter))
    }
}

impl ArtifactDownloader for HttpsDownloader {
    fn download<'a>(
        &'a self,
        request: DownloadRequest<'a>,
        diagnostics: &'a DiagnosticContext,
    ) -> Pin<Box<dyn Future<Output = Result<DownloadedArtifact, DownloadError>> + Send + 'a>> {
        Box::pin(async move { self.download_inner(&request, diagnostics).await })
    }
}

impl HttpsDownloader {
    /// 使用共享更新上下文下载，使所有 attempt/retry 沿用同一 trace。
    ///
    /// :param request: 受信 artifact、更新根与取消句柄。
    /// :param diagnostics: 调用链共享的类型化诊断上下文。
    /// :return: 大小和摘要均通过验证的暂存文件。
    /// :raises DownloadError: 网络、取消、资源或完整性边界失败时返回。
    pub async fn download_with_context(
        &self,
        request: DownloadRequest<'_>,
        diagnostics: &DiagnosticContext,
    ) -> Result<DownloadedArtifact, DownloadError> {
        self.download_inner(&request, diagnostics).await
    }
}

enum AttemptFailure {
    Final(DownloadError),
    Retry {
        error: DownloadError,
        retry_after: Option<Duration>,
    },
}

fn validate_trace_id(trace_id: &str) -> Result<(), DownloadError> {
    if trace_id.is_empty()
        || trace_id.len() > 64
        || !trace_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(DownloadError::InvalidTraceId);
    }
    Ok(())
}

const MAX_POLICY_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

fn validate_download_policy(policy: &DownloadPolicy) -> Result<(), DownloadError> {
    let timeout_fields = [
        ("connect_timeout", policy.connect_timeout),
        ("response_timeout", policy.response_timeout),
        ("total_timeout", policy.total_timeout),
    ];
    for (field, value) in timeout_fields {
        if value.is_zero() || value > MAX_POLICY_TIMEOUT {
            return Err(DownloadError::InvalidPolicy { field });
        }
    }
    if policy.max_artifact_bytes == 0 {
        return Err(DownloadError::InvalidPolicy {
            field: "max_artifact_bytes",
        });
    }
    if policy.initial_backoff > policy.max_backoff {
        return Err(DownloadError::InvalidPolicy {
            field: "initial_backoff",
        });
    }
    if policy.max_backoff > policy.total_timeout {
        return Err(DownloadError::InvalidPolicy {
            field: "max_backoff",
        });
    }
    if policy.max_jitter > policy.total_timeout {
        return Err(DownloadError::InvalidPolicy {
            field: "max_jitter",
        });
    }
    if policy.max_retry_after > policy.total_timeout {
        return Err(DownloadError::InvalidPolicy {
            field: "max_retry_after",
        });
    }
    if Instant::now().checked_add(policy.total_timeout).is_none() {
        return Err(DownloadError::InvalidPolicy {
            field: "total_timeout",
        });
    }
    Ok(())
}

fn validate_canonical_trace_path(
    canonical_updates: &Path,
    canonical_trace: &Path,
    trace_id: &str,
) -> Result<(), DownloadError> {
    validate_trace_id(trace_id)?;
    if canonical_trace != canonical_updates.join(trace_id) {
        return Err(DownloadError::UnsafeTraceDirectory);
    }
    Ok(())
}

fn parse_retry_after(response: &reqwest::Response, maximum: Duration) -> Option<Duration> {
    let seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds).min(maximum))
}

fn select_retry_delay(
    retry_after: Option<Duration>,
    maximum_retry_after: Duration,
    fallback_backoff: Duration,
) -> Duration {
    retry_after
        .map(|delay| delay.min(maximum_retry_after))
        .unwrap_or(fallback_backoff)
}

fn jitter(maximum: Duration) -> Duration {
    if maximum.is_zero() {
        return Duration::ZERO;
    }
    let ceiling = maximum.as_nanos();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        % (ceiling + 1);
    Duration::from_nanos(nanos.min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use sha2::{Digest, Sha256};
    use url::Url;

    use super::{
        ArtifactDownloader, DownloadCancellation, DownloadError, DownloadPolicy, DownloadRequest,
        HttpsDownloader, select_retry_delay,
    };
    use crate::diagnostics::{DiagnosticContext, TraceKind};
    use crate::update::manifest::RuntimeArtifact;

    #[derive(Clone)]
    struct MockReply {
        delay: Duration,
        bytes: Vec<u8>,
    }

    fn spawn_server(replies: Vec<MockReply>) -> (Url, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let address = listener.local_addr().expect("local address");
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_thread = Arc::clone(&attempts);
        thread::spawn(move || {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };
                attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut request = [0_u8; 2048];
                    let _ = socket.read(&mut request);
                    thread::sleep(reply.delay);
                    let _ = socket.write_all(&reply.bytes);
                    let _ = socket.flush();
                });
            }
        });
        (
            Url::parse(&format!("http://{address}/artifact.zip?token=secret")).expect("mock URL"),
            attempts,
        )
    }

    fn response(status: &str, body: &[u8], content_length: Option<u64>) -> MockReply {
        let length = content_length
            .map(|value| format!("Content-Length: {value}\r\n"))
            .unwrap_or_default();
        let mut bytes =
            format!("HTTP/1.1 {status}\r\n{length}Connection: close\r\n\r\n").into_bytes();
        bytes.extend_from_slice(body);
        MockReply {
            delay: Duration::ZERO,
            bytes,
        }
    }

    fn retry_response(status: &str, retry_after: Option<&str>) -> MockReply {
        let retry_after = retry_after
            .map(|value| format!("Retry-After: {value}\r\n"))
            .unwrap_or_default();
        MockReply {
            delay: Duration::ZERO,
            bytes: format!(
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\n{retry_after}Connection: close\r\n\r\n"
            )
            .into_bytes(),
        }
    }

    fn test_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dsh-download-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn artifact(url: Url, body: &[u8], expected_size: Option<u64>) -> RuntimeArtifact {
        RuntimeArtifact {
            url,
            size: expected_size.unwrap_or(body.len() as u64),
            sha256: Sha256::digest(body).into(),
        }
    }

    fn policy() -> DownloadPolicy {
        DownloadPolicy {
            connect_timeout: Duration::from_millis(100),
            response_timeout: Duration::from_millis(100),
            total_timeout: Duration::from_secs(2),
            max_artifact_bytes: 1024,
            max_retries: 2,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            max_jitter: Duration::ZERO,
            max_retry_after: Duration::ZERO,
        }
    }

    async fn run_download(
        downloader: &HttpsDownloader,
        artifact: &RuntimeArtifact,
        root: &std::path::Path,
        _trace_id: &str,
        cancellation: DownloadCancellation,
    ) -> Result<super::DownloadedArtifact, DownloadError> {
        let diagnostics = DiagnosticContext::noop(TraceKind::Update);
        downloader
            .download(
                DownloadRequest {
                    artifact,
                    updates_dir: root,
                    cancellation,
                },
                &diagnostics,
            )
            .await
    }

    #[test]
    fn production_downloader_rejects_non_https_urls() {
        let downloader = HttpsDownloader::new(DownloadPolicy::default()).expect("client");

        assert!(
            downloader
                .validate_url("http://127.0.0.1/artifact.zip")
                .is_err()
        );
        assert!(
            downloader
                .validate_url("https://updates.example/artifact.zip")
                .is_ok()
        );
        assert!(
            downloader
                .validate_url("https://user:password@updates.example/artifact.zip")
                .is_err()
        );
        assert!(
            downloader
                .validate_url("https://updates.example/artifact.zip#fragment")
                .is_err()
        );
    }

    #[tokio::test]
    async fn streams_valid_artifact_to_verified_file() {
        let body = b"verified-runtime";
        let (url, _) = spawn_server(vec![response("200 OK", body, Some(body.len() as u64))]);
        let artifact = artifact(url, body, None);
        let root = test_root("success");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let downloaded = run_download(
            &downloader,
            &artifact,
            &root,
            "trace_01",
            DownloadCancellation::default(),
        )
        .await
        .expect("verified download");

        assert_eq!(
            downloaded.verified_path.parent().and_then(Path::parent),
            Some(
                std::fs::canonicalize(&root)
                    .expect("canonical updates")
                    .as_path()
            )
        );
        assert_eq!(
            std::fs::read(downloaded.verified_path).expect("verified bytes"),
            body
        );
        assert_eq!(downloaded.size, 16);
        assert_eq!(downloaded.sha256, artifact.sha256);
        assert!(!root.join("trace_01/artifact.part").exists());
    }

    #[tokio::test]
    async fn accepts_missing_content_length_with_streamed_limits() {
        let body = b"no-length";
        let (url, _) = spawn_server(vec![response("200 OK", body, None)]);
        let artifact = artifact(url, body, None);
        let root = test_root("no-length");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        assert!(
            run_download(&downloader, &artifact, &root, "trace", Default::default())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rejects_declared_size_above_policy_before_writing() {
        let body = b"small";
        let (url, _) = spawn_server(vec![response("200 OK", body, Some(2048))]);
        let artifact = artifact(url, body, None);
        let root = test_root("declared-large");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("oversize must fail");

        assert!(matches!(error, DownloadError::SizeLimitExceeded));
        assert!(!root.join("trace/artifact.verified").exists());
    }

    #[tokio::test]
    async fn rejects_stream_that_crosses_policy_limit_without_length() {
        let body = vec![b'x'; 1025];
        let (url, _) = spawn_server(vec![response("200 OK", &body, None)]);
        let artifact = artifact(url, &body, Some(1000));
        let root = test_root("stream-large");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("stream limit must fail");

        assert!(matches!(error, DownloadError::SizeLimitExceeded));
        assert!(!root.join("trace/artifact.verified").exists());
    }

    #[tokio::test]
    async fn rejects_connection_closed_before_declared_body() {
        let body = b"short";
        let (url, attempts) = spawn_server(vec![response("200 OK", body, Some(99)); 3]);
        let artifact = artifact(url, body, Some(99));
        let root = test_root("disconnect");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("disconnect must fail");

        assert!(matches!(error, DownloadError::Network));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(!root.join("trace/artifact.verified").exists());
    }

    #[tokio::test]
    async fn rejects_digest_mismatch() {
        let body = b"actual";
        let (url, _) = spawn_server(vec![response("200 OK", body, Some(body.len() as u64))]);
        let mut artifact = artifact(url, body, None);
        artifact.sha256 = [7; 32];
        let root = test_root("digest");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("digest must fail");

        assert!(matches!(error, DownloadError::DigestMismatch));
        assert!(!root.join("trace/artifact.verified").exists());
    }

    #[tokio::test]
    async fn rejects_expected_size_mismatch() {
        let body = b"actual";
        let (url, _) = spawn_server(vec![response("200 OK", body, Some(body.len() as u64))]);
        let artifact = artifact(url, body, Some((body.len() + 1) as u64));
        let root = test_root("size");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("size must fail");

        assert!(matches!(error, DownloadError::SizeMismatch));
        assert!(!root.join("trace/artifact.verified").exists());
    }

    #[tokio::test]
    async fn retries_429_and_honors_capped_retry_after() {
        let body = b"ok";
        let (url, attempts) = spawn_server(vec![
            retry_response("429 Too Many Requests", Some("999")),
            response("200 OK", body, Some(2)),
        ]);
        let artifact = artifact(url, body, None);
        let root = test_root("429");
        let mut retry_policy = policy();
        retry_policy.initial_backoff = Duration::from_millis(200);
        retry_policy.max_backoff = Duration::from_millis(200);
        retry_policy.max_retry_after = Duration::from_millis(10);
        let downloader = HttpsDownloader::new_for_test(retry_policy).expect("test client");
        assert!(
            run_download(&downloader, &artifact, &root, "trace", Default::default())
                .await
                .is_ok()
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retry_after_is_deterministically_capped_before_backoff_selection() {
        assert_eq!(
            select_retry_delay(
                Some(Duration::from_secs(999)),
                Duration::from_millis(10),
                Duration::from_millis(200),
            ),
            Duration::from_millis(10),
        );
        assert_eq!(
            select_retry_delay(None, Duration::from_millis(10), Duration::from_millis(200)),
            Duration::from_millis(200),
        );
    }

    #[tokio::test]
    async fn retries_5xx_with_a_bounded_attempt_count() {
        let (url, attempts) = spawn_server(vec![
            retry_response("503 Service Unavailable", None),
            retry_response("503 Service Unavailable", None),
            retry_response("503 Service Unavailable", None),
            retry_response("503 Service Unavailable", None),
        ]);
        let artifact = artifact(url, b"unused", None);
        let root = test_root("5xx");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("retry budget must stop 5xx responses");

        assert!(matches!(error, DownloadError::HttpStatus { status: 503 }));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_404_or_expose_url_query() {
        let (url, attempts) = spawn_server(vec![retry_response("404 Not Found", None)]);
        let artifact = artifact(url, b"unused", None);
        let root = test_root("404");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("404 must fail");

        assert!(matches!(error, DownloadError::HttpStatus { status: 404 }));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn response_timeout_is_bounded_and_retryable() {
        let mut slow = retry_response("200 OK", None);
        slow.delay = Duration::from_millis(150);
        let (url, attempts) = spawn_server(vec![slow; 3]);
        let artifact = artifact(url, b"unused", None);
        let root = test_root("timeout");
        let mut timeout_policy = policy();
        timeout_policy.response_timeout = Duration::from_millis(20);
        let downloader = HttpsDownloader::new_for_test(timeout_policy).expect("test client");

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("timeout must fail");

        assert!(matches!(error, DownloadError::Timeout));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn total_timeout_caps_a_long_response_timeout() {
        let mut slow = retry_response("200 OK", None);
        slow.delay = Duration::from_millis(200);
        let (url, _) = spawn_server(vec![slow]);
        let artifact = artifact(url, b"unused", None);
        let root = test_root("total-timeout");
        let mut timeout_policy = policy();
        timeout_policy.response_timeout = Duration::from_secs(1);
        timeout_policy.total_timeout = Duration::from_millis(20);
        timeout_policy.max_retries = 0;
        let downloader = HttpsDownloader::new_for_test(timeout_policy).expect("test client");
        let started = std::time::Instant::now();

        let error = run_download(&downloader, &artifact, &root, "trace", Default::default())
            .await
            .expect_err("total timeout must fail");

        assert!(matches!(error, DownloadError::Timeout));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn cancellation_interrupts_response_wait() {
        let mut slow = retry_response("200 OK", None);
        slow.delay = Duration::from_secs(1);
        let (url, _) = spawn_server(vec![slow]);
        let artifact = artifact(url, b"unused", None);
        let root = test_root("cancel");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");
        let cancellation = DownloadCancellation::default();
        let cancel_from_task = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_from_task.cancel();
        });

        let error = run_download(&downloader, &artifact, &root, "trace", cancellation)
            .await
            .expect_err("cancel must fail");

        assert!(matches!(error, DownloadError::Cancelled));
    }

    #[tokio::test]
    async fn generated_trace_id_is_a_single_safe_path_segment() {
        let (url, _) = spawn_server(Vec::new());
        let artifact = artifact(url, b"unused", None);
        let root = test_root("trace");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");
        let diagnostics = DiagnosticContext::noop(TraceKind::Update);
        let trace_id = diagnostics.trace_str();
        assert!(super::validate_trace_id(trace_id).is_ok());
        assert_eq!(Path::new(trace_id).components().count(), 1);
        let _ = downloader
            .download(
                DownloadRequest {
                    artifact: &artifact,
                    updates_dir: &root,
                    cancellation: Default::default(),
                },
                &diagnostics,
            )
            .await;
    }

    #[tokio::test]
    async fn refuses_to_overwrite_existing_verified_artifact() {
        let body = b"new";
        let (url, attempts) = spawn_server(Vec::new());
        let artifact = artifact(url, body, None);
        let root = test_root("existing");
        let diagnostics = DiagnosticContext::noop(TraceKind::Update);
        let trace_dir = root.join(diagnostics.trace_str());
        std::fs::create_dir_all(&trace_dir).expect("trace dir");
        std::fs::write(trace_dir.join("artifact.verified"), b"old").expect("existing file");
        let downloader = HttpsDownloader::new_for_test(policy()).expect("test client");

        let error = downloader
            .download(
                DownloadRequest {
                    artifact: &artifact,
                    updates_dir: &root,
                    cancellation: Default::default(),
                },
                &diagnostics,
            )
            .await
            .expect_err("existing verified must fail");

        assert!(matches!(error, DownloadError::VerifiedAlreadyExists));
        assert_eq!(
            std::fs::read(trace_dir.join("artifact.verified")).expect("old bytes"),
            b"old"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn rejects_invalid_policy_without_panicking_on_extreme_duration() {
        fn assert_invalid_policy(expected_field: &'static str, policy: DownloadPolicy) {
            let error = HttpsDownloader::new(policy)
                .err()
                .expect("invalid policy must fail");
            assert!(matches!(
                error,
                DownloadError::InvalidPolicy { field } if field == expected_field
            ));
        }
        assert_invalid_policy(
            "connect_timeout",
            DownloadPolicy {
                connect_timeout: Duration::ZERO,
                ..DownloadPolicy::default()
            },
        );
        assert_invalid_policy(
            "response_timeout",
            DownloadPolicy {
                response_timeout: Duration::ZERO,
                ..DownloadPolicy::default()
            },
        );
        assert_invalid_policy(
            "total_timeout",
            DownloadPolicy {
                total_timeout: Duration::ZERO,
                ..DownloadPolicy::default()
            },
        );
        assert_invalid_policy(
            "max_artifact_bytes",
            DownloadPolicy {
                max_artifact_bytes: 0,
                ..DownloadPolicy::default()
            },
        );
        assert_invalid_policy(
            "initial_backoff",
            DownloadPolicy {
                initial_backoff: Duration::from_secs(2),
                max_backoff: Duration::from_secs(1),
                ..DownloadPolicy::default()
            },
        );

        let outcome = std::panic::catch_unwind(|| {
            HttpsDownloader::new(DownloadPolicy {
                total_timeout: Duration::MAX,
                ..DownloadPolicy::default()
            })
        });
        let error = outcome
            .expect("extreme duration must not panic")
            .err()
            .expect("extreme duration must fail");
        assert!(matches!(
            error,
            DownloadError::InvalidPolicy {
                field: "total_timeout"
            }
        ));
    }

    #[test]
    fn canonical_trace_boundary_rejects_link_target_outside_updates_root() {
        let canonical_updates = PathBuf::from(r"C:\Users\tester\AppData\Local\DSH\updates");
        let canonical_trace = PathBuf::from(r"C:\outside\trace");

        let error =
            super::validate_canonical_trace_path(&canonical_updates, &canonical_trace, "trace_01")
                .expect_err("outside canonical target must fail");

        assert!(matches!(error, DownloadError::UnsafeTraceDirectory));
        assert!(
            super::validate_canonical_trace_path(
                &canonical_updates,
                &canonical_updates.join("trace_01"),
                "trace_01",
            )
            .is_ok()
        );
    }
}
