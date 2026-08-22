use serde::{Deserialize, Serialize};
use std::fs::OpenOptions as SyncOpenOptions;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

const LOG_PREFIX: &str = "diagnostics-";
const LOG_SUFFIX: &str = ".jsonl";
const MAX_LOG_SLOT_BYTES: u64 = 4 * 1024 * 1024;

static TRACE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// 由程序内部生成的关联标识；没有接收调用方字符串的公共构造入口。
///
/// ```compile_fail
/// use dsh_desktop_lib::diagnostics::DiagnosticTraceId;
/// let _ = DiagnosticTraceId::parse("sk-proj-user-controlled");
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
struct DiagnosticTraceId(String);

/// 关联标识的程序定义域；前缀不能由网络或用户输入控制。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceKind {
    Activation,
    Runtime,
    Update,
}

impl TraceKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Activation => "activation",
            Self::Runtime => "runtime",
            Self::Update => "update",
        }
    }
}

/// 在同一更新或运行时操作的所有阶段间传递的类型化 trace。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationTrace {
    id: DiagnosticTraceId,
}

impl OperationTrace {
    /// 生成只由固定前缀、时间熵、进程 ID 与单调序号组成的新 trace。
    ///
    /// :param kind: 程序定义的操作类别。
    /// :return: 不接受任意字符串输入的关联标识。
    /// :raises: 系统时间异常时使用零时间熵，仍不会引入外部正文。
    pub fn begin(kind: TraceKind) -> Self {
        let time_entropy = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let sequence = TRACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            id: DiagnosticTraceId(format!(
                "{}-{:x}-{time_entropy:x}-{sequence:x}",
                kind.prefix(),
                std::process::id()
            )),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.id.0
    }
}

/// 可记录的有限阶段白名单；外部错误正文无法伪装成阶段字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStage {
    ActivationCommit,
    ActivationPrepare,
    ActivationRecovery,
    ActivationRollback,
    ArchiveInstall,
    CloseToTray,
    DownloadAttempt,
    DownloadComplete,
    ManifestVerify,
    OfficialCheck,
    CompatibilityCheck,
    ProbeComplete,
    ProbeStart,
    RuntimeFailed,
    RuntimeReady,
    RuntimeStart,
    RuntimeStopping,
    SnapshotPrepare,
    SingleInstanceFocus,
    SingleInstanceShow,
    SingleInstanceWindow,
    SkinApply,
    TrayExit,
    TrayHide,
    TrayOpen,
    TrayRestart,
    UpdateProbe,
    UpdateCheck,
}

/// 可记录的有限错误类别白名单，不携带底层 source 或动态上下文。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticErrorKind {
    ActivationBusy,
    AlreadyRunning,
    DeploymentChanged,
    HealthTimeout,
    InvalidLaunchPath,
    InvalidLoopbackPort,
    InvalidUrl,
    IoError,
    MainWindowMissing,
    MissingLoopbackPort,
    MockRuntimeDisabled,
    OutputReadinessTimeout,
    ProbeOperationInProgress,
    ProbeRequiresStoppedRuntime,
    ProcessError,
    ProcessExitedEarly,
    RuntimeFailure,
    StartupAbortTimeout,
    StartupAbortUnavailable,
    StartupAborted,
    StatePoisoned,
    TauriError,
    UpdateFailure,
}

/// 固定字段的本地诊断事件；类型上不提供 headers、env、body 或任意文本字段。
///
/// 外部 JSON 不能反序列化为生产事件，因此恢复文件也无法成为任意事件注入入口。
///
/// ```compile_fail
/// use dsh_desktop_lib::diagnostics::DiagnosticEvent;
/// let _: DiagnosticEvent = serde_json::from_str(
///     r#"{"elapsed_ms":0,"retry":0,"stage":"runtime_start","trace_id":"runtime-sk-proj-secret"}"#,
/// ).unwrap();
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticEvent {
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<DiagnosticErrorKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    retry: u32,
    stage: DiagnosticStage,
    trace_id: DiagnosticTraceId,
}

