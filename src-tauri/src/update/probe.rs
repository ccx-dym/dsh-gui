use crate::app_controller::{ProbeExecutionPermit, ProbeLease};
use crate::paths::RuntimeLayout;
use crate::runtime::command::{RuntimeLaunchSpec, reserve_loopback_port};
use crate::runtime::health::{HealthProbe, ReadyProbe};
use crate::runtime::install_state::{
    DataGeneration, InstallStateError, InstallStateStore, InstalledRuntime,
};
use crate::runtime::{ProcessLauncher, RuntimeError, RuntimeProcess, WindowsProcessLauncher};
use crate::update::archive::{
    ArchiveInstallError, ArchiveInstallPolicy, verify_installed_runtime_inventory,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use thiserror::Error;

const GENERATION_STATE_SCHEMA: u32 = 1;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;
const MAX_PROBE_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
    RuntimeIntegrityFailed,
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

/// 校验 candidate 及敏感配置文件的 Windows DACL 是否只允许可信主体写入。
pub trait ProbePermissionInspector: Send + Sync {
    /// 检查路径的敏感写权限。
    ///
    /// :param path: 已验证位于 candidate 内的目录或敏感文件。
    /// :return: 未向宽泛主体授予写/修改权限时返回。
    /// :raises std::io::Error: 无法读取安全描述符或发现不安全 ACL 时返回。
    fn ensure_private(&self, path: &Path) -> Result<(), std::io::Error>;
}

/// 使用 Windows DACL 的生产权限检查器。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProbePermissionInspector;

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

#[cfg(windows)]
impl ProbePermissionInspector for SystemProbePermissionInspector {
    fn ensure_private(&self, path: &Path) -> Result<(), std::io::Error> {
        ensure_private_windows_dacl(path)
    }
}

#[cfg(not(windows))]
impl ProbePermissionInspector for SystemProbePermissionInspector {
    fn ensure_private(&self, _path: &Path) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "仅支持 Windows DACL 检查",
        ))
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
#[derive(Debug)]
pub struct ProbeWorkspace {
    layout: RuntimeLayout,
    runtime: InstalledRuntime,
    node_version: Version,
    candidate: DataGeneration,
    active: Option<DataGeneration>,
    project_workspace: PathBuf,
    _permit: ProbeExecutionPermit,
}

impl ProbeWorkspace {
    /// 创建只引用既有 candidate 的探活工作区，不复制真实用户数据。
    ///
    /// :param layout: 固定 runtime/generation 根目录。
    /// :param runtime: 已安装且通过兼容清单验证的 runtime。
    /// :param node_version: runtime 包内受控 Node 版本。
    /// :param candidate: activator 已创建的一致数据 generation。
    /// :param project_workspace: DSH 启动使用的既有工作区。
    /// :param lease: AppController 原子签发并覆盖整个探活生命周期的独占 lease。
    /// :return: 尚未启动进程的隔离工作区值对象。
    /// :raises ProbeError: candidate 与 active 相同或任一必要目录不存在时返回。
    pub fn new(
        layout: RuntimeLayout,
        runtime: InstalledRuntime,
        node_version: Version,
        candidate: DataGeneration,
        project_workspace: PathBuf,
        lease: &ProbeLease,
    ) -> Result<Self, ProbeError> {
        if runtime.node_version != node_version {
            return Err(ProbeError::RuntimeDescriptorMismatch);
        }
        let permit = lease
            .claim_probe()
            .map_err(|_| ProbeError::ProbeAlreadyActive)?;
        let active = load_active_generation(&layout)?;
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
        let project_workspace = project_workspace
            .canonicalize()
            .map_err(|_| ProbeError::UnsafeBoundary)?;
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
            _permit: permit,
        })
    }
}

fn load_active_generation(layout: &RuntimeLayout) -> Result<Option<DataGeneration>, ProbeError> {
    match InstallStateStore::new(layout.clone()).load() {
        Ok(deployment) => Ok(Some(deployment.data)),
        Err(InstallStateError::NotInstalled) => Ok(None),
        Err(_) => Err(ProbeError::InvalidActiveDeployment),
    }
}

fn validate_active_snapshot(workspace: &ProbeWorkspace) -> Result<(), ProbeError> {
    let current = load_active_generation(&workspace.layout)?;
    if current != workspace.active {
        return Err(ProbeError::InvalidActiveDeployment);
    }
    if current.as_ref() == Some(&workspace.candidate) {
        return Err(ProbeError::CandidateIsActive);
    }
    Ok(())
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
    #[error("当前激活指针无法可信读取")]
    InvalidActiveDeployment,
    #[error("runtime descriptor 与 probe 的 Node 版本不一致")]
    RuntimeDescriptorMismatch,
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
    #[error("candidate generation 的敏感权限不安全")]
    UnsafePermissions,
    #[error("探活总截止时间已到")]
    DeadlineExceeded,
    #[error("探活已取消")]
    Cancelled,
    #[error("同一个 lease 已有探活正在执行")]
    ProbeAlreadyActive,
}

/// 使用同一受管进程/Job Object 能力执行隔离兼容探活。
pub struct RuntimeProbe {
    policy: ProbePolicy,
    launcher: Arc<dyn ProcessLauncher>,
    health: Arc<dyn ReadyProbe>,
    storage: Arc<dyn ProbeStorageInspector>,
    permissions: Arc<dyn ProbePermissionInspector>,
}

