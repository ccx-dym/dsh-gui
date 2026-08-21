use serde::Serialize;
use std::fmt;
use std::fs::OpenOptions as SyncOpenOptions;
use std::io;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

const LOG_PREFIX: &str = "diagnostics-";
const LOG_SUFFIX: &str = ".jsonl";
const MAX_LOG_SLOT_BYTES: u64 = 4 * 1024 * 1024;

/// 只允许安全关联标识进入诊断边界，避免正文、凭据、URL 与用户路径被误写。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DiagnosticTraceId(String);

impl DiagnosticTraceId {
    /// 校验并构造安全诊断标识。
    ///
    /// :param value: 预期为程序定义的 ASCII 标识符，而非外部错误正文。
    /// :return: 通过字符集、长度与敏感词门禁的值。
    /// :raises DiagnosticError: 值可能包含秘密、正文、URL 或路径时拒绝。
    pub fn parse(value: &str) -> Result<Self, DiagnosticError> {
        let lowered = value.to_ascii_lowercase();
        let suspicious = [
            "authorization",
            "bearer",
            "secret",
            "token",
            "api_key",
            "apikey",
            "password",
        ];
        let valid = !value.is_empty()
            && value.len() <= 96
            && value.is_ascii()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            && !suspicious.iter().any(|needle| lowered.contains(needle));
        if !valid {
            return Err(DiagnosticError::UnsafeMetadata);
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<&str> for DiagnosticTraceId {
    type Error = DiagnosticError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl fmt::Display for DiagnosticTraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 可记录的有限阶段白名单；外部错误正文无法伪装成阶段字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStage {
    CloseToTray,
    RuntimeFailed,
    RuntimeReady,
    RuntimeStart,
    RuntimeStopping,
    SingleInstanceFocus,
    SingleInstanceShow,
    SingleInstanceWindow,
    TrayExit,
    TrayHide,
    TrayOpen,
    TrayRestart,
    UpdateProbe,
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
}

/// 固定字段的本地诊断事件；类型上不提供 headers、env、body 或任意文本字段。
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
        trace_id: DiagnosticTraceId,
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
            trace_id,
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
    #[error("诊断元数据不符合安全标识规则")]
    UnsafeMetadata,
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
#[derive(Clone, Debug)]
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
}

/// 面向状态机的无失败诊断出口；满队列与磁盘错误只会丢弃诊断事件。
#[derive(Clone, Debug)]
pub struct DiagnosticSink {
    sender: mpsc::Sender<DiagnosticCommand>,
}

impl DiagnosticSink {
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
        tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    DiagnosticCommand::Event(event) => {
                        // 可观测性不能成为状态机依赖：任何写入错误都在边界内降级。
                        let _ = logger.write(&event).await;
                    }
                    DiagnosticCommand::Flush(complete) => {
                        let _ = complete.send(());
                    }
                }
            }
        });
        Ok(Self { sender })
    }

    /// 尽力排队一个类型化事件，队列满或 worker 已退出时立即丢弃。
    ///
    /// :param event: 不含任意文本字段的诊断事件。
    /// :return: 无返回值；诊断失败绝不传播给业务状态机。
    /// :raises: 此方法不产生错误且不阻塞。
    pub fn record(&self, event: DiagnosticEvent) {
        let _ = self.sender.try_send(DiagnosticCommand::Event(event));
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
}
