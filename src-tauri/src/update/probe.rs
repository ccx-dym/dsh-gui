use crate::paths::RuntimeLayout;
use crate::runtime::command::{RuntimeLaunchSpec, reserve_loopback_port};
use crate::runtime::health::{HealthProbe, ReadyProbe};
use crate::runtime::install_state::{DataGeneration, InstalledRuntime};
use crate::runtime::{ProcessLauncher, RuntimeError, RuntimeProcess, WindowsProcessLauncher};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use thiserror::Error;

const GENERATION_STATE_SCHEMA: u32 = 1;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;
const MAX_PROBE_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// 激活器已停止当前 DSH 后交给探活流程的只读门禁值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeStoppedState {
    ConfirmedStopped,
}

/// generation 在兼容 runtime 验证与后续激活中的持久化阶段。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GenerationState {
    Candidate,
    Probing,
    Passed,
    Failed,
    Active,
    Inactive,
}

/// 探活报告的最终阶段。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbePhase {
    Passed,
    Failed,
}

/// 不含进程输出、路径、URL 或用户数据的稳定失败类别。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeErrorKind {
    Cancelled,
    ReadinessTimeout,
    ProcessExitedEarly,
    InvalidWebUi,
    LaunchFailed,
    CleanupFailed,
    WorkerFailed,
    StateWriteFailed,
    CandidateRejected,
}

/// 可安全写入诊断日志的隔离探活报告。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeReport {
    pub version: String,
    pub phase: ProbePhase,
    pub elapsed_ms: u64,
    pub retry_count: u32,
    pub error_kind: Option<ProbeErrorKind>,
    pub trace_id: String,
}

/// 可跨异步任务请求中止探活等待的轻量令牌。
#[derive(Clone, Default)]
pub struct ProbeCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ProbeCancellation {
    /// 创建尚未取消的探活令牌。
    ///
    /// :return: 可克隆并在线程间共享的取消令牌。
    /// :raises: 此构造函数不产生错误。
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求尽快结束当前探活。
    ///
    /// :return: 原子取消标记设置完成时返回。
    /// :raises: 此原子操作不产生错误。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// 隔离探活的资源上限与等待策略。
#[derive(Clone, Copy, Debug)]
pub struct ProbePolicy {
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub stop_grace: Duration,
    pub max_files: u64,
    pub max_candidate_bytes: u64,
    pub required_free_bytes: u64,
}

impl Default for ProbePolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(45),
            poll_interval: Duration::from_millis(100),
            stop_grace: Duration::from_secs(2),
            max_files: 200_000,
            max_candidate_bytes: 8 * 1024 * 1024 * 1024,
            required_free_bytes: 512 * 1024 * 1024,
        }
    }
}

/// 提供目标卷可用空间；生产实现使用 Windows 卷信息，测试可注入确定值。
pub trait ProbeStorageInspector: Send + Sync {
    /// 查询包含指定 candidate 的卷对当前用户可用的字节数。
    ///
    /// :param path: 已验证的 candidate generation 目录。
    /// :return: 当前调用者可使用的可用空间字节数。
    /// :raises std::io::Error: 卷信息不可读取时返回，不包含 candidate 文件名。
    fn available_bytes(&self, path: &Path) -> Result<u64, std::io::Error>;
}

/// 通过 Windows 卷 API 查询当前用户可用空间的生产适配器。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProbeStorageInspector;

#[cfg(windows)]
impl ProbeStorageInspector for SystemProbeStorageInspector {
    fn available_bytes(&self, path: &Path) -> Result<u64, std::io::Error> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        use windows::core::PCWSTR;

        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        wide.push(0);
        let mut available = 0_u64;
        unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }
            .map_err(|error| std::io::Error::other(format!("HRESULT {:#010X}", error.code().0)))?;
        Ok(available)
    }
}

#[cfg(not(windows))]
impl ProbeStorageInspector for SystemProbeStorageInspector {
    fn available_bytes(&self, _path: &Path) -> Result<u64, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "仅支持 Windows 卷空间检查",
        ))
    }
}