impl RuntimeProbe {
    /// 创建使用 Windows Job Object、真实 HTTP 与系统卷空间检查的探活器。
    ///
    /// :param policy: 等待、清理和 candidate 资源上限。
    /// :return: 可异步执行隔离探活的对象。
    /// :raises ProbeError: 策略包含零值或不合理轮询间隔时返回。
    pub fn new(policy: ProbePolicy) -> Result<Self, ProbeError> {
        Self::with_inspectors(
            policy,
            Arc::new(WindowsProcessLauncher),
            Arc::new(HealthProbe),
            Arc::new(SystemProbeStorageInspector),
            Arc::new(SystemProbePermissionInspector),
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
        Self::with_inspectors(
            policy,
            launcher,
            health,
            storage,
            Arc::new(SystemProbePermissionInspector),
        )
    }

    /// 创建同时可注入卷与权限检查器的探活器。
    ///
    /// :param policy: 资源、总截止时间与轮询上限。
    /// :param launcher: 受管进程创建器。
    /// :param health: HTTP 就绪检查器。
    /// :param storage: candidate 卷空间检查器。
    /// :param permissions: candidate 与敏感文件 DACL 检查器。
    /// :return: 校验完成的探活器。
    /// :raises ProbeError: 策略包含零值或越界参数时返回。
    pub fn with_inspectors(
        policy: ProbePolicy,
        launcher: Arc<dyn ProcessLauncher>,
        health: Arc<dyn ReadyProbe>,
        storage: Arc<dyn ProbeStorageInspector>,
        permissions: Arc<dyn ProbePermissionInspector>,
    ) -> Result<Self, ProbeError> {
        validate_policy(policy)?;
        Ok(Self {
            policy,
            launcher,
            health,
            storage,
            permissions,
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
        let _execution_permit = workspace._permit.clone();
        let probe_started = Instant::now();
        let deadline = probe_started
            .checked_add(self.policy.timeout)
            .ok_or(ProbeError::InvalidPolicy { field: "timeout" })?;
        let boundaries = Arc::new(validate_workspace(&workspace)?);
        let candidate_dir = boundaries.candidate.clone();
        let state_binding = GenerationStateBinding::new(&workspace, &trace_id);
        let state_boundaries = Arc::new(ensure_initial_state(
            &workspace.layout,
            &workspace.candidate,
            &state_binding,
        )?);
        let preflight_dir = candidate_dir.clone();
        let preflight_policy = self.policy;
        let preflight_storage = Arc::clone(&self.storage);
        let preflight_permissions = Arc::clone(&self.permissions);
        let preflight_cancel = cancellation.clone();
        let mut preflight_task = tokio::task::spawn_blocking(move || {
            scan_candidate(
                &preflight_dir,
                preflight_policy,
                deadline,
                &preflight_cancel,
                preflight_permissions.as_ref(),
            )
            .and_then(|()| {
                let available = preflight_storage
                    .available_bytes(&preflight_dir)
                    .map_err(|_| ProbeError::InsufficientSpace)?;
                check_cancel_deadline(deadline, &preflight_cancel)?;
                if available < preflight_policy.required_free_bytes {
                    Err(ProbeError::InsufficientSpace)
                } else {
                    Ok(())
                }
            })
        });
        let timeout_cancel = cancellation.clone();
        let wait_cancel = cancellation.clone();
        let preflight = tokio::select! {
            result = &mut preflight_task => result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                timeout_cancel.cancel();
                Ok(Err(ProbeError::DeadlineExceeded))
            }
            _ = wait_for_probe_cancellation(wait_cancel) => {
                Ok(Err(ProbeError::Cancelled))
            }
        };
        if !matches!(&preflight, Ok(Ok(()))) {
            let preflight_kind = match &preflight {
                Ok(Err(ProbeError::Cancelled)) => ProbeErrorKind::Cancelled,
                Ok(Err(ProbeError::DeadlineExceeded)) => ProbeErrorKind::ReadinessTimeout,
                Err(_) => ProbeErrorKind::WorkerFailed,
                _ => ProbeErrorKind::CandidateRejected,
            };
            let error_kind = if state_boundaries.revalidate().is_ok()
                && write_generation_state(
                    &workspace.layout,
                    &workspace.candidate,
                    GenerationState::Failed,
                    &state_binding,
                )
                .is_ok()
            {
                preflight_kind
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
        if boundaries.revalidate().is_err()
            || state_boundaries.revalidate().is_err()
            || write_generation_state(
                &workspace.layout,
                &workspace.candidate,
                GenerationState::Probing,
                &state_binding,
            )
            .is_err()
        {
            let _ = write_generation_state(
                &workspace.layout,
                &workspace.candidate,
                GenerationState::Failed,
                &state_binding,
            );
            return Err(ProbeError::StateWrite);
        }

        let version = workspace.runtime.version.to_string();
        let policy = self.policy;
        let launcher = Arc::clone(&self.launcher);
        let health = Arc::clone(&self.health);
        let worker_cancel = cancellation.clone();
        let worker_boundaries = Arc::clone(&boundaries);
        let outcome = tokio::task::spawn_blocking(move || {
            run_probe_worker(
                &workspace,
                &worker_boundaries,
                policy,
                deadline,
                launcher,
                health,
                worker_cancel,
            )
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
        if boundaries.revalidate().is_err()
            || state_boundaries.revalidate().is_err()
            || write_generation_state(
                &state_binding.layout,
                &state_binding.candidate,
                final_state,
                &state_binding,
            )
            .is_err()
        {
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

async fn wait_for_probe_cancellation(cancellation: ProbeCancellation) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
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
            policy.poll_interval.is_zero() || policy.poll_interval > MAX_POLL_INTERVAL,
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

struct WorkspaceBoundaries {
    generation_root: DirectoryGuard,
    runtime_root: DirectoryGuard,
    runtime_dir: DirectoryGuard,
    candidate_guard: DirectoryGuard,
    project_workspace: DirectoryGuard,
    candidate: PathBuf,
}

impl WorkspaceBoundaries {
    fn revalidate(&self) -> Result<(), ProbeError> {
        self.generation_root.revalidate()?;
        self.runtime_root.revalidate()?;
        self.runtime_dir.revalidate()?;
        self.candidate_guard.revalidate()?;
        self.project_workspace.revalidate()
    }
}

fn validate_workspace(workspace: &ProbeWorkspace) -> Result<WorkspaceBoundaries, ProbeError> {
    validate_active_snapshot(workspace)?;
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
    if workspace.project_workspace.canonicalize().ok().as_deref()
        != Some(workspace.project_workspace.as_path())
    {
        return Err(ProbeError::UnsafeBoundary);
    }
    Ok(WorkspaceBoundaries {
        generation_root: DirectoryGuard::open(&generation_root)?,
        runtime_root: DirectoryGuard::open(&runtime_root)?,
        runtime_dir: DirectoryGuard::open(&runtime_dir)?,
        candidate_guard: DirectoryGuard::open(&candidate)?,
        project_workspace: DirectoryGuard::open(&workspace.project_workspace)?,
        candidate,
    })
}

#[derive(Clone)]
struct GenerationStateBinding {
    layout: RuntimeLayout,
    candidate: DataGeneration,
    runtime_version: String,
    manifest_digest: String,
    trace_id: String,
}

impl GenerationStateBinding {
    fn new(workspace: &ProbeWorkspace, trace_id: &str) -> Self {
        Self {
            layout: workspace.layout.clone(),
            candidate: workspace.candidate.clone(),
            runtime_version: workspace.runtime.version.to_string(),
            manifest_digest: workspace.runtime.manifest_digest.clone(),
            trace_id: trace_id.to_owned(),
        }
    }
}

fn ensure_initial_state(
    layout: &RuntimeLayout,
    candidate: &DataGeneration,
    binding: &GenerationStateBinding,
) -> Result<StateBoundaries, ProbeError> {
    let state_root = layout.generation_root().join(".state");
    if state_root.exists() {
        validate_plain_directory(&state_root)?;
    } else {
        fs::create_dir(&state_root).map_err(|_| ProbeError::StateWrite)?;
        validate_plain_directory(&state_root)?;
    }
    let state_dir = generation_state_dir(layout, candidate);
    if state_dir.exists() {
        validate_plain_directory(&state_dir)?;
    } else {
        fs::create_dir(&state_dir).map_err(|_| ProbeError::StateWrite)?;
    }
    validate_plain_directory(&state_dir)?;
    let canonical_root = state_root
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    let canonical_dir = state_dir
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    if canonical_dir.parent() != Some(canonical_root.as_path()) {
        return Err(ProbeError::UnsafeBoundary);
    }
    let boundaries = StateBoundaries {
        root: DirectoryGuard::open(&canonical_root)?,
        candidate: DirectoryGuard::open(&canonical_dir)?,
    };
    let probing = state_path(layout, candidate, GenerationState::Probing);
    let passed = state_path(layout, candidate, GenerationState::Passed);
    let failed = state_path(layout, candidate, GenerationState::Failed);
    let active = state_path(layout, candidate, GenerationState::Active);
    let inactive = state_path(layout, candidate, GenerationState::Inactive);
    if probing.exists()
        || passed.exists()
        || failed.exists()
        || active.exists()
        || inactive.exists()
    {
        return Err(ProbeError::InvalidGenerationState);
    }
    let candidate_state = state_path(layout, candidate, GenerationState::Candidate);
    if candidate_state.exists() {
        let bytes = read_state_document_bytes(layout, candidate, GenerationState::Candidate)?;
        let document: GenerationStateDocument =
            serde_json::from_slice(&bytes).map_err(|_| ProbeError::InvalidGenerationState)?;
        validate_state_document(&document, GenerationState::Candidate, binding)?;
    } else {
        write_generation_state(layout, candidate, GenerationState::Candidate, binding)?;
    }
    Ok(boundaries)
}

struct StateBoundaries {
    root: DirectoryGuard,
    candidate: DirectoryGuard,
}

impl StateBoundaries {
    fn revalidate(&self) -> Result<(), ProbeError> {
        self.root.revalidate()?;
        self.candidate.revalidate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationStateDocument {
    schema: u32,
    candidate_id: String,
    runtime_version: String,
    manifest_digest: String,
    state: GenerationState,
    trace_id: String,
}

fn validate_state_document(
    document: &GenerationStateDocument,
    expected: GenerationState,
    binding: &GenerationStateBinding,
) -> Result<(), ProbeError> {
    let valid = document.schema == GENERATION_STATE_SCHEMA
        && document.state == expected
        && document.candidate_id == binding.candidate.id
        && document.runtime_version == binding.runtime_version
        && document.manifest_digest == binding.manifest_digest
        && document.trace_id == binding.trace_id
        && validate_trace_id(&document.trace_id).is_ok();
    if valid {
        Ok(())
    } else {
        Err(ProbeError::InvalidGenerationState)
    }
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

fn generation_state_dir(layout: &RuntimeLayout, candidate: &DataGeneration) -> PathBuf {
    layout.generation_root().join(".state").join(&candidate.id)
}

fn state_path(
    layout: &RuntimeLayout,
    candidate: &DataGeneration,
    state: GenerationState,
) -> PathBuf {
    generation_state_dir(layout, candidate).join(format!("{}.json", state_name(state)))
}

/// 追加一个原子 generation 状态标记，供后续激活器沿用同一 schema。
fn write_generation_state(
    layout: &RuntimeLayout,
    candidate: &DataGeneration,
    state: GenerationState,
    binding: &GenerationStateBinding,
) -> Result<(), ProbeError> {
    let state_dir = generation_state_dir(layout, candidate);
    let destination = state_path(layout, candidate, state);
    if destination.exists() {
        return Err(ProbeError::InvalidGenerationState);
    }
    let temporary = state_dir.join(format!(".{}-{}.tmp", binding.trace_id, state_name(state)));
    let bytes = serde_json::to_vec(&GenerationStateDocument {
        schema: GENERATION_STATE_SCHEMA,
        candidate_id: candidate.id.clone(),
        runtime_version: binding.runtime_version.clone(),
        manifest_digest: binding.manifest_digest.clone(),
        state,
        trace_id: binding.trace_id.clone(),
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

/// Task 9 激活器读取的严格 Passed 状态证明。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PassedGenerationState {
    candidate: DataGeneration,
    runtime: InstalledRuntime,
    trace_id: String,
}

impl PassedGenerationState {
    /// 返回已严格校验的 candidate。
    ///
    /// :return: 与 Passed marker 绑定的 candidate 引用。
    /// :raises: 此只读访问不产生错误。
    pub fn candidate(&self) -> &DataGeneration {
        &self.candidate
    }

    /// 返回已严格校验的 runtime 与 manifest digest。
    ///
    /// :return: 与 Passed marker 绑定的 runtime 引用。
    /// :raises: 此只读访问不产生错误。
    pub fn runtime(&self) -> &InstalledRuntime {
        &self.runtime
    }

    /// 返回已严格校验的 trace id。
    ///
    /// :return: 与 Passed marker 绑定的 trace id。
    /// :raises: 此只读访问不产生错误。
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }
}

/// 严格读取并绑定 candidate/runtime/manifest/trace 的 Passed 标记。
///
/// :param layout: 固定 generation 元数据根布局。
/// :param candidate: Task 9 准备激活的 candidate。
/// :param runtime: 与探活时完全相同的 runtime 与 manifest digest。
/// :param trace_id: 与 candidate marker 完全相同的本次操作标识。
/// :return: 可供激活器继续提交 deployment 的类型化证明。
/// :raises ProbeError: 标记缺失、schema/字段不匹配或 JSON 非法时失败关闭。
pub fn read_passed_generation_state(
    layout: &RuntimeLayout,
    candidate: &DataGeneration,
    runtime: &InstalledRuntime,
    trace_id: &str,
) -> Result<PassedGenerationState, ProbeError> {
    validate_trace_id(trace_id)?;
    let binding = GenerationStateBinding {
        layout: layout.clone(),
        candidate: candidate.clone(),
        runtime_version: runtime.version.to_string(),
        manifest_digest: runtime.manifest_digest.clone(),
        trace_id: trace_id.to_owned(),
    };
    let bytes = read_state_document_bytes(layout, candidate, GenerationState::Passed)?;
    let document: GenerationStateDocument =
        serde_json::from_slice(&bytes).map_err(|_| ProbeError::InvalidGenerationState)?;
    validate_state_document(&document, GenerationState::Passed, &binding)?;
    Ok(PassedGenerationState {
        candidate: candidate.clone(),
        runtime: runtime.clone(),
        trace_id: trace_id.to_owned(),
    })
}

fn read_state_document_bytes(
    layout: &RuntimeLayout,
    candidate: &DataGeneration,
    state: GenerationState,
) -> Result<Vec<u8>, ProbeError> {
    let state_root = layout.generation_root().join(".state");
    let state_dir = generation_state_dir(layout, candidate);
    validate_plain_directory(&state_root)?;
    validate_plain_directory(&state_dir)?;
    let canonical_root = state_root
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    let canonical_dir = state_dir
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    if canonical_dir.parent() != Some(canonical_root.as_path()) {
        return Err(ProbeError::UnsafeBoundary);
    }
    let document_path = state_path(layout, candidate, state);
    let metadata =
        fs::symlink_metadata(&document_path).map_err(|_| ProbeError::InvalidGenerationState)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || has_reparse_point(&metadata)
        || has_multiple_links(&document_path)
    {
        return Err(ProbeError::InvalidGenerationState);
    }
    let canonical_document = document_path
        .canonicalize()
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    if canonical_document.parent() != Some(canonical_dir.as_path()) {
        return Err(ProbeError::UnsafeBoundary);
    }
    let root_guard = DirectoryGuard::open(&canonical_root)?;
    let directory_guard = DirectoryGuard::open(&canonical_dir)?;
    let document_guard = FileGuard::open(&canonical_document)?;
    let bytes = document_guard.read_bounded(16 * 1024)?;
    root_guard.revalidate()?;
    directory_guard.revalidate()?;
    document_guard.revalidate()?;
    Ok(bytes)
}

fn scan_candidate(
    candidate: &Path,
    policy: ProbePolicy,
    deadline: Instant,
    cancellation: &ProbeCancellation,
    permissions: &dyn ProbePermissionInspector,
) -> Result<(), ProbeError> {
    check_cancel_deadline(deadline, cancellation)?;
    permissions
        .ensure_private(candidate)
        .map_err(|_| ProbeError::UnsafePermissions)?;
    check_cancel_deadline(deadline, cancellation)?;
    let mut stack = vec![candidate.to_path_buf()];
    let mut entries_seen = 0_u64;
    let mut bytes = 0_u64;
    while let Some(directory) = stack.pop() {
        check_cancel_deadline(deadline, cancellation)?;
        let entries = fs::read_dir(&directory).map_err(|_| ProbeError::UnsafeBoundary)?;
        for entry in entries {
            check_cancel_deadline(deadline, cancellation)?;
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
            if is_sensitive_file(&canonical) {
                permissions
                    .ensure_private(&canonical)
                    .map_err(|_| ProbeError::UnsafePermissions)?;
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

fn check_cancel_deadline(
    deadline: Instant,
    cancellation: &ProbeCancellation,
) -> Result<(), ProbeError> {
    if cancellation.is_cancelled() {
        Err(ProbeError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(ProbeError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

fn is_sensitive_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.eq_ignore_ascii_case(".env")
            || name.eq_ignore_ascii_case(".credentials.yaml")
            || name.eq_ignore_ascii_case(".credentials.yml")
    })
}

#[cfg(windows)]
#[derive(Debug)]
struct DirectoryGuard {
    path: PathBuf,
    _handle: fs::File,
    identity: (u32, u64),
}

#[cfg(windows)]
#[derive(Debug)]
struct FileGuard {
    path: PathBuf,
    _handle: fs::File,
    identity: (u32, u64),
}

#[cfg(windows)]
impl FileGuard {
    fn open(path: &Path) -> Result<Self, ProbeError> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let metadata = fs::symlink_metadata(path).map_err(|_| ProbeError::UnsafeBoundary)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || has_reparse_point(&metadata)
            || has_multiple_links(path)
        {
            return Err(ProbeError::UnsafeBoundary);
        }
        let mut options = OpenOptions::new();
        options.read(true).share_mode(FILE_SHARE_READ.0);
        let handle = options.open(path).map_err(|_| ProbeError::UnsafeBoundary)?;
        let identity = file_identity(&handle)?;
        Ok(Self {
            path: path.to_path_buf(),
            _handle: handle,
            identity,
        })
    }

    fn revalidate(&self) -> Result<(), ProbeError> {
        let current = Self::open(&self.path)?;
        if current.identity == self.identity {
            Ok(())
        } else {
            Err(ProbeError::UnsafeBoundary)
        }
    }

    fn read_bounded(&self, max_bytes: u64) -> Result<Vec<u8>, ProbeError> {
        let metadata = self
            ._handle
            .metadata()
            .map_err(|_| ProbeError::InvalidGenerationState)?;
        if metadata.len() > max_bytes {
            return Err(ProbeError::InvalidGenerationState);
        }
        let reader = self
            ._handle
            .try_clone()
            .map_err(|_| ProbeError::InvalidGenerationState)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        reader
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ProbeError::InvalidGenerationState)?;
        if bytes.len() as u64 > max_bytes {
            return Err(ProbeError::InvalidGenerationState);
        }
        self.revalidate()?;
        Ok(bytes)
    }
}

#[cfg(windows)]
impl DirectoryGuard {
    fn open(path: &Path) -> Result<Self, ProbeError> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        validate_plain_directory(path)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            // 故意不共享 DELETE：从预检到 stop 期间禁止重命名/替换目录实体。
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0);
        let handle = options.open(path).map_err(|_| ProbeError::UnsafeBoundary)?;
        let identity = file_identity(&handle)?;
        Ok(Self {
            path: path.to_path_buf(),
            _handle: handle,
            identity,
        })
    }

    fn revalidate(&self) -> Result<(), ProbeError> {
        let current = Self::open(&self.path)?;
        if current.identity == self.identity {
            Ok(())
        } else {
            Err(ProbeError::UnsafeBoundary)
        }
    }
}

#[cfg(windows)]
fn file_identity(file: &fs::File) -> Result<(u32, u64), ProbeError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(|_| ProbeError::UnsafeBoundary)?;
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, index))
}

#[cfg(not(windows))]
#[derive(Debug)]
struct DirectoryGuard {
    path: PathBuf,
}

#[cfg(not(windows))]
#[derive(Debug)]
struct FileGuard {
    path: PathBuf,
}

#[cfg(not(windows))]
impl FileGuard {
    fn open(path: &Path) -> Result<Self, ProbeError> {
        let metadata = fs::symlink_metadata(path).map_err(|_| ProbeError::UnsafeBoundary)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(ProbeError::UnsafeBoundary);
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn revalidate(&self) -> Result<(), ProbeError> {
        Self::open(&self.path).map(|_| ())
    }

    fn read_bounded(&self, max_bytes: u64) -> Result<Vec<u8>, ProbeError> {
        let mut file =
            fs::File::open(&self.path).map_err(|_| ProbeError::InvalidGenerationState)?;
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ProbeError::InvalidGenerationState)?;
        if bytes.len() as u64 > max_bytes {
            Err(ProbeError::InvalidGenerationState)
        } else {
            Ok(bytes)
        }
    }
}

#[cfg(not(windows))]
impl DirectoryGuard {
    fn open(path: &Path) -> Result<Self, ProbeError> {
        validate_plain_directory(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn revalidate(&self) -> Result<(), ProbeError> {
        validate_plain_directory(&self.path)
    }
}

#[cfg(windows)]
pub(crate) fn ensure_private_windows_dacl(path: &Path) -> Result<(), std::io::Error> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation,
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY,
        TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinCreatorOwnerSid, WinLocalSystemSid,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PCWSTR;

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status.0 as i32));
    }
    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
    let _descriptor = Descriptor(descriptor);
    if dacl.is_null() || owner.0.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "null DACL",
        ));
    }
    let mut raw_token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) }
        .map_err(|error| std::io::Error::other(format!("HRESULT {:#010X}", error.code().0)))?;
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token.0) };
    let mut token_bytes = 0_u32;
    let _ = unsafe { GetTokenInformation(raw_token, TokenUser, None, 0, &mut token_bytes) };
    if token_bytes < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(std::io::Error::other("token user size"));
    }
    let word_bytes = std::mem::size_of::<usize>();
    let token_words = (token_bytes as usize).div_ceil(word_bytes);
    let mut token_buffer = vec![0_usize; token_words];
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            Some(token_buffer.as_mut_ptr().cast()),
            token_bytes,
            &mut token_bytes,
        )
    }
    .map_err(|error| std::io::Error::other(format!("HRESULT {:#010X}", error.code().0)))?;
    let current_user = unsafe { (*(token_buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let owner_trusted = unsafe {
        EqualSid(owner, current_user).is_ok()
            || IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
    };
    if !owner_trusted {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "untrusted owner",
        ));
    }
    let ace_count = unsafe { (*dacl).AceCount };
    for index in 0..u32::from(ace_count) {
        let mut raw_ace: *mut c_void = std::ptr::null_mut();
        unsafe { GetAce(dacl, index, &mut raw_ace) }
            .map_err(|error| std::io::Error::other(format!("HRESULT {:#010X}", error.code().0)))?;
        if raw_ace.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "null ACE",
            ));
        }
        let header = unsafe { &*(raw_ace.cast::<windows::Win32::Security::ACE_HEADER>()) };
        if matches!(header.AceType, 4 | 5 | 9 | 11) {
            // 条件/对象 allow ACE 的 SID 布局不同。为避免把无法完整解释的宽泛写
            // 授权误判为安全，敏感路径上保守拒绝这类少见 ACL。
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsupported allow ACE",
            ));
        }
        if header.AceType != 0 {
            continue;
        }
        let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
        if !mask_grants_sensitive_write(ace.Mask) {
            continue;
        }
        let sid = PSID((&raw const ace.SidStart).cast_mut().cast::<c_void>());
        let trusted = unsafe {
            EqualSid(sid, current_user).is_ok()
                // 沙箱化桌面进程的 token SID 可能不同于已验证的真实文件 owner；
                // owner 已在上方限定为当前用户/SYSTEM/Administrators，可安全授权写入。
                || EqualSid(sid, owner).is_ok()
                || IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool()
                || IsWellKnownSid(sid, WinCreatorOwnerSid).as_bool()
        };
        if !trusted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "untrusted write ACL",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn mask_grants_sensitive_write(mask: u32) -> bool {
    use windows::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
        FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
    };

    // FILE_GENERIC_WRITE 是复合掩码并包含 SYNCHRONIZE；若直接按位相交，只有读取
    // 权限但带 SYNCHRONIZE 的主体也会被误判为可写。这里只检查真实写入/接管位。
    let dangerous = GENERIC_ALL.0
        | GENERIC_WRITE.0
        | DELETE.0
        | FILE_DELETE_CHILD.0
        | FILE_WRITE_DATA.0
        | FILE_APPEND_DATA.0
        | FILE_WRITE_EA.0
        | FILE_WRITE_ATTRIBUTES.0
        | WRITE_DAC.0
        | WRITE_OWNER.0;
    mask & dangerous != 0
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
    boundaries: &WorkspaceBoundaries,
    policy: ProbePolicy,
    deadline: Instant,
    launcher: Arc<dyn ProcessLauncher>,
    health: Arc<dyn ReadyProbe>,
    cancellation: ProbeCancellation,
) -> (ProbePhase, u32, Option<ProbeErrorKind>, u64) {
    let started = Instant::now();
    if cancellation.is_cancelled() {
        return failed(started, 0, ProbeErrorKind::Cancelled);
    }
    if Instant::now() >= deadline {
        return failed(started, 0, ProbeErrorKind::ReadinessTimeout);
    }
    if validate_active_snapshot(workspace).is_err() {
        return failed(started, 0, ProbeErrorKind::CandidateRejected);
    }
    if let Err(kind) = verify_runtime_closure(workspace, deadline, &cancellation) {
        return failed(started, 0, kind);
    }
    let prepared = boundaries
        .revalidate()
        .map_err(|_| RuntimeError::InvalidLaunchPath {
            field: "boundary",
            reason: "changed",
        })
        .and_then(|()| build_launch_spec(workspace));
    let Ok(prepared) = prepared else {
        return failed(started, 0, ProbeErrorKind::LaunchFailed);
    };
    if cancellation.is_cancelled() {
        return failed(started, 0, ProbeErrorKind::Cancelled);
    }
    if Instant::now() >= deadline {
        return failed(started, 0, ProbeErrorKind::ReadinessTimeout);
    }
    if prepared.revalidate().is_err() {
        return failed(started, 0, ProbeErrorKind::LaunchFailed);
    }
    let Ok(mut child) = launcher.spawn(&prepared.spec) else {
        return failed(started, 0, ProbeErrorKind::LaunchFailed);
    };

    let outcome = wait_for_both_gates(
        child.as_mut(),
        prepared
            .spec
            .loopback_port
            .expect("official spec always has port"),
        policy,
        deadline,
        health.as_ref(),
        &cancellation,
    );
    // 正常路径只显式 stop 一次；panic/提前返回依赖 RuntimeProcess 的 Drop 契约回收。
    let cleanup_grace = policy
        .stop_grace
        .min(deadline.saturating_duration_since(Instant::now()));
    let cleanup = child.stop(cleanup_grace);
    let boundary_cleanup = boundaries.revalidate().and_then(|()| prepared.revalidate());
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if cleanup.is_err() || boundary_cleanup.is_err() {
        return (
            ProbePhase::Failed,
            outcome.1,
            Some(ProbeErrorKind::CleanupFailed),
            elapsed_ms,
        );
    }
    if let Err(kind) = verify_runtime_closure(workspace, deadline, &cancellation) {
        // 完整性不一致始终优先；若进程阶段已得到稳定失败类别，则截止/取消仅表示
        // 复核未能完成，不应把 InvalidWebUi 等更具体的原始结论覆盖掉。
        if kind == ProbeErrorKind::RuntimeIntegrityFailed || outcome.0.is_none() {
            return failed(started, outcome.1, kind);
        }
    }
    match outcome.0 {
        None => (ProbePhase::Passed, outcome.1, None, elapsed_ms),
        Some(error) => (ProbePhase::Failed, outcome.1, Some(error), elapsed_ms),
    }
}

