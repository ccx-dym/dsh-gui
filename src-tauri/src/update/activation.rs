/// Agent 任务层的可确认忙闲状态；与 runtime 生命周期状态分开建模。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeBusyState {
    /// Agent 已明确确认没有正在执行的任务。
    ConfirmedIdle,
    /// 至少一个 Agent 任务仍在执行。
    ActiveTask,
    /// 无法可靠确认 Agent 是否空闲。
    UnknownBusy,
}

/// 在控制器独占生命周期门禁后确认 Agent 是否可安全停止的可信提供者。
pub trait RuntimeBusyProvider: Send + Sync + 'static {
    /// 尝试冻结新任务并返回当前忙闲结论。
    ///
    /// :return: 只有实现能原子阻止新任务时才返回 `ConfirmedIdle`。
    /// :raises: 失败通过 `UnknownBusy` 关闭 Ready 激活路径，不暴露动态错误。
    fn quiesce(&self) -> RuntimeBusyState;
}

/// 尚未接入官方 Agent 活动 API 时的生产安全默认值。
pub struct UnknownRuntimeBusyProvider;

impl RuntimeBusyProvider for UnknownRuntimeBusyProvider {
    fn quiesce(&self) -> RuntimeBusyState {
        RuntimeBusyState::UnknownBusy
    }
}

use crate::app_controller::{ActivationSession, ProbeLease};
use crate::paths::RuntimeLayout;
use crate::runtime::RuntimeError;
use crate::runtime::atomic_file::replace_file;
use crate::runtime::install_state::{
    ActiveDeployment, DataGeneration, InstallStateError, InstallStateStore, InstalledRuntime,
    validate_project_workspace,
};
use crate::update::probe::{
    ProbeCancellation, ProbeError, ProbePhase, ProbeWorkspace, RuntimeProbe,
    read_passed_generation_state,
};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

const LEGACY_ACTIVATION_SCHEMA: u32 = 1;
const ACTIVATION_SCHEMA: u32 = 2;
const SETTINGS_SCHEMA: u32 = 1;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;

/// candidate 快照的资源边界。
#[derive(Clone, Copy, Debug)]
pub struct SnapshotPolicy {
    pub max_files: u64,
    pub max_bytes: u64,
    pub required_free_bytes: u64,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_bytes: 8 * 1024 * 1024 * 1024,
            required_free_bytes: 64 * 1024 * 1024,
        }
    }
}

/// 激活事务的持久化崩溃点。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationCheckpoint {
    CandidatePrepared,
    JournalPrepared,
    PointerCommitted,
    JournalCommitted,
    BeforeFirstStart,
    RollingBackPersisted,
}

/// 可注入的崩溃点观察器；生产实现通常是 no-op。
pub trait ActivationCheckpointSink: Send + Sync {
    fn reached(&self, checkpoint: ActivationCheckpoint) -> Result<(), ActivationError>;
}

/// 激活器调用的隔离 probe 边界。
pub trait ActivationProbe: Send + Sync {
    fn probe<'a>(
        &'a self,
        layout: &'a RuntimeLayout,
        runtime: &'a InstalledRuntime,
        candidate: &'a DataGeneration,
        project_workspace: &'a Path,
        trace_id: &'a str,
        lease: ProbeLease,
    ) -> BoxFuture<'a, Result<(), ActivationError>>;
}

/// 将 Task 8 的真实异步 probe 接入激活事务。
pub struct RuntimeProbeAdapter {
    probe: RuntimeProbe,
}

impl RuntimeProbeAdapter {
    /// 包装 Task 8 的真实异步 probe。
    ///
    /// :param probe: 已配置超时、进程与权限策略的隔离 probe。
    /// :return: 可供 activation 事务调用的异步适配器。
    /// :raises: 此构造器不执行 I/O，不产生错误。
    pub fn new(probe: RuntimeProbe) -> Self {
        Self { probe }
    }
}

impl ActivationProbe for RuntimeProbeAdapter {
    fn probe<'a>(
        &'a self,
        layout: &'a RuntimeLayout,
        runtime: &'a InstalledRuntime,
        candidate: &'a DataGeneration,
        project_workspace: &'a Path,
        trace_id: &'a str,
        lease: ProbeLease,
    ) -> BoxFuture<'a, Result<(), ActivationError>> {
        async move {
            let node_version = runtime.node_version.clone();
            let workspace = ProbeWorkspace::new(
                layout.clone(),
                runtime.clone(),
                node_version,
                candidate.clone(),
                project_workspace.to_path_buf(),
                &lease,
            )?;
            let report = self
                .probe
                .probe(workspace, trace_id.to_owned(), ProbeCancellation::new())
                .await?;
            if report.phase != ProbePhase::Passed {
                return Err(ActivationError::ProbeRejected);
            }
            Ok(())
        }
        .boxed()
    }
}

/// 一次兼容运行时激活请求；项目目录从可信本地设置或 prior deployment 派生。
#[derive(Clone, Debug)]
pub struct ActivationRequest {
    pub runtime: InstalledRuntime,
    pub candidate: DataGeneration,
    pub activated_at: String,
    pub trace_id: String,
}