/// activator 已准备好的不可变 runtime 与隔离 candidate generation。
#[derive(Clone, Debug)]
pub struct ProbeWorkspace {
    layout: RuntimeLayout,
    runtime: InstalledRuntime,
    node_version: Version,
    candidate: DataGeneration,
    active: Option<DataGeneration>,
    project_workspace: PathBuf,
}

impl ProbeWorkspace {
    /// 创建只引用既有 candidate 的探活工作区，不复制真实用户数据。
    ///
    /// :param layout: 固定 runtime/generation 根目录。
    /// :param runtime: 已安装且通过兼容清单验证的 runtime。
    /// :param node_version: runtime 包内受控 Node 版本。
    /// :param candidate: activator 已创建的一致数据 generation。
    /// :param active: 当前 active generation；首次安装时为 `None`。
    /// :param project_workspace: DSH 启动使用的既有工作区。
    /// :param stopped: AppController 确认当前 DSH 已停止的门禁值。
    /// :return: 尚未启动进程的隔离工作区值对象。
    /// :raises ProbeError: candidate 与 active 相同或任一必要目录不存在时返回。
    pub fn new(
        layout: RuntimeLayout,
        runtime: InstalledRuntime,
        node_version: Version,
        candidate: DataGeneration,
        active: Option<DataGeneration>,
        project_workspace: PathBuf,
        stopped: RuntimeStoppedState,
    ) -> Result<Self, ProbeError> {
        if stopped != RuntimeStoppedState::ConfirmedStopped {
            return Err(ProbeError::RuntimeNotStopped);
        }
        if active.as_ref().is_some_and(|value| value == &candidate) {
            return Err(ProbeError::CandidateIsActive);
        }
        if !layout.generation_root().is_absolute()
            || !layout.runtime_root().is_absolute()
            || !project_workspace.is_absolute()
        {
            return Err(ProbeError::UnsafeBoundary);
        }
        validate_plain_directory(layout.generation_root())?;
        validate_plain_directory(&layout.generation_dir(&candidate))?;
        validate_plain_directory(layout.runtime_root())?;
        validate_plain_directory(&layout.runtime_dir(&runtime))?;
        validate_plain_directory(&project_workspace)?;
        if let Some(active_generation) = &active {
            validate_plain_directory(&layout.generation_dir(active_generation))?;
        }
        Ok(Self {
            layout,
            runtime,
            node_version,
            candidate,
            active,
            project_workspace,
        })
    }
}

fn validate_plain_directory(path: &Path) -> Result<(), ProbeError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ProbeError::MissingWorkspace)?;
    if !metadata.is_dir() {
        return Err(ProbeError::MissingWorkspace);
    }
    if metadata.file_type().is_symlink() || has_reparse_point(&metadata) {
        return Err(ProbeError::UnsafeBoundary);
    }
    Ok(())
}

/// 探活参数、文件边界或状态持久化失败。
#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("探活策略无效: {field}")]
    InvalidPolicy { field: &'static str },
    #[error("trace_id 必须是安全的单段标识")]
    InvalidTraceId,
    #[error("当前 runtime 尚未确认停止")]
    RuntimeNotStopped,
    #[error("candidate generation 不能复用 active generation")]
    CandidateIsActive,
    #[error("探活工作区缺少必要目录")]
    MissingWorkspace,
    #[error("探活路径越出固定目录边界")]
    UnsafeBoundary,
    #[error("candidate generation 包含不允许的链接或重解析点")]
    UnsafeEntry,
    #[error("candidate generation 文件数量超过上限")]
    FileCountLimit,
    #[error("candidate generation 大小超过上限")]
    CandidateSizeLimit,
    #[error("candidate generation 所在卷可用空间不足")]
    InsufficientSpace,
    #[error("generation 状态不允许开始探活")]
    InvalidGenerationState,
    #[error("generation 状态写入失败")]
    StateWrite,
}