fn verify_runtime_closure(
    workspace: &ProbeWorkspace,
    deadline: Instant,
    cancellation: &ProbeCancellation,
) -> Result<(), ProbeErrorKind> {
    // 候选数据的扫描额度与已安装 runtime 的清单额度是两个独立安全域；复核沿用
    // 安装器的默认上限，避免候选的小额度错误拒绝合法 runtime。
    let archive_policy = ArchiveInstallPolicy::default();
    let runtime_dir = workspace.layout.runtime_dir(&workspace.runtime);
    let result = verify_installed_runtime_inventory(&runtime_dir, archive_policy, || {
        !cancellation.is_cancelled() && Instant::now() < deadline
    });
    match result {
        Ok(()) => Ok(()),
        Err(ArchiveInstallError::InventoryVerificationAborted) if cancellation.is_cancelled() => {
            Err(ProbeErrorKind::Cancelled)
        }
        Err(ArchiveInstallError::InventoryVerificationAborted) => {
            Err(ProbeErrorKind::ReadinessTimeout)
        }
        Err(_) => Err(ProbeErrorKind::RuntimeIntegrityFailed),
    }
}

struct PreparedLaunch {
    spec: RuntimeLaunchSpec,
    directories: Vec<DirectoryGuard>,
    node: FileGuard,
    cli: FileGuard,
}