/// 仅用于校验已有 JSONL 的私有 DTO；它不会转换成 `DiagnosticEvent` 或进入 sink。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedDiagnosticEvent {
    #[serde(rename = "elapsed_ms")]
    _elapsed_ms: u64,
    #[serde(default)]
    error_kind: Option<String>,
    #[serde(default)]
    #[serde(rename = "pid")]
    _pid: Option<u32>,
    #[serde(rename = "retry")]
    _retry: u32,
    stage: String,
    trace_id: String,
}

impl PersistedDiagnosticEvent {
    fn is_valid(&self) -> bool {
        is_generated_trace_id(&self.trace_id)
            && is_persisted_stage(&self.stage)
            && self
                .error_kind
                .as_deref()
                .is_none_or(is_persisted_error_kind)
    }
}

impl DiagnosticEvent {
    /// 从稳定标识符构造固定 schema 事件。
    ///
    /// :param trace_id: 已校验的本次本地操作关联标识。
    /// :param stage: 类型化阶段码。
    /// :param elapsed_ms: 该阶段已耗时毫秒数。
    /// :param retry: 当前重试次数。
    /// :param pid: 可选的本地进程 ID。
    /// :param error_kind: 可选的类型化错误类别。
    /// :return: 只能由安全字段组成的诊断事件。
    /// :raises: 此构造器只接收已校验或有限枚举，不产生错误。
    pub fn new(
        trace: &OperationTrace,
        stage: DiagnosticStage,
        elapsed_ms: u64,
        retry: u32,
        pid: Option<u32>,
        error_kind: Option<DiagnosticErrorKind>,
    ) -> Self {
        Self {
            elapsed_ms,
            error_kind,
            pid,
            retry,
            stage,
            trace_id: trace.id.clone(),
        }
    }
}

/// 本地文件诊断的固定容量策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticPolicy {
    pub max_file_bytes: u64,
    pub slot_count: usize,
}

impl Default for DiagnosticPolicy {
    fn default() -> Self {
        Self {
            max_file_bytes: 256 * 1024,
            slot_count: 3,
        }
    }
}

#[derive(Debug, Error)]
pub enum DiagnosticError {
    #[error("诊断日志策略无效")]
    InvalidPolicy,
    #[error("诊断日志事件超过单文件容量")]
    EventTooLarge,
    #[error("诊断日志 I/O 失败")]
    Io,
    #[error("诊断事件序列化失败")]
    Serialize,
}

#[derive(Debug)]
struct LoggerState {
    slot: usize,
}

/// 写入固定数量日志槽位的异步本地诊断器。
#[derive(Clone)]
pub struct DiagnosticLogger {
    directory: PathBuf,
    policy: DiagnosticPolicy,
    state: Arc<Mutex<LoggerState>>,
}

impl DiagnosticLogger {
    /// 创建尚未触碰文件系统的诊断器。
    ///
    /// :param directory: 仅由应用路径系统提供的日志目录。
    /// :param policy: 单槽大小与固定槽位数量。
    /// :return: 可在线程间克隆的异步诊断器。
    /// :raises DiagnosticError: 容量为零或槽位数量不在安全范围时返回。
    pub fn new(directory: PathBuf, policy: DiagnosticPolicy) -> Result<Self, DiagnosticError> {
        if policy.max_file_bytes == 0
            || policy.max_file_bytes > MAX_LOG_SLOT_BYTES
            || policy.slot_count == 0
            || policy.slot_count > 16
        {
            return Err(DiagnosticError::InvalidPolicy);
        }
        Ok(Self {
            directory,
            policy,
            state: Arc::new(Mutex::new(LoggerState { slot: 0 })),
        })
    }