/// 使用同一受管进程/Job Object 能力执行隔离兼容探活。
pub struct RuntimeProbe {
    policy: ProbePolicy,
    launcher: Arc<dyn ProcessLauncher>,
    health: Arc<dyn ReadyProbe>,
    storage: Arc<dyn ProbeStorageInspector>,
}

impl RuntimeProbe {
    /// 创建使用 Windows Job Object、真实 HTTP 与系统卷空间检查的探活器。
    ///
    /// :param policy: 等待、清理和 candidate 资源上限。
    /// :return: 可异步执行隔离探活的对象。
    /// :raises ProbeError: 策略包含零值或不合理轮询间隔时返回。
    pub fn new(policy: ProbePolicy) -> Result<Self, ProbeError> {
        Self::with_dependencies(
            policy,
            Arc::new(WindowsProcessLauncher),
            Arc::new(HealthProbe),
            Arc::new(SystemProbeStorageInspector),
        )
    }

    /// 创建依赖可注入的探活器，供边界测试与生产适配器复用。
    ///
    /// :param policy: 资源与时间上限。
    /// :param launcher: 受管进程创建器。
    /// :param health: HTTP 200 探活器。
    /// :param storage: 卷可用空间检查器。
    /// :return: 校验完成的探活器。
    /// :raises ProbeError: 策略无效时返回。
    pub fn with_dependencies(
        policy: ProbePolicy,
        launcher: Arc<dyn ProcessLauncher>,
        health: Arc<dyn ReadyProbe>,
        storage: Arc<dyn ProbeStorageInspector>,
    ) -> Result<Self, ProbeError> {
        validate_policy(policy)?;
        Ok(Self {
            policy,
            launcher,
            health,
            storage,
        })
    }

    /// 在 blocking worker 中启动 candidate，完成 stdout+HTTP 双门禁后仍立即回收进程树。
    ///
    /// :param workspace: activator 已创建且与 active 隔离的工作区。
    /// :param trace_id: 仅含安全 ASCII 字符的关联标识。
    /// :param cancellation: 可中断短片轮询的取消令牌。
    /// :return: 只含稳定字段的成功或失败报告；预检失败返回结构化错误。
    /// :raises ProbeError: 路径/资源/状态预检失败，或 blocking worker 无法完成时返回。
    pub async fn probe(
        &self,
        workspace: ProbeWorkspace,
        trace_id: String,
        cancellation: ProbeCancellation,
    ) -> Result<ProbeReport, ProbeError> {
        validate_trace_id(&trace_id)?;
        let probe_started = Instant::now();
        let candidate_dir = validate_workspace(&workspace)?;
        ensure_initial_state(&candidate_dir, &trace_id)?;
        let preflight_dir = candidate_dir.clone();
        let preflight_policy = self.policy;
        let preflight_storage = Arc::clone(&self.storage);
        let preflight = tokio::task::spawn_blocking(move || {
            scan_candidate(&preflight_dir, preflight_policy).and_then(|()| {
                let available = preflight_storage
                    .available_bytes(&preflight_dir)
                    .map_err(|_| ProbeError::InsufficientSpace)?;
                if available < preflight_policy.required_free_bytes {
                    Err(ProbeError::InsufficientSpace)
                } else {
                    Ok(())
                }
            })
        })
        .await;
        if !matches!(&preflight, Ok(Ok(()))) {
            let error_kind =
                if write_generation_state(&candidate_dir, GenerationState::Failed, &trace_id)
                    .is_ok()
                {
                    if preflight.is_err() {
                        ProbeErrorKind::WorkerFailed
                    } else {
                        ProbeErrorKind::CandidateRejected
                    }
                } else {
                    ProbeErrorKind::StateWriteFailed
                };
            return Ok(ProbeReport {
                version: workspace.runtime.version.to_string(),
                phase: ProbePhase::Failed,
                elapsed_ms: probe_started.elapsed().as_millis() as u64,
                retry_count: 0,
                error_kind: Some(error_kind),
                trace_id,
            });
        }
        write_generation_state(&candidate_dir, GenerationState::Probing, &trace_id)?;

        let version = workspace.runtime.version.to_string();
        let policy = self.policy;
        let launcher = Arc::clone(&self.launcher);
        let health = Arc::clone(&self.health);
        let worker_workspace = workspace.clone();
        let worker_cancel = cancellation.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            run_probe_worker(&worker_workspace, policy, launcher, health, worker_cancel)
        })
        .await;

        let (phase, retry_count, error_kind, _worker_elapsed_ms) = match outcome {
            Ok(worker) => worker,
            Err(_) => (ProbePhase::Failed, 0, Some(ProbeErrorKind::WorkerFailed), 0),
        };
        let final_state = if phase == ProbePhase::Passed {
            GenerationState::Passed
        } else {
            GenerationState::Failed
        };
        let elapsed_ms = probe_started.elapsed().as_millis() as u64;
        if write_generation_state(&candidate_dir, final_state, &trace_id).is_err() {
            return Ok(ProbeReport {
                version,
                phase: ProbePhase::Failed,
                elapsed_ms,
                retry_count,
                error_kind: Some(ProbeErrorKind::StateWriteFailed),
                trace_id,
            });
        }
        Ok(ProbeReport {
            version,
            phase,
            elapsed_ms,
            retry_count,
            error_kind,
            trace_id,
        })
    }
}