impl PreparedLaunch {
    fn revalidate(&self) -> Result<(), ProbeError> {
        for directory in &self.directories {
            directory.revalidate()?;
        }
        self.node.revalidate()?;
        self.cli.revalidate()
    }
}

fn build_launch_spec(workspace: &ProbeWorkspace) -> Result<PreparedLaunch, RuntimeError> {
    let runtime_root = workspace
        .layout
        .runtime_root()
        .canonicalize()
        .map_err(|_| RuntimeError::InvalidLaunchPath {
            field: "runtime_root",
            reason: "missing",
        })?;
    let runtime_dir = workspace
        .layout
        .runtime_dir(&workspace.runtime)
        .canonicalize()
        .map_err(|_| RuntimeError::InvalidLaunchPath {
            field: "runtime_root",
            reason: "missing",
        })?;
    if runtime_dir.parent() != Some(runtime_root.as_path()) {
        return Err(RuntimeError::InvalidLaunchPath {
            field: "runtime_root",
            reason: "outside boundary",
        });
    }
    let node = runtime_dir
        .join(format!("node-v{}-win-x64", workspace.node_version))
        .join("node.exe");
    let cli = runtime_dir.join("app/node_modules/@deepseek-ai/dsh/lib/bin.js");
    for (field, path) in [("node", &node), ("cli", &cli)] {
        validate_runtime_member(&runtime_dir, path, field)?;
        let canonical = path
            .canonicalize()
            .map_err(|_| RuntimeError::InvalidLaunchPath {
                field,
                reason: "missing",
            })?;
        if !canonical.starts_with(&runtime_dir)
            || fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_symlink() || has_reparse_point(&metadata))
                .unwrap_or(true)
        {
            return Err(RuntimeError::InvalidLaunchPath {
                field,
                reason: "outside boundary",
            });
        }
    }
    let candidate = workspace.layout.generation_dir(&workspace.candidate);
    let port = reserve_loopback_port()?;
    let spec = RuntimeLaunchSpec::official(
        runtime_dir.clone(),
        node.clone(),
        cli.clone(),
        workspace.project_workspace.clone(),
        candidate,
        port,
    )?;
    let directories = runtime_member_directories(&runtime_dir, [&node, &cli]).map_err(|_| {
        RuntimeError::InvalidLaunchPath {
            field: "runtime_member",
            reason: "changed",
        }
    })?;
    let node = FileGuard::open(&node).map_err(|_| RuntimeError::InvalidLaunchPath {
        field: "node",
        reason: "changed",
    })?;
    let cli = FileGuard::open(&cli).map_err(|_| RuntimeError::InvalidLaunchPath {
        field: "cli",
        reason: "changed",
    })?;
    Ok(PreparedLaunch {
        spec,
        directories,
        node,
        cli,
    })
}