    /// 将一个已脱敏事件追加到有界固定槽位。
    ///
    /// 写满后截断下一个受控槽位，不生成带时间戳的无界文件，也不调用删除 API。
    /// 调用方应把错误作为可丢弃的可观测性故障处理，不能让它改变更新状态机。
    ///
    /// :param event: 只含固定安全元数据的事件。
    /// :return: 写入并 flush 成功时返回 `Ok(())`。
    /// :raises DiagnosticError: 目录、序列化或文件写入失败时返回，不会 panic。
    pub async fn write(&self, event: &DiagnosticEvent) -> Result<(), DiagnosticError> {
        let mut line = serde_json::to_vec(event).map_err(|_| DiagnosticError::Serialize)?;
        line.push(b'\n');
        if line.len() as u64 > self.policy.max_file_bytes {
            return Err(DiagnosticError::EventTooLarge);
        }

        let mut state = self.state.lock().await;
        fs::create_dir_all(&self.directory)
            .await
            .map_err(|_| DiagnosticError::Io)?;
        let scan_directory = self.directory.clone();
        let scan_policy = self.policy;
        tokio::task::spawn_blocking(move || reclaim_oversized_slots(&scan_directory, scan_policy))
            .await
            .map_err(|_| DiagnosticError::Io)?
            .map_err(|_| DiagnosticError::Io)?;
        let current = self.slot_path(state.slot);
        let current_size = match fs::metadata(&current).await {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(_) => return Err(DiagnosticError::Io),
        };
        let rotate = current_size.saturating_add(line.len() as u64) > self.policy.max_file_bytes;
        if rotate {
            state.slot = (state.slot + 1) % self.policy.slot_count;
        }

        let directory = self.directory.clone();
        let slot = self.slot_path(state.slot);
        tokio::task::spawn_blocking(move || write_slot(&directory, &slot, &line, rotate))
            .await
            .map_err(|_| DiagnosticError::Io)?
            .map_err(|_| DiagnosticError::Io)
    }

    fn slot_path(&self, slot: usize) -> PathBuf {
        self.directory
            .join(format!("{LOG_PREFIX}{slot}{LOG_SUFFIX}"))
    }
}

fn reclaim_oversized_slots(
    directory: &std::path::Path,
    policy: DiagnosticPolicy,
) -> io::Result<()> {
    let _directory_guard = open_directory_guard(directory)?;
    for slot in 0..policy.slot_count {
        let path = directory.join(format!("{LOG_PREFIX}{slot}{LOG_SUFFIX}"));
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let mut file = open_validated_slot(&path)?;
                if file.metadata()?.len() > policy.max_file_bytes {
                    // 超限旧槽位可能含截断 JSONL；验证完成后在原句柄内收敛，不删除。
                    file.set_len(0)?;
                    file.sync_data()?;
                } else {
                    repair_jsonl_prefix(&mut file)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn repair_jsonl_prefix(file: &mut std::fs::File) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut valid_end = 0_usize;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let document = &line[..line.len() - 1];
        let valid = serde_json::from_slice::<PersistedDiagnosticEvent>(document)
            .is_ok_and(|event| event.is_valid());
        if !valid {
            break;
        }
        valid_end += line.len();
    }
    if valid_end != bytes.len() {
        // 仅在已验证的同一句柄上截到首个坏行，保留此前完整事件且不调用删除 API。
        file.set_len(valid_end as u64)?;
        file.sync_data()?;
    }
    Ok(())
}

fn is_generated_trace_id(value: &str) -> bool {
    let mut parts = value.split('-');
    let kind = parts.next();
    let pid = parts.next();
    let time_entropy = parts.next();
    let sequence = parts.next();
    parts.next().is_none()
        && matches!(kind, Some("activation" | "runtime" | "update"))
        && pid.is_some_and(|part| u32::from_str_radix(part, 16).is_ok())
        && time_entropy.is_some_and(|part| u128::from_str_radix(part, 16).is_ok())
        && sequence.is_some_and(|part| u64::from_str_radix(part, 16).is_ok())
}

fn is_persisted_stage(value: &str) -> bool {
    matches!(
        value,
        "activation_commit"
            | "activation_prepare"
            | "activation_recovery"
            | "activation_rollback"
            | "archive_install"
            | "close_to_tray"
            | "download_attempt"
            | "download_complete"
            | "manifest_verify"
            | "official_check"
            | "compatibility_check"
            | "probe_complete"
            | "probe_start"
            | "runtime_failed"
            | "runtime_ready"
            | "runtime_start"
            | "runtime_stopping"
            | "snapshot_prepare"
            | "single_instance_focus"
            | "single_instance_show"
            | "single_instance_window"
            | "skin_apply"
            | "tray_exit"
            | "tray_hide"
            | "tray_open"
            | "tray_restart"
            | "update_probe"
            | "update_check"
    )
}

fn is_persisted_error_kind(value: &str) -> bool {
    matches!(
        value,
        "activation_busy"
            | "already_running"
            | "deployment_changed"
            | "health_timeout"
            | "invalid_launch_path"
            | "invalid_loopback_port"
            | "invalid_url"
            | "io_error"
            | "main_window_missing"
            | "missing_loopback_port"
            | "mock_runtime_disabled"
            | "output_readiness_timeout"
            | "probe_operation_in_progress"
            | "probe_requires_stopped_runtime"
            | "process_error"
            | "process_exited_early"
            | "runtime_failure"
            | "startup_abort_timeout"
            | "startup_abort_unavailable"
            | "startup_aborted"
            | "state_poisoned"
            | "tauri_error"
            | "update_failure"
    )
}

fn write_slot(
    directory: &std::path::Path,
    slot: &std::path::Path,
    line: &[u8],
    rotate: bool,
) -> io::Result<()> {
    let _directory_guard = open_directory_guard(directory)?;
    let mut file = open_validated_slot(slot)?;
    if rotate {
        // 验证句柄指向普通、单链接、私有文件后才允许复用固定槽位。
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
    } else {
        file.seek(SeekFrom::End(0))?;
    }
    file.write_all(line)?;
    file.flush()
}

#[cfg(windows)]
fn open_directory_guard(directory: &std::path::Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };

    crate::update::probe::ensure_private_windows_dacl(directory)?;
    let guard = SyncOpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(directory)?;
    validate_windows_handle(&guard, true)?;
    Ok(guard)
}