fn validate_policy(policy: ProbePolicy) -> Result<(), ProbeError> {
    for (field, invalid) in [
        (
            "timeout",
            policy.timeout.is_zero() || policy.timeout > MAX_PROBE_DURATION,
        ),
        (
            "poll_interval",
            policy.poll_interval.is_zero() || policy.poll_interval > MAX_PROBE_DURATION,
        ),
        ("max_files", policy.max_files == 0),
        ("max_candidate_bytes", policy.max_candidate_bytes == 0),
        ("required_free_bytes", policy.required_free_bytes == 0),
    ] {
        if invalid {
            return Err(ProbeError::InvalidPolicy { field });
        }
    }
    if policy.poll_interval > policy.timeout {
        return Err(ProbeError::InvalidPolicy {
            field: "poll_interval",
        });
    }
    Ok(())
}

fn validate_trace_id(trace_id: &str) -> Result<(), ProbeError> {
    let valid = !trace_id.is_empty()
        && trace_id.len() <= 96
        && trace_id != "."
        && trace_id != ".."
        && trace_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ProbeError::InvalidTraceId)
    }
}

fn validate_workspace(workspace: &ProbeWorkspace) -> Result<PathBuf, ProbeError> {
    let generation_root = workspace
        .layout
        .generation_root()
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    let candidate = workspace
        .layout
        .generation_dir(&workspace.candidate)
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    if candidate.parent() != Some(generation_root.as_path()) {
        return Err(ProbeError::UnsafeBoundary);
    }
    if let Some(active) = &workspace.active {
        let active_path = workspace
            .layout
            .generation_dir(active)
            .canonicalize()
            .map_err(|_| ProbeError::UnsafeBoundary)?;
        if active_path == candidate {
            return Err(ProbeError::CandidateIsActive);
        }
    }
    let runtime_root = workspace
        .layout
        .runtime_root()
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    let runtime_dir = workspace
        .layout
        .runtime_dir(&workspace.runtime)
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    if runtime_dir.parent() != Some(runtime_root.as_path()) {
        return Err(ProbeError::UnsafeBoundary);
    }
    workspace
        .project_workspace
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    Ok(candidate)
}