fn runtime_member_directories<'a>(
    runtime_dir: &Path,
    members: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<Vec<DirectoryGuard>, ProbeError> {
    let mut paths = Vec::<PathBuf>::new();
    for member in members {
        let relative = member
            .parent()
            .and_then(|parent| parent.strip_prefix(runtime_dir).ok())
            .ok_or(ProbeError::UnsafeBoundary)?;
        let mut current = runtime_dir.to_path_buf();
        for component in relative.components() {
            current.push(component.as_os_str());
            if !paths.contains(&current) {
                paths.push(current.clone());
            }
        }
    }
    paths
        .iter()
        .map(|path| DirectoryGuard::open(path))
        .collect()
}

fn validate_runtime_member(
    runtime_dir: &Path,
    member: &Path,
    field: &'static str,
) -> Result<(), RuntimeError> {
    let relative =
        member
            .strip_prefix(runtime_dir)
            .map_err(|_| RuntimeError::InvalidLaunchPath {
                field,
                reason: "outside boundary",
            })?;
    let mut current = runtime_dir.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| RuntimeError::InvalidLaunchPath {
                field,
                reason: "missing",
            })?;
        if metadata.file_type().is_symlink() || has_reparse_point(&metadata) {
            return Err(RuntimeError::InvalidLaunchPath {
                field,
                reason: "reparse point",
            });
        }
    }
    Ok(())
}