#[cfg(not(windows))]
fn open_directory_guard(directory: &std::path::Path) -> io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "诊断目录必须是普通目录",
        ));
    }
    std::fs::File::open(directory)
}

#[cfg(windows)]
fn open_validated_slot(slot: &std::path::Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    let mut options = SyncOpenOptions::new();
    options
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    let file = match options.open(slot) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut create = SyncOpenOptions::new();
            create
                .read(true)
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ.0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
            create.open(slot)?
        }
        Err(error) => return Err(error),
    };
    validate_windows_handle(&file, false)?;
    crate::update::probe::ensure_private_windows_dacl(slot)?;
    Ok(file)
}

#[cfg(not(windows))]
fn open_validated_slot(slot: &std::path::Path) -> io::Result<std::fs::File> {
    let mut options = SyncOpenOptions::new();
    options.read(true).write(true);
    let file = match options.open(slot) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => SyncOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(slot)?,
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "诊断槽位必须是普通文件",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn validate_windows_handle(file: &std::fs::File, expect_directory: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(io::Error::other)?;
    let attributes = information.dwFileAttributes;
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if is_directory != expect_directory
        || attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || (!expect_directory && information.nNumberOfLinks != 1)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "诊断路径安全属性无效",
        ));
    }
    Ok(())
}

enum DiagnosticCommand {
    Event(DiagnosticEvent),
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

/// 更新与运行时状态机使用的无失败诊断边界。
pub trait DiagnosticSink: Send + Sync + 'static {
    /// 尽力记录一个完全类型化的事件。
    ///
    /// :param event: 不含任意文本字段的诊断事件。
    /// :return: 无返回值；实现必须吸收队列、锁和 I/O 故障。
    /// :raises: 此接口禁止传播错误。
    fn record(&self, event: DiagnosticEvent);
}

/// 在完整更新操作中共享同一 trace 与无失败 sink 的上下文。
#[derive(Clone)]
pub struct DiagnosticContext {
    trace: OperationTrace,
    sink: Arc<dyn DiagnosticSink>,
}

impl DiagnosticContext {
    /// 为一条生产操作链创建类型化诊断上下文。
    ///
    /// :param kind: 固定 trace 类别。
    /// :param sink: 本地文件 sink 或 no-op 实现。
    /// :return: 可跨 download/probe/activation 阶段克隆的上下文。
    /// :raises: 此构造器不接受字符串且不产生错误。
    pub fn begin(kind: TraceKind, sink: Arc<dyn DiagnosticSink>) -> Self {
        Self {
            trace: OperationTrace::begin(kind),
            sink,
        }
    }