fn ensure_initial_state(candidate: &Path, trace_id: &str) -> Result<(), ProbeError> {
    let probing = state_path(candidate, GenerationState::Probing);
    let passed = state_path(candidate, GenerationState::Passed);
    let failed = state_path(candidate, GenerationState::Failed);
    let active = state_path(candidate, GenerationState::Active);
    let inactive = state_path(candidate, GenerationState::Inactive);
    if probing.exists()
        || passed.exists()
        || failed.exists()
        || active.exists()
        || inactive.exists()
    {
        return Err(ProbeError::InvalidGenerationState);
    }
    let candidate_state = state_path(candidate, GenerationState::Candidate);
    if candidate_state.exists() {
        let bytes = fs::read(&candidate_state).map_err(|_| ProbeError::InvalidGenerationState)?;
        let document: GenerationStateDocument<'_> =
            serde_json::from_slice(&bytes).map_err(|_| ProbeError::InvalidGenerationState)?;
        if document.schema != GENERATION_STATE_SCHEMA
            || document.state != GenerationState::Candidate
            || validate_trace_id(document.trace_id).is_err()
        {
            return Err(ProbeError::InvalidGenerationState);
        }
    } else {
        write_generation_state(candidate, GenerationState::Candidate, trace_id)?;
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct GenerationStateDocument<'a> {
    schema: u32,
    state: GenerationState,
    trace_id: &'a str,
}

fn state_name(state: GenerationState) -> &'static str {
    match state {
        GenerationState::Candidate => "candidate",
        GenerationState::Probing => "probing",
        GenerationState::Passed => "passed",
        GenerationState::Failed => "failed",
        GenerationState::Active => "active",
        GenerationState::Inactive => "inactive",
    }
}

fn state_path(candidate: &Path, state: GenerationState) -> PathBuf {
    candidate.join(format!("generation-state-{}.json", state_name(state)))
}

/// 追加一个原子 generation 状态标记，供后续激活器沿用同一 schema。
pub(crate) fn write_generation_state(
    candidate: &Path,
    state: GenerationState,
    trace_id: &str,
) -> Result<(), ProbeError> {
    let destination = state_path(candidate, state);
    if destination.exists() {
        return Err(ProbeError::InvalidGenerationState);
    }
    let temporary = candidate.join(format!(
        ".generation-state-{}-{trace_id}.tmp",
        state_name(state)
    ));
    let bytes = serde_json::to_vec(&GenerationStateDocument {
        schema: GENERATION_STATE_SCHEMA,
        state,
        trace_id,
    })
    .map_err(|_| ProbeError::StateWrite)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|_| ProbeError::StateWrite)?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| ProbeError::StateWrite)?;
    drop(file);
    fs::rename(&temporary, &destination).map_err(|_| ProbeError::StateWrite)?;
    Ok(())
}

fn scan_candidate(candidate: &Path, policy: ProbePolicy) -> Result<(), ProbeError> {
    let mut stack = vec![candidate.to_path_buf()];
    let mut entries_seen = 0_u64;
    let mut bytes = 0_u64;
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory).map_err(|_| ProbeError::UnsafeBoundary)?;
        for entry in entries {
            let entry = entry.map_err(|_| ProbeError::UnsafeBoundary)?;
            let metadata =
                fs::symlink_metadata(entry.path()).map_err(|_| ProbeError::UnsafeEntry)?;
            // 空目录也消耗遍历资源，必须计入同一上限，避免目录扇出造成拒绝服务。
            entries_seen = entries_seen
                .checked_add(1)
                .ok_or(ProbeError::FileCountLimit)?;
            if entries_seen > policy.max_files {
                return Err(ProbeError::FileCountLimit);
            }
            if metadata.file_type().is_symlink() || has_reparse_point(&metadata) {
                return Err(ProbeError::UnsafeEntry);
            }
            let canonical = entry
                .path()
                .canonicalize()
                .map_err(|_| ProbeError::UnsafeBoundary)?;
            if !canonical.starts_with(candidate) {
                return Err(ProbeError::UnsafeBoundary);
            }
            if metadata.is_dir() {
                stack.push(canonical);
                continue;
            }
            if !metadata.is_file() || has_multiple_links(&canonical) {
                return Err(ProbeError::UnsafeEntry);
            }
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or(ProbeError::CandidateSizeLimit)?;
            if bytes > policy.max_candidate_bytes {
                return Err(ProbeError::CandidateSizeLimit);
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn has_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0
}

#[cfg(not(windows))]
fn has_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn has_multiple_links(path: &Path) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let Ok(file) = fs::File::open(path) else {
        return true;
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }.is_err()
        || information.nNumberOfLinks > 1
}