fn wait_for_both_gates(
    child: &mut dyn RuntimeProcess,
    port: u16,
    policy: ProbePolicy,
    deadline: Instant,
    health: &dyn ReadyProbe,
    cancellation: &ProbeCancellation,
) -> (Option<ProbeErrorKind>, u32) {
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
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let kind = if stdout_ready {
                ProbeErrorKind::InvalidWebUi
            } else {
                ProbeErrorKind::ReadinessTimeout
            };
            return (Some(kind), retries);
        }
        let slice = remaining
            .min(policy.poll_interval)
            .min(Duration::from_millis(100));
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

#[cfg(all(test, windows))]
mod tests {
    use super::{FileGuard, mask_grants_sensitive_write};
    use std::fs::{self, OpenOptions};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{FILE_DELETE_CHILD, WRITE_DAC, WRITE_OWNER};

    #[test]
    fn file_guard_denies_in_place_writes_while_identity_is_trusted() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("dsh-probe-file-guard-{unique}"));
        fs::write(&path, b"trusted").expect("fixture");
        let guard = FileGuard::open(&path).expect("guard");

        assert!(OpenOptions::new().write(true).open(&path).is_err());
        guard.revalidate().expect("same identity");
        assert_eq!(guard.read_bounded(16).expect("read"), b"trusted");
    }

    #[test]
    fn dacl_mask_rejects_generic_and_acl_takeover_rights() {
        for mask in [
            GENERIC_ALL.0,
            GENERIC_WRITE.0,
            WRITE_DAC.0,
            WRITE_OWNER.0,
            FILE_DELETE_CHILD.0,
        ] {
            assert!(mask_grants_sensitive_write(mask));
        }
        assert!(!mask_grants_sensitive_write(1));
    }
}