    /// 创建未配置日志时的安全默认上下文。
    ///
    /// :param kind: 固定 trace 类别。
    /// :return: 使用 no-op sink 的可共享上下文。
    /// :raises: 此构造器不产生错误。
    pub fn noop(kind: TraceKind) -> Self {
        Self::begin(kind, Arc::new(NoopDiagnosticSink))
    }

    /// 记录一个固定阶段；调用方不能传入 message、path、URL 或错误 source。
    pub fn record(
        &self,
        stage: DiagnosticStage,
        elapsed_ms: u64,
        retry: u32,
        pid: Option<u32>,
        error_kind: Option<DiagnosticErrorKind>,
    ) {
        self.sink.record(DiagnosticEvent::new(
            &self.trace,
            stage,
            elapsed_ms,
            retry,
            pid,
            error_kind,
        ));
    }

    pub(crate) fn trace_str(&self) -> &str {
        self.trace.as_str()
    }
}

/// 未配置本地日志时使用的无副作用默认实现。
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopDiagnosticSink;

impl DiagnosticSink for NoopDiagnosticSink {
    fn record(&self, _event: DiagnosticEvent) {}
}

/// 面向桌面应用的有界文件诊断出口；满队列与磁盘错误只会丢弃事件。
#[derive(Clone)]
pub struct FileDiagnosticSink {
    sender: mpsc::Sender<DiagnosticCommand>,
}

impl FileDiagnosticSink {
    /// 创建有界队列并启动单一异步写入 worker。
    ///
    /// :param logger: 已配置固定槽位上限的本地 writer。
    /// :param queue_capacity: 内存中允许等待的事件数量。
    /// :return: 可同步、无失败调用的诊断出口。
    /// :raises DiagnosticError: 队列容量为零或过大时返回。
    pub fn new(logger: DiagnosticLogger, queue_capacity: usize) -> Result<Self, DiagnosticError> {
        if queue_capacity == 0 || queue_capacity > 4096 {
            return Err(DiagnosticError::InvalidPolicy);
        }
        let (sender, mut receiver) = mpsc::channel(queue_capacity);
        // Tauri 的 setup 回调运行在 GUI 事件线程，并不位于“当前 Tokio reactor”内；
        // 使用 Tauri 全局异步运行时才能同时覆盖同步 setup 与异步测试调用点。
        tauri::async_runtime::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    DiagnosticCommand::Event(event) => {
                        // 可观测性不能成为状态机依赖：任何写入错误都在边界内降级。
                        let _ = logger.write(&event).await;
                    }
                    DiagnosticCommand::Flush(complete) => {
                        let _ = complete.send(());
                    }
                    DiagnosticCommand::Shutdown(complete) => {
                        let _ = complete.send(());
                        break;
                    }
                }
            }
        });
        Ok(Self { sender })
    }

    /// 等待调用前已成功入队的事件完成处理，主要用于测试与受控退出。
    ///
    /// :return: worker 可用时在先前事件处理后返回；worker 退出时直接返回。
    /// :raises: 此方法吸收 channel 关闭，不产生错误。
    pub async fn flush(&self) {
        let (complete, completed) = oneshot::channel();
        if self
            .sender
            .send(DiagnosticCommand::Flush(complete))
            .await
            .is_ok()
        {
            let _ = completed.await;
        }
    }

    /// 有序停止 writer；关闭后所有 `record` 调用都由 channel 边界静默丢弃。
    ///
    /// :return: 先前已入队命令处理完且 worker 退出后返回。
    /// :raises: channel 已关闭时直接返回，不传播错误。
    pub async fn shutdown(&self) {
        let (complete, completed) = oneshot::channel();
        if self
            .sender
            .send(DiagnosticCommand::Shutdown(complete))
            .await
            .is_ok()
        {
            let _ = completed.await;
        }
    }
}

impl DiagnosticSink for FileDiagnosticSink {
    fn record(&self, event: DiagnosticEvent) {
        let _ = self.sender.try_send(DiagnosticCommand::Event(event));
    }
}