#[cfg(unix)]
fn has_multiple_links(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).map_or(true, |metadata| metadata.nlink() > 1)
}

#[cfg(not(any(windows, unix)))]
fn has_multiple_links(_path: &Path) -> bool {
    false
}

fn run_probe_worker(
    workspace: &ProbeWorkspace,
    policy: ProbePolicy,
    launcher: Arc<dyn ProcessLauncher>,
    health: Arc<dyn ReadyProbe>,
    cancellation: ProbeCancellation,
) -> (ProbePhase, u32, Option<ProbeErrorKind>, u64) {
    let started = Instant::now();
    let result = build_launch_spec(workspace)
        .and_then(|spec| launcher.spawn(&spec).map(|child| (spec, child)));
    let Ok((spec, mut child)) = result else {
        return failed(started, 0, ProbeErrorKind::LaunchFailed);
    };

    let outcome = wait_for_both_gates(
        child.as_mut(),
        spec.loopback_port.expect("official spec always has port"),
        policy,
        health.as_ref(),
        &cancellation,
    );
    // 无论探活结果如何，只调用一次 stop；Job Object 会回收整个后代树。
    let cleanup = child.stop(policy.stop_grace);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if cleanup.is_err() {
        return (
            ProbePhase::Failed,
            outcome.1,
            Some(ProbeErrorKind::CleanupFailed),
            elapsed_ms,
        );
    }
    match outcome.0 {
        None => (ProbePhase::Passed, outcome.1, None, elapsed_ms),
        Some(error) => (ProbePhase::Failed, outcome.1, Some(error), elapsed_ms),
    }
}

fn build_launch_spec(workspace: &ProbeWorkspace) -> Result<RuntimeLaunchSpec, RuntimeError> {
    let runtime_dir = workspace.layout.runtime_dir(&workspace.runtime);
    let node = runtime_dir
        .join(format!("node-v{}-win-x64", workspace.node_version))
        .join("node.exe");
    let cli = runtime_dir.join("app/node_modules/@deepseek-ai/dsh/lib/bin.js");
    let candidate = workspace.layout.generation_dir(&workspace.candidate);
    let port = reserve_loopback_port()?;
    RuntimeLaunchSpec::official(
        runtime_dir,
        node,
        cli,
        workspace.project_workspace.clone(),
        candidate,
        port,
    )
}

fn wait_for_both_gates(
    child: &mut dyn RuntimeProcess,
    port: u16,
    policy: ProbePolicy,
    health: &dyn ReadyProbe,
    cancellation: &ProbeCancellation,
) -> (Option<ProbeErrorKind>, u32) {
    let started = Instant::now();
    let mut stdout_ready = false;
    let mut retries = 0_u32;
    loop {
        if cancellation.is_cancelled() {
            return (Some(ProbeErrorKind::Cancelled), retries);
        }
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => {
                return (Some(ProbeErrorKind::ProcessExitedEarly), retries);
            }
            Ok(None) => {}
        }
        let remaining = policy.timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            let kind = if stdout_ready {
                ProbeErrorKind::InvalidWebUi
            } else {
                ProbeErrorKind::ReadinessTimeout
            };
            return (Some(kind), retries);
        }
        let slice = remaining.min(policy.poll_interval);
        if !stdout_ready {
            if child.wait_for_readiness(port, slice).is_ok() {
                stdout_ready = true;
            }
            continue;
        }
        match health.wait_until_ready(port, slice) {
            Ok(_) => return (None, retries),
            Err(_) => retries = retries.saturating_add(1),
        }
    }
}

fn failed(
    started: Instant,
    retries: u32,
    kind: ProbeErrorKind,
) -> (ProbePhase, u32, Option<ProbeErrorKind>, u64) {
    (
        ProbePhase::Failed,
        retries,
        Some(kind),
        started.elapsed().as_millis() as u64,
    )
}