/// 激活或单次恢复的稳定终态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivationOutcome {
    Activated,
    RolledBack { failure: ActivationFailure },
    FreshInstallFailed { failure: ActivationFailure },
    NothingToRecover,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationFailureStage {
    TargetStart,
    RecoveryResume,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationFailure {
    pub stage: ActivationFailureStage,
    pub error_code: String,
}

impl ActivationFailure {
    fn runtime(stage: ActivationFailureStage, error: &RuntimeError) -> Self {
        Self {
            stage,
            error_code: error.code().to_owned(),
        }
    }

    fn interrupted_resume() -> Self {
        Self {
            stage: ActivationFailureStage::RecoveryResume,
            error_code: "activation_interrupted".to_owned(),
        }
    }
}

/// 激活、回滚或崩溃恢复失败。
#[derive(Debug, Error)]
pub enum ActivationError {
    #[error("激活 I/O 失败: {0}")]
    Io(#[source] std::io::Error),
    #[error("激活后台文件任务异常退出")]
    WorkerFailed,
    #[error("安装状态失败: {0}")]
    InstallState(#[from] InstallStateError),
    #[error("candidate probe 证明无效: {0}")]
    Probe(#[from] ProbeError),
    #[error("candidate 未通过隔离 probe")]
    ProbeRejected,
    #[error("runtime 生命周期失败: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("candidate 已存在，拒绝覆盖")]
    CandidateAlreadyExists,
    #[error("candidate 快照超出文件数、大小或磁盘空间边界")]
    SnapshotLimit,
    #[error("candidate 快照包含重解析点、硬链接或特殊文件")]
    UnsafeSnapshot,
    #[error("activation journal 无效或与 pointer 不一致")]
    InvalidJournal,
    #[error("activation 在 {checkpoint:?} 被模拟中断")]
    Interrupted { checkpoint: ActivationCheckpoint },
    #[error("新版失败后恢复未完成，需要人工恢复（{failure:?}，recovery={recovery_code}）")]
    RecoveryRequired {
        failure: ActivationFailure,
        recovery_code: String,
    },
}

impl ActivationError {
    pub(crate) fn io(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    Prepared,
    Committed,
    RollingBack,
    Active,
    RolledBack,
    FreshInstallFailed,
    RecoveryRequired,
}

/// 可审计的单次 activation journal。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationJournal {
    schema: u32,
    activation_id: String,
    prior: Option<JournalDeployment>,
    target: JournalDeployment,
    state: JournalState,
    #[serde(default)]
    failure: Option<ActivationFailure>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalDeployment {
    runtime_version: String,
    manifest_digest: String,
    node_version: String,
    data_id: String,
    activated_at: String,
    project_workspace: String,
}

impl JournalDeployment {
    fn from_active(value: &ActiveDeployment) -> Result<Self, ActivationError> {
        let node_version = &value.runtime.node_version;
        let workspace =
            value
                .project_workspace
                .as_ref()
                .ok_or(InstallStateError::MissingDescriptor {
                    field: "project_workspace",
                })?;
        Ok(Self {
            runtime_version: value.runtime.version.to_string(),
            manifest_digest: value.runtime.manifest_digest.clone(),
            node_version: node_version.to_string(),
            data_id: value.data.id.clone(),
            activated_at: value.activated_at.clone(),
            project_workspace: workspace.to_string_lossy().into_owned(),
        })
    }

    fn into_active(self) -> Result<ActiveDeployment, ActivationError> {
        let runtime = InstalledRuntime::with_node_version(
            &self.runtime_version,
            self.manifest_digest,
            &self.node_version,
        )?;
        let data = DataGeneration::new(&self.data_id)?;
        let workspace = validate_project_workspace(Path::new(&self.project_workspace))?;
        Ok(ActiveDeployment::with_project_workspace(
            runtime,
            data,
            self.activated_at,
            workspace,
        ))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActivationSettings {
    schema: u32,
    project_workspace: String,
}

/// 串行准备 candidate、严格 probe、提交 pointer，并在首启失败时回滚配对。
pub struct RuntimeActivator {
    layout: RuntimeLayout,
    store: Arc<dyn DeploymentStore>,
    policy: SnapshotPolicy,
    settings_file: PathBuf,
    journal_root: PathBuf,
    acl: Arc<dyn SnapshotAclInspector>,
}

trait DeploymentStore: Send + Sync {
    fn load(&self) -> Result<ActiveDeployment, InstallStateError>;
    fn save(&self, deployment: &ActiveDeployment) -> Result<(), InstallStateError>;
    fn mark_uninstalled(&self, changed_at: &str) -> Result<(), InstallStateError>;
}

impl DeploymentStore for InstallStateStore {
    fn load(&self) -> Result<ActiveDeployment, InstallStateError> {
        InstallStateStore::load(self)
    }

    fn save(&self, deployment: &ActiveDeployment) -> Result<(), InstallStateError> {
        InstallStateStore::save(self, deployment)
    }

    fn mark_uninstalled(&self, changed_at: &str) -> Result<(), InstallStateError> {
        InstallStateStore::mark_uninstalled(self, changed_at)
    }
}

trait SnapshotAclInspector: Send + Sync {
    fn ensure_private(&self, path: &Path) -> Result<(), ActivationError>;
}

struct SystemSnapshotAclInspector;

impl SnapshotAclInspector for SystemSnapshotAclInspector {
    fn ensure_private(&self, path: &Path) -> Result<(), ActivationError> {
        validate_private_acl(path)
    }
}

impl RuntimeActivator {
    /// 创建绑定固定 runtime/data/settings 根的激活器。
    ///
    /// :param layout: 预定义运行时、generation 与漫游设置布局。
    /// :param policy: candidate 文件数、字节数与磁盘余量边界。
    /// :return: 不创建目录的事务激活器。
    /// :raises ActivationError: 额度为零或布局缺少设置目录时返回。
    pub fn new(layout: RuntimeLayout, policy: SnapshotPolicy) -> Result<Self, ActivationError> {
        if policy.max_files == 0 || policy.max_bytes == 0 {
            return Err(ActivationError::SnapshotLimit);
        }
        let settings = layout
            .settings_dir()
            .map_err(|error| ActivationError::Io(std::io::Error::other(error.to_string())))?;
        Ok(Self {
            store: Arc::new(InstallStateStore::new(layout.clone())),
            settings_file: settings.join("activation-settings.json"),
            journal_root: settings.join("activation-journal"),
            layout,
            policy,
            acl: Arc::new(SystemSnapshotAclInspector),
        })
    }

    #[cfg(test)]
    fn with_acl_inspector(mut self, acl: Arc<dyn SnapshotAclInspector>) -> Self {
        self.acl = acl;
        self
    }

    #[cfg(test)]
    fn with_store(mut self, store: Arc<dyn DeploymentStore>) -> Self {
        self.store = store;
        self
    }

    /// 保存首次安装向导已确认的可信本地项目目录。
    ///
    /// :param path: 用户选择且当前存在的绝对普通目录。
    /// :return: 严格本地设置已 flush 并原子替换时返回。
    /// :raises ActivationError: 目录不可信或设置文件写入失败时返回。
    pub fn save_trusted_workspace(&self, path: &Path) -> Result<(), ActivationError> {
        let path = validate_project_workspace(path)?;
        let document = ActivationSettings {
            schema: SETTINGS_SCHEMA,
            project_workspace: path.to_string_lossy().into_owned(),
        };
        let parent = self
            .settings_file
            .parent()
            .ok_or(ActivationError::InvalidJournal)?;
        fs::create_dir_all(parent).map_err(ActivationError::io)?;
        atomic_json_write(
            parent,
            &self.settings_file,
            "activation-settings.json.tmp",
            &document,
        )
    }

    /// 执行单次 activation；调用方必须持有 AppController 签发的 session 至返回。
    ///
    /// :param session: 已完成 busy/lifecycle 门禁并受控停止 runtime 的独占会话。
    /// :param request: 已安装目标、唯一 candidate 与关联 trace。
    /// :param probe: Task 8 隔离探活实现。
    /// :param checkpoints: journal 崩溃点观察器。
    /// :return: 新版已就绪、旧版已回滚或 fresh 保持未激活的终态。
    /// :raises ActivationError: 快照、probe、pointer、首启或回滚失败时返回。
    pub async fn activate(
        &self,
        session: ActivationSession,
        request: ActivationRequest,
        probe: &dyn ActivationProbe,
        checkpoints: &dyn ActivationCheckpointSink,
    ) -> Result<ActivationOutcome, ActivationError> {
        session.claim_transaction()?;
        let prior = match self.store.load() {
            Ok(value) => Some(value),
            Err(InstallStateError::NotInstalled) => None,
            Err(error) => return Err(error.into()),
        };
        let workspace = match &prior {
            Some(value) => value
                .project_workspace
                .as_deref()
                .ok_or(InstallStateError::MissingDescriptor {
                    field: "project_workspace",
                })?
                .to_path_buf(),
            None => self.load_trusted_workspace()?,
        };
        validate_project_workspace(&workspace)?;
        self.prepare_candidate(prior.as_ref(), &request.candidate)
            .await?;
        checkpoints.reached(ActivationCheckpoint::CandidatePrepared)?;

        probe
            .probe(
                &self.layout,
                &request.runtime,
                &request.candidate,
                &workspace,
                &request.trace_id,
                session.probe_lease(),
            )
            .await?;
        let passed = read_passed_generation_state(
            &self.layout,
            &request.candidate,
            &request.runtime,
            &request.trace_id,
        )?;
        if passed.runtime() != &request.runtime || passed.candidate() != &request.candidate {
            return Err(ActivationError::InvalidJournal);
        }
        let target = ActiveDeployment::with_project_workspace(
            request.runtime,
            request.candidate,
            request.activated_at,
            workspace,
        );
        let mut journal = ActivationJournal {
            schema: ACTIVATION_SCHEMA,
            activation_id: request.trace_id,
            prior: prior
                .as_ref()
                .map(JournalDeployment::from_active)
                .transpose()?,
            target: JournalDeployment::from_active(&target)?,
            state: JournalState::Prepared,
            failure: None,
        };
        self.write_journal(&journal)?;
        checkpoints.reached(ActivationCheckpoint::JournalPrepared)?;
        self.store.save(&target)?;
        checkpoints.reached(ActivationCheckpoint::PointerCommitted)?;
        journal.state = JournalState::Committed;
        self.write_journal(&journal)?;
        checkpoints.reached(ActivationCheckpoint::JournalCommitted)?;
        checkpoints.reached(ActivationCheckpoint::BeforeFirstStart)?;
        let committed = self.store.load()?;
        match session.start_and_wait_ready(&committed, &target).await {
            Ok(()) => {
                journal.state = JournalState::Active;
                self.write_journal(&journal)?;
                Ok(ActivationOutcome::Activated)
            }
            Err(error) => {
                let failure =
                    ActivationFailure::runtime(ActivationFailureStage::TargetStart, &error);
                self.rollback_failed_start(
                    &session,
                    &mut journal,
                    prior.as_ref(),
                    failure,
                    Some(checkpoints),
                )
                .await
            }
        }
    }

    /// 恢复唯一未完成 journal；不删除 runtime、generation 或 journal。
    ///
    /// :param session: 启动阶段取得的独占激活会话。
    /// :return: 恢复 target/prior、fresh 未激活或无待恢复事务的终态。
    /// :raises ActivationError: journal/pointer 不一致或恢复启动失败时返回。
    pub async fn recover(
        &self,
        session: ActivationSession,
    ) -> Result<ActivationOutcome, ActivationError> {
        session.claim_transaction()?;
        let Some(mut journal) = self.load_pending_journal()? else {
            return Ok(ActivationOutcome::NothingToRecover);
        };
        if journal.state == JournalState::RecoveryRequired {
            return Err(ActivationError::RecoveryRequired {
                failure: journal
                    .failure
                    .clone()
                    .unwrap_or_else(ActivationFailure::interrupted_resume),
                recovery_code: "persisted_recovery_required".to_owned(),
            });
        }
        let target = journal.target.clone().into_active()?;
        let prior = journal
            .prior
            .clone()
            .map(JournalDeployment::into_active)
            .transpose()?;
        let current = match self.store.load() {
            Ok(value) => Some(value),
            Err(InstallStateError::NotInstalled) => None,
            Err(error) => return Err(error.into()),
        };
        if journal.state == JournalState::RollingBack {
            let failure = journal
                .failure
                .clone()
                .unwrap_or_else(ActivationFailure::interrupted_resume);
            return self
                .resume_rollback(&session, &mut journal, prior.as_ref(), failure)
                .await;
        }
        if current.as_ref() == Some(&target) {
            return match session
                .start_and_wait_ready(current.as_ref().expect("target checked"), &target)
                .await
            {
                Ok(()) => {
                    journal.state = JournalState::Active;
                    self.write_journal(&journal)?;
                    Ok(ActivationOutcome::Activated)
                }
                Err(error) => {
                    let failure =
                        ActivationFailure::runtime(ActivationFailureStage::RecoveryResume, &error);
                    self.rollback_failed_start(
                        &session,
                        &mut journal,
                        prior.as_ref(),
                        failure,
                        None,
                    )
                    .await
                }
            };
        }
        if current == prior {
            if let Some(old) = &prior {
                let actual = self.store.load()?;
                if let Err(error) = session.start_and_wait_ready(&actual, old).await {
                    let failure =
                        ActivationFailure::runtime(ActivationFailureStage::RecoveryResume, &error);
                    return Err(self.recovery_required(
                        &mut journal,
                        failure,
                        "prior_start_failed",
                    ));
                }
                journal.state = JournalState::RolledBack;
                self.write_journal(&journal)?;
                return Ok(ActivationOutcome::RolledBack {
                    failure: journal
                        .failure
                        .clone()
                        .unwrap_or_else(ActivationFailure::interrupted_resume),
                });
            }
            journal.state = JournalState::FreshInstallFailed;
            self.write_journal(&journal)?;
            return Ok(ActivationOutcome::FreshInstallFailed {
                failure: journal
                    .failure
                    .clone()
                    .unwrap_or_else(ActivationFailure::interrupted_resume),
            });
        }
        let failure = journal
            .failure
            .clone()
            .unwrap_or_else(ActivationFailure::interrupted_resume);
        Err(self.recovery_required(&mut journal, failure, "pointer_mismatch"))
    }

    async fn rollback_failed_start(
        &self,
        session: &ActivationSession,
        journal: &mut ActivationJournal,
        prior: Option<&ActiveDeployment>,
        failure: ActivationFailure,
        checkpoints: Option<&dyn ActivationCheckpointSink>,
    ) -> Result<ActivationOutcome, ActivationError> {
        journal.state = JournalState::RollingBack;
        journal.failure = Some(failure.clone());
        self.write_journal(journal)?;
        if let Some(checkpoints) = checkpoints {
            checkpoints.reached(ActivationCheckpoint::RollingBackPersisted)?;
        }
        self.resume_rollback(session, journal, prior, failure).await
    }

    async fn resume_rollback(
        &self,
        session: &ActivationSession,
        journal: &mut ActivationJournal,
        prior: Option<&ActiveDeployment>,
        failure: ActivationFailure,
    ) -> Result<ActivationOutcome, ActivationError> {
        if session.stop().await.is_err() {
            return Err(self.recovery_required(journal, failure, "target_stop_failed"));
        }
        if let Some(old) = prior {
            if self.store.save(old).is_err() {
                return Err(self.recovery_required(journal, failure, "pointer_restore_failed"));
            }
            let actual = self.store.load()?;
            if session.start_and_wait_ready(&actual, old).await.is_err() {
                return Err(self.recovery_required(journal, failure, "prior_start_failed"));
            }
            journal.state = JournalState::RolledBack;
            self.write_journal(journal)?;
            Ok(ActivationOutcome::RolledBack { failure })
        } else {
            if self
                .store
                .mark_uninstalled(&journal.target.activated_at)
                .is_err()
            {
                return Err(self.recovery_required(journal, failure, "mark_uninstalled_failed"));
            }
            journal.state = JournalState::FreshInstallFailed;
            self.write_journal(journal)?;
            Ok(ActivationOutcome::FreshInstallFailed { failure })
        }
    }

    fn recovery_required(
        &self,
        journal: &mut ActivationJournal,
        failure: ActivationFailure,
        recovery_code: &str,
    ) -> ActivationError {
        journal.state = JournalState::RecoveryRequired;
        journal.failure = Some(failure.clone());
        let code = match self.write_journal(journal) {
            Ok(()) => recovery_code.to_owned(),
            Err(_) => format!("{recovery_code}:journal_persist_failed"),
        };
        ActivationError::RecoveryRequired {
            failure,
            recovery_code: code,
        }
    }

    fn load_trusted_workspace(&self) -> Result<PathBuf, ActivationError> {
        let bytes = fs::read(&self.settings_file).map_err(ActivationError::io)?;
        let document: ActivationSettings =
            serde_json::from_slice(&bytes).map_err(|_| ActivationError::InvalidJournal)?;
        if document.schema != SETTINGS_SCHEMA {
            return Err(ActivationError::InvalidJournal);
        }
        validate_project_workspace(Path::new(&document.project_workspace)).map_err(Into::into)
    }

    async fn prepare_candidate(
        &self,
        prior: Option<&ActiveDeployment>,
        candidate: &DataGeneration,
    ) -> Result<(), ActivationError> {
        let layout = self.layout.clone();
        let prior = prior.cloned();
        let candidate = candidate.clone();
        let policy = self.policy;
        let acl = Arc::clone(&self.acl);
        tokio::task::spawn_blocking(move || {
            prepare_candidate_blocking(&layout, prior.as_ref(), &candidate, policy, acl.as_ref())
        })
        .await
        .map_err(|_| ActivationError::WorkerFailed)?
    }
}

fn prepare_candidate_blocking(
    layout: &RuntimeLayout,
    prior: Option<&ActiveDeployment>,
    candidate: &DataGeneration,
    policy: SnapshotPolicy,
    acl: &dyn SnapshotAclInspector,
) -> Result<(), ActivationError> {
    fs::create_dir_all(layout.generation_root()).map_err(ActivationError::io)?;
    validate_plain_dir(layout.generation_root())?;
    let canonical_root = layout
        .generation_root()
        .canonicalize()
        .map_err(ActivationError::io)?;
    let target = layout.generation_dir(candidate);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(ActivationError::CandidateAlreadyExists);
    }
    if let Some(prior) = prior {
        let source = layout.generation_dir(&prior.data);
        let canonical_source = source.canonicalize().map_err(ActivationError::io)?;
        if canonical_source.parent() != Some(canonical_root.as_path()) {
            return Err(ActivationError::UnsafeSnapshot);
        }
        copy_generation(&source, &target, policy, acl)?;
    } else {
        fs::create_dir(&target).map_err(ActivationError::io)?;
        acl.ensure_private(&target)?;
    }
    let canonical_target = target.canonicalize().map_err(ActivationError::io)?;
    if canonical_target.parent() != Some(canonical_root.as_path()) {
        return Err(ActivationError::UnsafeSnapshot);
    }
    Ok(())
}

impl RuntimeActivator {
    fn journal_path(&self, activation_id: &str) -> Result<PathBuf, ActivationError> {
        let safe = !activation_id.is_empty()
            && activation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !safe {
            return Err(ActivationError::InvalidJournal);
        }
        Ok(self.journal_root.join(format!("{activation_id}.json")))
    }

    fn write_journal(&self, journal: &ActivationJournal) -> Result<(), ActivationError> {
        fs::create_dir_all(&self.journal_root).map_err(ActivationError::io)?;
        let destination = self.journal_path(&journal.activation_id)?;
        atomic_json_write(
            &self.journal_root,
            &destination,
            &format!(".{}.tmp", journal.activation_id),
            journal,
        )
    }

    fn load_pending_journal(&self) -> Result<Option<ActivationJournal>, ActivationError> {
        let entries = match fs::read_dir(&self.journal_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ActivationError::io(error)),
        };
        let mut pending = Vec::new();
        for entry in entries {
            let entry = entry.map_err(ActivationError::io)?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(ActivationError::io)?;
            let journal: ActivationJournal =
                serde_json::from_slice(&bytes).map_err(|_| ActivationError::InvalidJournal)?;
            if !matches!(journal.schema, LEGACY_ACTIVATION_SCHEMA | ACTIVATION_SCHEMA) {
                return Err(ActivationError::InvalidJournal);
            }
            if matches!(
                journal.state,
                JournalState::Prepared
                    | JournalState::Committed
                    | JournalState::RollingBack
                    | JournalState::RecoveryRequired
            ) {
                pending.push(journal);
            }
        }
        if pending.len() > 1 {
            return Err(ActivationError::RecoveryRequired {
                failure: ActivationFailure::interrupted_resume(),
                recovery_code: "multiple_pending_journals".to_owned(),
            });
        }
        Ok(pending.pop())
    }
}

fn atomic_json_write<T: Serialize>(
    parent: &Path,
    destination: &Path,
    temporary_name: &str,
    value: &T,
) -> Result<(), ActivationError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ActivationError::InvalidJournal)?;
    let temporary = parent.join(temporary_name);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(ActivationError::io)?;
    file.write_all(&bytes).map_err(ActivationError::io)?;
    file.sync_all().map_err(ActivationError::io)?;
    drop(file);
    replace_file(&temporary, destination).map_err(ActivationError::io)
}

fn copy_generation(
    source: &Path,
    target: &Path,
    policy: SnapshotPolicy,
    acl: &dyn SnapshotAclInspector,
) -> Result<(), ActivationError> {
    validate_plain_dir(source)?;
    let (files, bytes) = measure_generation(source, policy)?;
    let available = available_bytes(source)?;
    if files > policy.max_files
        || bytes > policy.max_bytes
        || available < bytes.saturating_add(policy.required_free_bytes)
    {
        return Err(ActivationError::SnapshotLimit);
    }
    fs::create_dir(target).map_err(ActivationError::io)?;
    acl.ensure_private(target)?;
    let mut names = SnapshotPathRegistry::default();
    let mut stack = vec![(source.to_path_buf(), target.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        for entry in fs::read_dir(&from).map_err(ActivationError::io)? {
            let entry = entry.map_err(ActivationError::io)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(ActivationError::io)?;
            if metadata.file_type().is_symlink() || has_reparse_point(&metadata) {
                return Err(ActivationError::UnsafeSnapshot);
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(source)
                .map_err(|_| ActivationError::UnsafeSnapshot)?;
            names.register(relative)?;
            let destination = to.join(entry.file_name());
            if metadata.is_dir() {
                fs::create_dir(&destination).map_err(ActivationError::io)?;
                stack.push((entry.path(), destination));
            } else if metadata.is_file() && !has_multiple_links(&entry.path()) {
                fs::copy(entry.path(), &destination).map_err(ActivationError::io)?;
                if is_sensitive_file(&destination) {
                    acl.ensure_private(&destination)?;
                }
            } else {
                return Err(ActivationError::UnsafeSnapshot);
            }
        }
    }
    Ok(())
}

fn measure_generation(
    source: &Path,
    policy: SnapshotPolicy,
) -> Result<(u64, u64), ActivationError> {
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut names = SnapshotPathRegistry::default();
    let mut stack = vec![source.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory).map_err(ActivationError::io)? {
            let entry = entry.map_err(ActivationError::io)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(ActivationError::io)?;
            if metadata.file_type().is_symlink() || has_reparse_point(&metadata) {
                return Err(ActivationError::UnsafeSnapshot);
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(source)
                .map_err(|_| ActivationError::UnsafeSnapshot)?;
            names.register(relative)?;
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() && !has_multiple_links(&entry.path()) {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(metadata.len());
                if files > policy.max_files || bytes > policy.max_bytes {
                    return Err(ActivationError::SnapshotLimit);
                }
            } else {
                return Err(ActivationError::UnsafeSnapshot);
            }
        }
    }
    Ok((files, bytes))
}

#[derive(Default)]
struct SnapshotPathRegistry {
    folded: HashSet<String>,
}

impl SnapshotPathRegistry {
    fn register(&mut self, relative: &Path) -> Result<(), ActivationError> {
        let mut folded = String::new();
        for component in relative.components() {
            let std::path::Component::Normal(value) = component else {
                return Err(ActivationError::UnsafeSnapshot);
            };
            let value = value.to_str().ok_or(ActivationError::UnsafeSnapshot)?;
            // NTFS ADS 与 Win32 尾随点/空格会产生不同名称映射，快照必须拒绝。
            if value.contains(':') || value.ends_with('.') || value.ends_with(' ') {
                return Err(ActivationError::UnsafeSnapshot);
            }
            if !folded.is_empty() {
                folded.push('/');
            }
            folded.push_str(&value.to_lowercase());
        }
        if folded.is_empty() || !self.folded.insert(folded) {
            return Err(ActivationError::UnsafeSnapshot);
        }
        Ok(())
    }
}

fn validate_plain_dir(path: &Path) -> Result<(), ActivationError> {
    let metadata = fs::symlink_metadata(path).map_err(ActivationError::io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || has_reparse_point(&metadata) {
        return Err(ActivationError::UnsafeSnapshot);
    }
    Ok(())
}

fn is_sensitive_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|value| value.to_str()),
        Some(".env" | ".credentials.yaml" | ".credentials.yml")
    )
}

#[cfg(windows)]
fn validate_private_acl(path: &Path) -> Result<(), ActivationError> {
    crate::update::probe::ensure_private_windows_dacl(path)
        .map_err(|_| ActivationError::UnsafeSnapshot)
}

#[cfg(not(windows))]
fn validate_private_acl(_path: &Path) -> Result<(), ActivationError> {
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

#[cfg(windows)]
fn available_bytes(path: &Path) -> Result<u64, ActivationError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::PCWSTR;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut available = 0_u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }
        .map_err(|error| ActivationError::Io(std::io::Error::other(error.to_string())))?;
    Ok(available)
}

#[cfg(not(windows))]
fn available_bytes(_path: &Path) -> Result<u64, ActivationError> {
    Ok(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        ActivationCheckpoint, ActivationCheckpointSink, ActivationOutcome, ActivationProbe,
        ActivationRequest, RuntimeActivator, SnapshotPathRegistry, SnapshotPolicy,
    };
    use crate::app_controller::{AppController, ProbeLease, RuntimeLifecycle};
    use crate::paths::{AppPaths, RuntimeLayout};
    use crate::runtime::RuntimeError;
    use crate::runtime::install_state::{
        ActiveDeployment, DataGeneration, InstallStateError, InstallStateStore, InstalledRuntime,
    };
    use futures_util::FutureExt;
    use futures_util::future::BoxFuture;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct Fixture {
        layout: RuntimeLayout,
        workspace: PathBuf,
        old: ActiveDeployment,
        new_runtime: InstalledRuntime,
        candidate: DataGeneration,
    }

    impl Fixture {
        fn new(label: &str, with_prior: bool) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("dsh-activation-{label}-{nonce}"));
            let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
            let layout = RuntimeLayout::from_paths(&paths);
            let workspace_input = root.join("workspace");
            fs::create_dir_all(&workspace_input).expect("workspace");
            let workspace = workspace_input.canonicalize().expect("canonical workspace");
            let old_runtime =
                InstalledRuntime::with_node_version("0.1.1-rc.1", "a".repeat(64), "24.15.0")
                    .expect("old runtime");
            let old_data = DataGeneration::new("generation-old").expect("old generation");
            let old = ActiveDeployment::with_project_workspace(
                old_runtime,
                old_data,
                "2026-08-22T00:00:00Z".to_owned(),
                workspace.clone(),
            );
            let new_runtime =
                InstalledRuntime::with_node_version("0.1.2", "b".repeat(64), "24.15.0")
                    .expect("new runtime");
            fs::create_dir_all(layout.runtime_dir(&new_runtime)).expect("new runtime dir");
            if with_prior {
                fs::create_dir_all(layout.runtime_dir(&old.runtime)).expect("old runtime dir");
                fs::create_dir_all(layout.generation_dir(&old.data)).expect("old generation dir");
                fs::write(layout.generation_dir(&old.data).join("memory.db"), b"old")
                    .expect("old data");
                InstallStateStore::new(layout.clone())
                    .save(&old)
                    .expect("old pointer");
            }
            Self {
                layout,
                workspace,
                old,
                new_runtime,
                candidate: DataGeneration::new("generation-new").expect("candidate"),
            }
        }

        fn request(&self) -> ActivationRequest {
            ActivationRequest {
                runtime: self.new_runtime.clone(),
                candidate: self.candidate.clone(),
                activated_at: "2026-08-22T01:00:00Z".to_owned(),
                trace_id: "activation-test-001".to_owned(),
            }
        }
    }

    #[derive(Default)]
    struct RecordingRuntime {
        calls: Mutex<Vec<String>>,
        fail_version: Mutex<Option<String>>,
        fail_all: Mutex<bool>,
    }

    impl RuntimeLifecycle for RecordingRuntime {
        fn start(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn stop(&self) -> Result<(), RuntimeError> {
            self.calls.lock().expect("calls").push("stop".to_owned());
            Ok(())
        }

        fn start_and_wait_ready(&self, deployment: &ActiveDeployment) -> Result<(), RuntimeError> {
            let version = deployment.runtime.version.to_string();
            self.calls
                .lock()
                .expect("calls")
                .push(format!("start:{version}:{}", deployment.data.id));
            if *self.fail_all.lock().expect("fail all")
                || self.fail_version.lock().expect("fail version").as_deref() == Some(&version)
            {
                Err(RuntimeError::Tauri("首启失败".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    struct PassedProbe;

    impl ActivationProbe for PassedProbe {
        fn probe<'a>(
            &'a self,
            layout: &'a RuntimeLayout,
            runtime: &'a InstalledRuntime,
            candidate: &'a DataGeneration,
            _project_workspace: &'a Path,
            trace_id: &'a str,
            _lease: ProbeLease,
        ) -> BoxFuture<'a, Result<(), super::ActivationError>> {
            async move {
                let state = layout.generation_root().join(".state").join(&candidate.id);
                fs::create_dir_all(&state).map_err(super::ActivationError::io)?;
                let document = serde_json::json!({
                    "schema": 1,
                    "candidate_id": candidate.id,
                    "runtime_version": runtime.version.to_string(),
                    "manifest_digest": runtime.manifest_digest,
                    "state": "passed",
                    "trace_id": trace_id,
                });
                fs::write(
                    state.join("passed.json"),
                    serde_json::to_vec(&document).expect("state json"),
                )
                .map_err(super::ActivationError::io)
            }
            .boxed()
        }
    }

    struct NoCrash;

    struct PermissiveAcl;

    struct SlowAcl;

    struct ConfirmedIdleProvider;

    struct FailingDeploymentStore {
        inner: InstallStateStore,
        fail_restore_version: Option<String>,
        fail_mark_uninstalled: bool,
    }

    impl super::DeploymentStore for FailingDeploymentStore {
        fn load(&self) -> Result<ActiveDeployment, InstallStateError> {
            self.inner.load()
        }

        fn save(&self, deployment: &ActiveDeployment) -> Result<(), InstallStateError> {
            if self.fail_restore_version.as_deref() == Some(&deployment.runtime.version.to_string())
            {
                return Err(InstallStateError::Io {
                    operation: "injected pointer restore",
                    path: PathBuf::from("deployment.json"),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected failure",
                    ),
                });
            }
            self.inner.save(deployment)
        }

        fn mark_uninstalled(&self, changed_at: &str) -> Result<(), InstallStateError> {
            if self.fail_mark_uninstalled {
                return Err(InstallStateError::Io {
                    operation: "injected mark uninstalled",
                    path: PathBuf::from("deployment.json"),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected failure",
                    ),
                });
            }
            self.inner.mark_uninstalled(changed_at)
        }
    }

    impl super::RuntimeBusyProvider for ConfirmedIdleProvider {
        fn quiesce(&self) -> super::RuntimeBusyState {
            super::RuntimeBusyState::ConfirmedIdle
        }
    }

    impl super::SnapshotAclInspector for PermissiveAcl {
        fn ensure_private(&self, _path: &Path) -> Result<(), super::ActivationError> {
            Ok(())
        }
    }

    impl super::SnapshotAclInspector for SlowAcl {
        fn ensure_private(&self, _path: &Path) -> Result<(), super::ActivationError> {
            std::thread::sleep(Duration::from_millis(150));
            Ok(())
        }
    }

    impl ActivationCheckpointSink for NoCrash {
        fn reached(&self, _checkpoint: ActivationCheckpoint) -> Result<(), super::ActivationError> {
            Ok(())
        }
    }

    struct CrashAt(ActivationCheckpoint);

    impl ActivationCheckpointSink for CrashAt {
        fn reached(&self, checkpoint: ActivationCheckpoint) -> Result<(), super::ActivationError> {
            if checkpoint == self.0 {
                Err(super::ActivationError::Interrupted { checkpoint })
            } else {
                Ok(())
            }
        }
    }

    fn controller(runtime: Arc<RecordingRuntime>) -> AppController {
        AppController::for_test_with_busy(runtime, Arc::new(ConfirmedIdleProvider))
    }

    fn activator(fixture: &Fixture) -> RuntimeActivator {
        let activator = RuntimeActivator::new(fixture.layout.clone(), SnapshotPolicy::default())
            .expect("activator")
            .with_acl_inspector(Arc::new(PermissiveAcl));
        activator
            .save_trusted_workspace(&fixture.workspace)
            .expect("trusted workspace");
        activator
    }

    #[tokio::test]
    async fn activation_copies_after_session_stops_and_commits_exact_runtime_data_pair() {
        let fixture = Fixture::new("success", true);
        let runtime = Arc::new(RecordingRuntime::default());
        let session = controller(runtime.clone())
            .begin_activation()
            .expect("session");
        let outcome = activator(&fixture)
            .activate(session, fixture.request(), &PassedProbe, &NoCrash)
            .await
            .expect("activation");

        assert_eq!(outcome, ActivationOutcome::Activated);
        assert_eq!(
            fs::read(
                fixture
                    .layout
                    .generation_dir(&fixture.candidate)
                    .join("memory.db")
            )
            .expect("copied data"),
            b"old"
        );
        let active = InstallStateStore::new(fixture.layout.clone())
            .load()
            .expect("active pointer");
        assert_eq!(active.runtime, fixture.new_runtime);
        assert_eq!(active.data, fixture.candidate);
    }

    #[tokio::test]
    async fn failed_first_start_restores_old_pair_and_starts_old_runtime_once() {
        let fixture = Fixture::new("rollback", true);
        let runtime = Arc::new(RecordingRuntime::default());
        *runtime.fail_version.lock().expect("fail version") =
            Some(fixture.new_runtime.version.to_string());
        let session = controller(runtime.clone())
            .begin_activation()
            .expect("session");

        let outcome = activator(&fixture)
            .activate(session, fixture.request(), &PassedProbe, &NoCrash)
            .await
            .expect("controlled rollback");

        assert!(matches!(
            outcome,
            ActivationOutcome::RolledBack { ref failure }
                if failure.error_code == "tauri_error"
        ));
        assert_eq!(
            InstallStateStore::new(fixture.layout.clone())
                .load()
                .expect("old pointer"),
            fixture.old
        );
        assert_eq!(
            runtime.calls.lock().expect("calls").as_slice(),
            [
                "start:0.1.2:generation-new",
                "stop",
                "start:0.1.1-rc.1:generation-old"
            ]
        );
    }

    #[tokio::test]
    async fn crash_after_rolling_back_journal_never_restarts_failed_candidate() {
        let fixture = Fixture::new("rolling-back-recover", true);
        let runtime = Arc::new(RecordingRuntime::default());
        *runtime.fail_version.lock().expect("fail version") =
            Some(fixture.new_runtime.version.to_string());
        let app = controller(runtime.clone());
        let session = app.begin_activation().expect("session");
        let activator = activator(&fixture);

        assert!(matches!(
            activator
                .activate(
                    session,
                    fixture.request(),
                    &PassedProbe,
                    &CrashAt(ActivationCheckpoint::RollingBackPersisted),
                )
                .await,
            Err(super::ActivationError::Interrupted { .. })
        ));
        let recovery = app.begin_activation().expect("recovery session");
        assert!(matches!(
            activator
                .recover(recovery)
                .await
                .expect("rollback recovery"),
            ActivationOutcome::RolledBack { .. }
        ));
        assert_eq!(
            runtime.calls.lock().expect("calls").as_slice(),
            [
                "start:0.1.2:generation-new",
                "stop",
                "start:0.1.1-rc.1:generation-old"
            ]
        );
    }

    #[tokio::test]
    async fn pointer_restore_failure_persists_recovery_required_for_next_start() {
        let fixture = Fixture::new("restore-persist-failure", true);
        let runtime = Arc::new(RecordingRuntime::default());
        *runtime.fail_version.lock().expect("fail version") =
            Some(fixture.new_runtime.version.to_string());
        let app = controller(runtime);
        let session = app.begin_activation().expect("session");
        let store = Arc::new(FailingDeploymentStore {
            inner: InstallStateStore::new(fixture.layout.clone()),
            fail_restore_version: Some(fixture.old.runtime.version.to_string()),
            fail_mark_uninstalled: false,
        });
        let activator = activator(&fixture).with_store(store);

        let error = activator
            .activate(session, fixture.request(), &PassedProbe, &NoCrash)
            .await
            .expect_err("restore must fail closed");
        assert!(matches!(
            error,
            super::ActivationError::RecoveryRequired { ref recovery_code, .. }
                if recovery_code == "pointer_restore_failed"
        ));
        let recovery = app.begin_activation().expect("recovery session");
        assert!(matches!(
            activator.recover(recovery).await,
            Err(super::ActivationError::RecoveryRequired { ref recovery_code, .. })
                if recovery_code == "persisted_recovery_required"
        ));
    }

    #[tokio::test]
    async fn fresh_uninstalled_pointer_failure_persists_recovery_required() {
        let fixture = Fixture::new("fresh-persist-failure", false);
        let runtime = Arc::new(RecordingRuntime::default());
        *runtime.fail_version.lock().expect("fail version") =
            Some(fixture.new_runtime.version.to_string());
        let app = controller(runtime);
        let session = app.begin_activation().expect("session");
        let store = Arc::new(FailingDeploymentStore {
            inner: InstallStateStore::new(fixture.layout.clone()),
            fail_restore_version: None,
            fail_mark_uninstalled: true,
        });
        let activator = activator(&fixture).with_store(store);

        assert!(matches!(
            activator
                .activate(session, fixture.request(), &PassedProbe, &NoCrash)
                .await,
            Err(super::ActivationError::RecoveryRequired { ref recovery_code, .. })
                if recovery_code == "mark_uninstalled_failed"
        ));
        let recovery = app.begin_activation().expect("recovery session");
        assert!(matches!(
            activator.recover(recovery).await,
            Err(super::ActivationError::RecoveryRequired { .. })
        ));
    }

    #[tokio::test]
    async fn fresh_install_failure_keeps_runtime_and_generation_but_persists_uninstalled() {
        let fixture = Fixture::new("fresh", false);
        let runtime = Arc::new(RecordingRuntime::default());
        *runtime.fail_version.lock().expect("fail version") =
            Some(fixture.new_runtime.version.to_string());
        let app = controller(runtime.clone());
        let session = app.begin_activation().expect("session");
        let activator = activator(&fixture);
        let outcome = activator
            .activate(session, fixture.request(), &PassedProbe, &NoCrash)
            .await
            .expect("fresh failure is controlled");

        assert!(matches!(
            outcome,
            ActivationOutcome::FreshInstallFailed { ref failure }
                if failure.error_code == "tauri_error"
        ));
        assert!(matches!(
            InstallStateStore::new(fixture.layout.clone()).load(),
            Err(InstallStateError::NotInstalled)
        ));
        assert!(fixture.layout.runtime_dir(&fixture.new_runtime).is_dir());
        assert!(fixture.layout.generation_dir(&fixture.candidate).is_dir());
        assert_eq!(
            runtime.calls.lock().expect("calls").as_slice(),
            ["start:0.1.2:generation-new", "stop"]
        );
        let offline = app.begin_activation().expect("offline session");
        assert_eq!(
            activator.recover(offline).await.expect("terminal journal"),
            ActivationOutcome::NothingToRecover
        );
    }

    #[tokio::test]
    async fn rollback_start_failure_enters_recovery_required_without_retry_loop() {
        let fixture = Fixture::new("rollback-required", true);
        let runtime = Arc::new(RecordingRuntime::default());
        *runtime.fail_all.lock().expect("fail all") = true;
        let session = controller(runtime.clone())
            .begin_activation()
            .expect("session");

        let error = activator(&fixture)
            .activate(session, fixture.request(), &PassedProbe, &NoCrash)
            .await
            .expect_err("new and old startup failure requires recovery");

        assert!(matches!(
            error,
            super::ActivationError::RecoveryRequired { .. }
        ));
        assert_eq!(
            InstallStateStore::new(fixture.layout.clone())
                .load()
                .expect("old pointer"),
            fixture.old
        );
        assert_eq!(
            runtime.calls.lock().expect("calls").as_slice(),
            [
                "start:0.1.2:generation-new",
                "stop",
                "start:0.1.1-rc.1:generation-old"
            ]
        );
    }

    #[tokio::test]
    async fn session_refuses_to_start_when_authoritative_pointer_is_not_exact_target() {
        let fixture = Fixture::new("pointer-reload", true);
        let runtime = Arc::new(RecordingRuntime::default());
        let session = controller(runtime.clone())
            .begin_activation()
            .expect("session");
        let target = ActiveDeployment::with_project_workspace(
            fixture.new_runtime,
            fixture.candidate,
            "2026-08-22T01:00:00Z".to_owned(),
            fixture.workspace,
        );

        let actual = InstallStateStore::new(fixture.layout)
            .load()
            .expect("prior");
        assert!(matches!(
            session.start_and_wait_ready(&actual, &target).await,
            Err(RuntimeError::DeploymentChanged)
        ));
        assert!(runtime.calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn crash_after_pointer_replace_recovers_target_without_mixing_pair() {
        let fixture = Fixture::new("recover", true);
        let runtime = Arc::new(RecordingRuntime::default());
        let app = controller(runtime.clone());
        let session = app.begin_activation().expect("session");
        let activator = activator(&fixture);
        let error = activator
            .activate(
                session,
                fixture.request(),
                &PassedProbe,
                &CrashAt(ActivationCheckpoint::PointerCommitted),
            )
            .await
            .expect_err("simulated crash");
        assert!(matches!(error, super::ActivationError::Interrupted { .. }));
        let recovery = app.begin_activation().expect("recovery session");
        assert_eq!(
            activator.recover(recovery).await.expect("recover target"),
            ActivationOutcome::Activated
        );
        let active = InstallStateStore::new(fixture.layout.clone())
            .load()
            .expect("pointer");
        assert_eq!(
            (active.runtime, active.data),
            (fixture.new_runtime, fixture.candidate)
        );
    }

    #[tokio::test]
    async fn prepared_journal_crash_recovers_prior_pair_without_committing_target() {
        let fixture = Fixture::new("prepared-recover", true);
        let runtime = Arc::new(RecordingRuntime::default());
        let app = controller(runtime.clone());
        let session = app.begin_activation().expect("session");
        let activator = activator(&fixture);
        assert!(matches!(
            activator
                .activate(
                    session,
                    fixture.request(),
                    &PassedProbe,
                    &CrashAt(ActivationCheckpoint::JournalPrepared),
                )
                .await,
            Err(super::ActivationError::Interrupted { .. })
        ));
        assert_eq!(
            InstallStateStore::new(fixture.layout.clone())
                .load()
                .expect("prior"),
            fixture.old
        );
        let recovery = app.begin_activation().expect("recovery session");
        assert!(matches!(
            activator.recover(recovery).await.expect("recover prior"),
            ActivationOutcome::RolledBack { ref failure }
                if failure.error_code == "activation_interrupted"
        ));
        assert_eq!(
            runtime.calls.lock().expect("calls").as_slice(),
            ["start:0.1.1-rc.1:generation-old"]
        );
    }

    #[tokio::test]
    async fn crash_before_first_start_is_recovered_from_committed_journal() {
        let fixture = Fixture::new("first-start-crash", true);
        let runtime = Arc::new(RecordingRuntime::default());
        let app = controller(runtime.clone());
        let session = app.begin_activation().expect("session");
        let activator = activator(&fixture);
        assert!(matches!(
            activator
                .activate(
                    session,
                    fixture.request(),
                    &PassedProbe,
                    &CrashAt(ActivationCheckpoint::BeforeFirstStart),
                )
                .await,
            Err(super::ActivationError::Interrupted { .. })
        ));
        let recovery = app.begin_activation().expect("recovery session");
        assert_eq!(
            activator
                .recover(recovery)
                .await
                .expect("recover committed target"),
            ActivationOutcome::Activated
        );
        assert_eq!(
            runtime.calls.lock().expect("calls").as_slice(),
            ["start:0.1.2:generation-new"]
        );
    }

    #[tokio::test]
    async fn snapshot_limit_fails_before_probe_and_preserves_prior_pointer() {
        let fixture = Fixture::new("snapshot-limit", true);
        let runtime = Arc::new(RecordingRuntime::default());
        let session = controller(runtime).begin_activation().expect("session");
        let activator = RuntimeActivator::new(
            fixture.layout.clone(),
            SnapshotPolicy {
                max_files: 100,
                max_bytes: 1,
                required_free_bytes: 0,
            },
        )
        .expect("activator")
        .with_acl_inspector(Arc::new(PermissiveAcl));

        assert!(matches!(
            activator
                .activate(session, fixture.request(), &PassedProbe, &NoCrash)
                .await,
            Err(super::ActivationError::SnapshotLimit)
        ));
        assert_eq!(
            InstallStateStore::new(fixture.layout.clone())
                .load()
                .expect("prior"),
            fixture.old
        );
    }

    #[tokio::test]
    async fn large_snapshot_work_does_not_block_tokio_worker() {
        let fixture = Fixture::new("async-snapshot", true);
        let runtime = Arc::new(RecordingRuntime::default());
        let session = controller(runtime).begin_activation().expect("session");
        let activator = activator(&fixture).with_acl_inspector(Arc::new(SlowAcl));
        let started = Instant::now();
        let mut activation =
            Box::pin(activator.activate(session, fixture.request(), &PassedProbe, &NoCrash));

        tokio::select! {
            result = &mut activation => panic!("blocking snapshot completed too early: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(
            activation.await.expect("activation"),
            ActivationOutcome::Activated
        );
    }

    #[test]
    fn snapshot_names_reject_ads_and_windows_case_folded_collisions() {
        let mut names = SnapshotPathRegistry::default();
        names
            .register(Path::new("Data/File.txt"))
            .expect("first spelling");
        assert!(matches!(
            names.register(Path::new("data/file.TXT")),
            Err(super::ActivationError::UnsafeSnapshot)
        ));

        let mut names = SnapshotPathRegistry::default();
        assert!(matches!(
            names.register(Path::new("secret.txt:stream")),
            Err(super::ActivationError::UnsafeSnapshot)
        ));
    }
}
