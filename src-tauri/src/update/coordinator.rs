use std::{
    collections::BTreeSet,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt, sync::Mutex, task::JoinHandle};

use super::{
    manifest::{CoreCompatibility, SkinCompatibility, VerifiedManifest},
    version_source::{CompatibilitySource, OfficialVersionSource, SourceError},
};
use crate::diagnostics::{DiagnosticContext, DiagnosticErrorKind, DiagnosticStage};
use crate::domain::UpdateNotice;

const UPDATE_STATE_SCHEMA: u32 = 1;
static UPDATE_CHECK_LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 启动与周期检查策略；默认启动后短暂延迟，并每 12 小时检查一次。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateSchedule {
    pub startup_delay: Duration,
    pub interval: Duration,
}

impl Default for UpdateSchedule {
    fn default() -> Self {
        Self {
            startup_delay: Duration::from_secs(20),
            interval: Duration::from_secs(12 * 60 * 60),
        }
    }
}

/// 可注入的整数 epoch 时间源，避免状态测试依赖真实时钟。
pub trait UpdateTimeSource: Send + Sync {
    /// 返回 Unix epoch 秒。
    ///
    /// :return: 当前 UTC epoch 秒。
    /// :raises UpdateStateError: 系统时间早于 epoch 时返回。
    fn now_epoch_secs(&self) -> Result<u64, UpdateStateError>;
}

/// 生产系统时钟。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemUpdateTimeSource;

impl UpdateTimeSource for SystemUpdateTimeSource {
    fn now_epoch_secs(&self) -> Result<u64, UpdateStateError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| UpdateStateError::Clock)
    }
}

/// 更新检查状态文件，保存去重键与下一次检查时间。
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedUpdateState {
    pub schema: u32,
    pub next_check_epoch_secs: u64,
    pub notification_keys: BTreeSet<String>,
}

impl Default for PersistedUpdateState {
    fn default() -> Self {
        Self {
            schema: UPDATE_STATE_SCHEMA,
            next_check_epoch_secs: 0,
            notification_keys: BTreeSet::new(),
        }
    }
}

impl PersistedUpdateState {
    /// 严格解析持久化状态。
    ///
    /// :param bytes: 状态文件原始字节。
    /// :return: schema 受支持的状态。
    /// :raises UpdateStateError: 截断、未知字段或未知 schema 时返回。
    pub fn from_json(bytes: &[u8]) -> Result<Self, UpdateStateError> {
        let state: Self =
            serde_json::from_slice(bytes).map_err(|_| UpdateStateError::InvalidState)?;
        if state.schema != UPDATE_STATE_SCHEMA {
            return Err(UpdateStateError::UnsupportedSchema {
                schema: state.schema,
            });
        }
        Ok(state)
    }

    /// 序列化状态，不写入 URL、token 或响应正文。
    ///
    /// :return: 紧凑 JSON 字节。
    /// :raises UpdateStateError: 序列化失败时返回。
    pub fn to_json(&self) -> Result<Vec<u8>, UpdateStateError> {
        serde_json::to_vec(self).map_err(|_| UpdateStateError::InvalidState)
    }
}

/// 状态持久化或调度失败的稳定原因。
#[derive(Debug, Error)]
pub enum UpdateStateError {
    #[error("更新状态文件无效")]
    InvalidState,
    #[error("不支持的更新状态 schema: {schema}")]
    UnsupportedSchema { schema: u32 },
    #[error("更新状态文件访问失败")]
    FileSystem,
    #[error("更新状态临时文件已存在")]
    TemporaryFileExists,
    #[error("系统时间无效")]
    Clock,
    #[error("更新调度策略无效")]
    InvalidSchedule,
}

/// 原子状态文件存储；临时文件冲突时稳定失败，不覆盖来源不明的文件。
#[derive(Clone, Debug)]
pub struct UpdateStateStore {
    state_file: PathBuf,
}

impl UpdateStateStore {
    /// 创建指定设置目录下的 update-state 存储。
    ///
    /// :param settings_dir: 应用私有设置目录。
    /// :return: 尚未访问文件系统的存储对象。
    /// :raises: 此构造器不访问文件系统。
    pub fn new(settings_dir: &Path) -> Self {
        Self {
            state_file: settings_dir.join("update-state.json"),
        }
    }

    /// 异步加载状态；文件不存在表示首次检查。
    ///
    /// :return: 已保存状态或默认状态。
    /// :raises UpdateStateError: 文件读取或严格解析失败时返回。
    pub async fn load(&self) -> Result<PersistedUpdateState, UpdateStateError> {
        match fs::read(&self.state_file).await {
            Ok(bytes) => PersistedUpdateState::from_json(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(PersistedUpdateState::default())
            }
            Err(_) => Err(UpdateStateError::FileSystem),
        }
    }

    /// flush 临时文件后原子替换正式状态。
    ///
    /// :param state: 待持久化状态。
    /// :return: 文件提交完成时返回 `()`。
    /// :raises UpdateStateError: 临时文件存在、写入或原子替换失败时返回。
    pub async fn save(&self, state: &PersistedUpdateState) -> Result<(), UpdateStateError> {
        let parent = self
            .state_file
            .parent()
            .ok_or(UpdateStateError::FileSystem)?;
        fs::create_dir_all(parent)
            .await
            .map_err(|_| UpdateStateError::FileSystem)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| UpdateStateError::Clock)?
            .as_nanos();
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        // 临时名只由数值组成且固定在 state 文件父目录；崩溃残留会保留诊断价值，
        // 后续保存使用新 nonce，不需要删除或覆盖残留文件。
        let temporary = parent.join(format!(
            "update-state.json.tmp-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    UpdateStateError::TemporaryFileExists
                } else {
                    UpdateStateError::FileSystem
                }
            })?;
        file.write_all(&state.to_json()?)
            .await
            .map_err(|_| UpdateStateError::FileSystem)?;
        file.sync_all()
            .await
            .map_err(|_| UpdateStateError::FileSystem)?;
        drop(file);
        let destination = self.state_file.clone();
        tokio::task::spawn_blocking(move || atomic_replace(&temporary, &destination))
            .await
            .map_err(|_| UpdateStateError::FileSystem)??;
        Ok(())
    }
}

/// 一次检查的通知、可安装清单和持久化去重结论。
#[derive(Clone, Debug)]
pub struct UpdateCheckResult {
    pub notice: UpdateNotice,
    pub compatible_manifest: Option<VerifiedManifest>,
    pub should_notify: bool,
    pub next_check_epoch_secs: u64,
}

/// 启动延迟检查的结果；未到期与实际网络检查明确分离。
#[derive(Clone, Debug)]
pub enum ScheduledCheckResult {
    Skipped { next_check_epoch_secs: u64 },
    Checked(Box<UpdateCheckResult>),
}

/// 将官方发现、签名兼容源和通知状态组合在一起的异步协调器。
pub struct UpdateCoordinator {
    official: Arc<dyn OfficialVersionSource>,
    compatibility: Arc<dyn CompatibilitySource>,
    state_store: UpdateStateStore,
    time: Arc<dyn UpdateTimeSource>,
    schedule: UpdateSchedule,
    channel: String,
    check_lock: Arc<Mutex<()>>,
}

impl UpdateCoordinator {
    /// 创建更新协调器。
    ///
    /// :param official: 只负责官方版本发现的 source。
    /// :param compatibility: 只返回签名验证成功清单的 source。
    /// :param state_store: 去重和调度状态存储。
    /// :param time: 可注入时间源。
    /// :param schedule: 启动延迟和周期。
    /// :param channel: 通知渠道，例如 `stable`。
    /// :return: 配置有效的协调器。
    /// :raises UpdateStateError: 周期为零或 channel 非安全标识时返回。
    pub fn new(
        official: Arc<dyn OfficialVersionSource>,
        compatibility: Arc<dyn CompatibilitySource>,
        state_store: UpdateStateStore,
        time: Arc<dyn UpdateTimeSource>,
        schedule: UpdateSchedule,
        channel: String,
    ) -> Result<Self, UpdateStateError> {
        if schedule.interval.as_secs() == 0
            || schedule.interval.subsec_nanos() != 0
            || schedule.interval > Duration::from_secs(30 * 24 * 60 * 60)
            || schedule.startup_delay > Duration::from_secs(60 * 60)
            || channel.is_empty()
            || !channel
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(UpdateStateError::InvalidSchedule);
        }
        Ok(Self {
            official,
            compatibility,
            state_store,
            time,
            schedule,
            channel,
            // 当前桌面进程只有一个状态文件命名空间；所有协调器实例共享这把锁，
            // 防止 startup/tray/manual 入口分别构造对象时丢失通知键。
            check_lock: UPDATE_CHECK_LOCK
                .get_or_init(|| Arc::new(Mutex::new(())))
                .clone(),
        })
    }

    /// 检查官方与兼容源；网络、超时、限流和服务不可用产生 `Offline`，
    /// 签名验证、响应格式或配置错误产生 `CheckFailed`，两者都不会伪装为无更新。
    ///
    /// :param current: 当前已安装 DSH 版本；首次安装为 `None`。
    /// :return: 通知、可选受信清单、去重结论和下次检查时间。
    /// :raises UpdateStateError: 状态文件或时钟失败时返回。
    #[cfg(test)]
    async fn check(
        &self,
        current: Option<&Version>,
    ) -> Result<UpdateCheckResult, UpdateStateError> {
        self.check_with_context(
            current,
            &DiagnosticContext::noop(crate::diagnostics::TraceKind::Update),
        )
        .await
    }

    /// 使用共享诊断上下文执行官方与兼容检查。
    ///
    /// :param current: 当前已安装版本，首次安装为 None。
    /// :param diagnostics: 同一次更新操作共享的类型化上下文。
    /// :return: 更新通知、受信清单与持久化调度结果。
    /// :raises UpdateStateError: 状态或时钟失败时返回稳定错误。
    pub async fn check_with_context(
        &self,
        current: Option<&Version>,
        diagnostics: &DiagnosticContext,
    ) -> Result<UpdateCheckResult, UpdateStateError> {
        let started = std::time::Instant::now();
        diagnostics.record(DiagnosticStage::UpdateCheck, 0, 0, None, None);
        let _guard = self.check_lock.lock().await;
        let result = match self.state_store.load().await {
            Ok(state) => self.check_with_state(current, state, diagnostics).await,
            Err(error) => Err(error),
        };
        diagnostics.record(
            DiagnosticStage::UpdateCheck,
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

    async fn check_with_state(
        &self,
        current: Option<&Version>,
        mut state: PersistedUpdateState,
        diagnostics: &DiagnosticContext,
    ) -> Result<UpdateCheckResult, UpdateStateError> {
        let official_started = std::time::Instant::now();
        diagnostics.record(DiagnosticStage::OfficialCheck, 0, 0, None, None);
        let official_result = self.official.latest().await;
        diagnostics.record(
            DiagnosticStage::OfficialCheck,
            official_started.elapsed().as_millis() as u64,
            0,
            None,
            official_result
                .as_ref()
                .err()
                .map(|_| DiagnosticErrorKind::UpdateFailure),
        );
        let (notice, manifest) = match official_result {
            Err(error) => (failed_notice(current, None, &error), None),
            Ok(official) => {
                let compatibility_started = std::time::Instant::now();
                diagnostics.record(DiagnosticStage::CompatibilityCheck, 0, 0, None, None);
                let compatibility_result = self
                    .compatibility
                    .latest_compatible_with_context(diagnostics)
                    .await;
                diagnostics.record(
                    DiagnosticStage::CompatibilityCheck,
                    compatibility_started.elapsed().as_millis() as u64,
                    0,
                    None,
                    compatibility_result
                        .as_ref()
                        .err()
                        .map(|_| DiagnosticErrorKind::UpdateFailure),
                );
                match compatibility_result {
                    Err(error) => (
                        failed_notice(current, Some(&official.version), &error),
                        None,
                    ),
                    Ok(manifest) => (
                        decide_notice(current, &official.version, manifest.as_ref()),
                        manifest,
                    ),
                }
            }
        };
        let key = notification_key(&self.channel, &notice);
        let should_notify = insert_bounded_notification_key(&mut state.notification_keys, key);
        // 只有明确的可用状态才把受信 artifact 交给后续安装流程；等待、失败或已是
        // 最新时即使 endpoint 返回了旧/不相关清单，也不能形成旁路安装授权。
        let is_newer_candidate = manifest.as_ref().is_some_and(|verified| {
            current.is_none_or(|installed| verified.manifest.dsh_version > *installed)
        });
        let installable_manifest = if matches!(
            notice,
            UpdateNotice::RuntimeAvailable { .. } | UpdateNotice::SkinUnverified { .. }
        ) && is_newer_candidate
        {
            manifest
        } else {
            None
        };
        let now = self.time.now_epoch_secs()?;
        state.next_check_epoch_secs = now
            .checked_add(self.schedule.interval.as_secs())
            .ok_or(UpdateStateError::Clock)?;
        self.state_store.save(&state).await?;
        Ok(UpdateCheckResult {
            notice,
            compatible_manifest: installable_manifest,
            should_notify,
            next_check_epoch_secs: state.next_check_epoch_secs,
        })
    }

    /// 在 Tauri/Tokio runtime 上延迟启动检查，不阻塞 UI 线程。
    ///
    /// :param coordinator: 共享协调器。
    /// :param current: 当前安装版本快照。
    /// :param diagnostics: 由启动入口创建并贯穿定时检查的类型化上下文。
    /// :return: 可观测的后台任务句柄。
    /// :raises UpdateStateError: 由任务结果返回，而非在 UI 线程抛出。
    pub fn spawn_startup_check(
        coordinator: Arc<Self>,
        current: Option<Version>,
        diagnostics: DiagnosticContext,
    ) -> JoinHandle<Result<ScheduledCheckResult, UpdateStateError>> {
        tokio::spawn(async move {
            tokio::time::sleep(coordinator.schedule.startup_delay).await;
            coordinator
                .check_if_due(current.as_ref(), &diagnostics)
                .await
        })
    }

    /// 仅在持久化截止时间到期时执行检查；到期判断与状态提交共用同一进程锁。
    ///
    /// :param current: 当前安装版本快照。
    /// :param diagnostics: 与实际网络检查共享的类型化上下文。
    /// :return: 未到期时返回 `Skipped`，到期时返回实际检查结果。
    /// :raises UpdateStateError: 状态、时钟或检查后的状态提交失败时返回。
    pub async fn check_if_due(
        &self,
        current: Option<&Version>,
        diagnostics: &DiagnosticContext,
    ) -> Result<ScheduledCheckResult, UpdateStateError> {
        let started = std::time::Instant::now();
        diagnostics.record(DiagnosticStage::UpdateCheck, 0, 0, None, None);
        let _guard = self.check_lock.lock().await;
        let state = self.state_store.load().await?;
        if self.time.now_epoch_secs()? < state.next_check_epoch_secs {
            diagnostics.record(
                DiagnosticStage::UpdateCheck,
                started.elapsed().as_millis() as u64,
                0,
                None,
                None,
            );
            return Ok(ScheduledCheckResult::Skipped {
                next_check_epoch_secs: state.next_check_epoch_secs,
            });
        }
        let result = self
            .check_with_state(current, state, diagnostics)
            .await
            .map(Box::new)
            .map(ScheduledCheckResult::Checked);
        diagnostics.record(
            DiagnosticStage::UpdateCheck,
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

    /// 返回只读的到期提示，供设置页展示；不作为发起检查的前置门禁。
    ///
    /// 托盘和其他周期入口必须直接调用 `check_if_due`，由同一把锁原子完成到期判断与
    /// 检查；若先调用本方法再调用 `check`，两步之间会存在竞态并可能重复网络请求。
    ///
    /// :return: 当前时刻已到达 next-check 时为 `true`。
    /// :raises UpdateStateError: 状态或时钟读取失败时返回。
    pub fn is_due<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, UpdateStateError>> + Send + 'a>> {
        Box::pin(async move {
            let state = self.state_store.load().await?;
            Ok(self.time.now_epoch_secs()? >= state.next_check_epoch_secs)
        })
    }

    /// 系统通知无法提交时释放对应去重键，允许下次检查重试。
    ///
    /// :param notice: 本次检查产生且尚未成功交付的类型化通知。
    /// :return: 去重状态原子保存完成时返回。
    /// :raises UpdateStateError: 状态读取、时钟或文件保存失败时返回。
    pub async fn release_notification(
        &self,
        notice: &UpdateNotice,
    ) -> Result<(), UpdateStateError> {
        let _guard = self.check_lock.lock().await;
        let mut state = self.state_store.load().await?;
        state
            .notification_keys
            .remove(&notification_key(&self.channel, notice));
        self.state_store.save(&state).await
    }
}

fn failed_notice(
    current: Option<&Version>,
    discovered: Option<&Version>,
    error: &SourceError,
) -> UpdateNotice {
    let (error_kind, is_offline) = match error {
        SourceError::Network => ("network", true),
        SourceError::Timeout => ("timeout", true),
        SourceError::ResponseTooLarge => ("response_too_large", false),
        SourceError::RateLimited => ("rate_limited", true),
        SourceError::ServerUnavailable => ("server_unavailable", true),
        SourceError::CompatibilityVerification => ("compatibility_verification", false),
        SourceError::CompatibilityUnavailable => ("compatibility_unavailable", false),
        SourceError::InvalidConfiguration => ("configuration", false),
        SourceError::HttpStatus { .. }
        | SourceError::InvalidResponse
        | SourceError::InvalidVersion
        | SourceError::InvalidIntegrity => ("invalid_response", false),
    };
    let fields = (
        current.map(ToString::to_string),
        discovered.map(ToString::to_string),
        error_kind.to_owned(),
    );
    if is_offline {
        UpdateNotice::Offline {
            current: fields.0,
            version: fields.1,
            error_kind: fields.2,
        }
    } else {
        UpdateNotice::CheckFailed {
            current: fields.0,
            version: fields.1,
            error_kind: fields.2,
        }
    }
}

fn decide_notice(
    current: Option<&Version>,
    official: &Version,
    compatible: Option<&VerifiedManifest>,
) -> UpdateNotice {
    let current_text = current.map(ToString::to_string);
    if let (Some(installed), Some(compatible)) = (current, compatible)
        && compatible.manifest.dsh_version == *installed
        && compatible.manifest.skin_compatibility == SkinCompatibility::Unverified
        && compatible.manifest.core_compatibility == CoreCompatibility::Compatible
        && compatible.desktop_version_supported
        && installed <= official
    {
        return UpdateNotice::SkinUnverified {
            current: current_text,
            official: official.to_string(),
            compatible: compatible.manifest.dsh_version.to_string(),
        };
    }
    if current.is_some_and(|installed| installed >= official) {
        return UpdateNotice::UpToDate {
            current: current_text,
            official: official.to_string(),
        };
    }
    let Some(compatible) = compatible else {
        return UpdateNotice::OfficialAvailable {
            current: current_text,
            official: official.to_string(),
        };
    };
    let candidate = &compatible.manifest;
    if candidate.dsh_version > *official {
        return UpdateNotice::CheckFailed {
            current: current_text,
            version: Some(candidate.dsh_version.to_string()),
            error_kind: "compatibility_not_official".to_owned(),
        };
    }
    if current.is_some_and(|installed| candidate.dsh_version <= *installed) {
        return UpdateNotice::OfficialAvailable {
            current: current_text,
            official: official.to_string(),
        };
    }
    if candidate.core_compatibility == CoreCompatibility::DesktopRequired
        || !compatible.desktop_version_supported
    {
        return UpdateNotice::DesktopRequired {
            current: current_text,
            official: official.to_string(),
            compatible: candidate.dsh_version.to_string(),
            minimum_desktop: candidate.minimum_desktop_version.to_string(),
        };
    }
    match candidate.skin_compatibility {
        SkinCompatibility::Verified => UpdateNotice::RuntimeAvailable {
            current: current_text,
            official: official.to_string(),
            compatible: candidate.dsh_version.to_string(),
        },
        SkinCompatibility::Unverified => UpdateNotice::SkinUnverified {
            current: current_text,
            official: official.to_string(),
            compatible: candidate.dsh_version.to_string(),
        },
    }
}

fn notification_key(channel: &str, notice: &UpdateNotice) -> String {
    match notice {
        UpdateNotice::UpToDate { official, .. } => format!("{channel}:{official}:up_to_date"),
        UpdateNotice::OfficialAvailable { official, .. } => {
            format!("{channel}:{official}:official_available")
        }
        UpdateNotice::RuntimeAvailable { compatible, .. } => {
            format!("{channel}:{compatible}:runtime_available")
        }
        UpdateNotice::DesktopRequired {
            compatible,
            minimum_desktop,
            ..
        } => format!("{channel}:{compatible}:{minimum_desktop}:desktop_required"),
        UpdateNotice::SkinUnverified { compatible, .. } => {
            format!("{channel}:{compatible}:skin_unverified")
        }
        UpdateNotice::Offline {
            version,
            error_kind,
            ..
        } => format!(
            "{channel}:{}:{error_kind}:offline",
            version.as_deref().unwrap_or("unknown")
        ),
        UpdateNotice::CheckFailed {
            version,
            error_kind,
            ..
        } => format!(
            "{channel}:{}:{error_kind}:check_failed",
            version.as_deref().unwrap_or("unknown")
        ),
    }
}

fn insert_bounded_notification_key(keys: &mut BTreeSet<String>, key: String) -> bool {
    const MAX_NOTIFICATION_KEYS: usize = 128;
    if keys.contains(&key) {
        return false;
    }
    if keys.len() >= MAX_NOTIFICATION_KEYS {
        // key 不含隐私数据；按稳定字典序回收一项即可维持磁盘与解析上限。
        keys.pop_first();
    }
    keys.insert(key)
}

#[cfg(windows)]
fn atomic_replace(temporary: &Path, destination: &Path) -> Result<(), UpdateStateError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{REPLACE_FILE_FLAGS, ReplaceFileW},
        core::PCWSTR,
    };

    if !destination.exists() {
        return std::fs::rename(temporary, destination).map_err(|_| UpdateStateError::FileSystem);
    }
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 两个 UTF-16 缓冲区均以 NUL 结尾，并在系统调用期间保持有效。
    unsafe {
        ReplaceFileW(
            PCWSTR(destination_wide.as_ptr()),
            PCWSTR(temporary_wide.as_ptr()),
            PCWSTR::null(),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    }
    .map_err(|_| UpdateStateError::FileSystem)
}

#[cfg(not(windows))]
fn atomic_replace(temporary: &Path, destination: &Path) -> Result<(), UpdateStateError> {
    std::fs::rename(temporary, destination).map_err(|_| UpdateStateError::FileSystem)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        future::Future,
        path::PathBuf,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use semver::Version;

    use super::{
        PersistedUpdateState, ScheduledCheckResult, UpdateCoordinator, UpdateSchedule,
        UpdateStateError, UpdateStateStore, UpdateTimeSource, decide_notice, failed_notice,
        notification_key,
    };
    use crate::diagnostics::{DiagnosticContext, DiagnosticEvent, DiagnosticSink, TraceKind};
    use crate::domain::UpdateNotice;
    use crate::update::{
        manifest::{
            CompatibilityManifest, CoreCompatibility, RuntimeArtifact, SkinCompatibility,
            VerifiedManifest,
        },
        version_source::{
            CompatibilitySource, OfficialRelease, OfficialVersionSource, SourceError,
        },
    };

    struct FixedOfficial(Result<OfficialRelease, SourceError>);

    #[derive(Default)]
    struct CountingDiagnostics(AtomicUsize);

    impl DiagnosticSink for CountingDiagnostics {
        fn record(&self, _event: DiagnosticEvent) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl OfficialVersionSource for FixedOfficial {
        fn latest<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<OfficialRelease, SourceError>> + Send + 'a>>
        {
            let result = self.0.clone();
            Box::pin(async move { result })
        }
    }

    struct FixedCompatibility(Result<Option<VerifiedManifest>, SourceError>);

    impl CompatibilitySource for FixedCompatibility {
        fn latest_compatible_with_context<'a>(
            &'a self,
            _diagnostics: &'a DiagnosticContext,
        ) -> Pin<Box<dyn Future<Output = Result<Option<VerifiedManifest>, SourceError>> + Send + 'a>>
        {
            let result = self.0.clone();
            Box::pin(async move { result })
        }
    }

    struct FixedTime(u64);

    impl UpdateTimeSource for FixedTime {
        fn now_epoch_secs(&self) -> Result<u64, UpdateStateError> {
            Ok(self.0)
        }
    }

    fn unique_settings(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("dsh-task7-{label}-{}-{nonce}", std::process::id()))
    }

    fn official(version: &str) -> Arc<dyn OfficialVersionSource> {
        Arc::new(FixedOfficial(Ok(OfficialRelease {
            version: Version::parse(version).unwrap(),
            integrity: "sha512-YWJjZA==".to_owned(),
        })))
    }

    fn no_compatibility() -> Arc<dyn CompatibilitySource> {
        Arc::new(FixedCompatibility(Ok(None)))
    }

    fn compatible(version: &str) -> Arc<dyn CompatibilitySource> {
        Arc::new(FixedCompatibility(Ok(Some(VerifiedManifest {
            manifest: CompatibilityManifest {
                schema: 1,
                dsh_version: Version::parse(version).unwrap(),
                node_version: Version::parse("24.15.0").unwrap(),
                minimum_desktop_version: Version::parse("0.1.0").unwrap(),
                core_compatibility: CoreCompatibility::Compatible,
                skin_compatibility: SkinCompatibility::Verified,
                platform: "windows".to_owned(),
                arch: "x86_64".to_owned(),
                artifact: RuntimeArtifact {
                    url: url::Url::parse("https://updates.example.invalid/runtime.zip").unwrap(),
                    size: 10,
                    sha256: [3_u8; 32],
                },
                verified_at: "2026-08-22T00:00:00Z".to_owned(),
                compatibility_summary: "verified".to_owned(),
            },
            manifest_digest: "a".repeat(64),
            desktop_version_supported: true,
        }))))
    }

    fn candidate(
        version: &str,
        minimum_desktop: &str,
        core: CoreCompatibility,
        skin: SkinCompatibility,
        desktop_version_supported: bool,
    ) -> VerifiedManifest {
        VerifiedManifest {
            manifest: CompatibilityManifest {
                schema: 2,
                dsh_version: Version::parse(version).unwrap(),
                node_version: Version::parse("24.15.0").unwrap(),
                minimum_desktop_version: Version::parse(minimum_desktop).unwrap(),
                core_compatibility: core,
                skin_compatibility: skin,
                platform: "windows".to_owned(),
                arch: "x86_64".to_owned(),
                artifact: RuntimeArtifact {
                    url: url::Url::parse("https://updates.example.invalid/runtime.zip").unwrap(),
                    size: 10,
                    sha256: [3_u8; 32],
                },
                verified_at: "2026-08-22T00:00:00Z".to_owned(),
                compatibility_summary: "verified".to_owned(),
            },
            manifest_digest: "a".repeat(64),
            desktop_version_supported,
        }
    }

    #[test]
    fn decision_matrix_classifies_all_success_states() {
        let current = Version::parse("0.1.1-rc.1").unwrap();
        let official = Version::parse("0.1.1-rc.2").unwrap();
        assert!(matches!(
            decide_notice(Some(&current), &official, None),
            UpdateNotice::OfficialAvailable { .. }
        ));

        let verified = candidate(
            "0.1.1-rc.2",
            "0.1.0",
            CoreCompatibility::Compatible,
            SkinCompatibility::Verified,
            true,
        );
        assert!(matches!(
            decide_notice(Some(&current), &official, Some(&verified)),
            UpdateNotice::RuntimeAvailable { ref compatible, .. }
                if compatible == "0.1.1-rc.2"
        ));

        let skin_unverified = candidate(
            "0.1.1-rc.2",
            "0.1.0",
            CoreCompatibility::Compatible,
            SkinCompatibility::Unverified,
            true,
        );
        assert!(matches!(
            decide_notice(Some(&current), &official, Some(&skin_unverified)),
            UpdateNotice::SkinUnverified { ref compatible, .. }
                if compatible == "0.1.1-rc.2"
        ));

        assert!(matches!(
            decide_notice(Some(&official), &official, None),
            UpdateNotice::UpToDate { .. }
        ));
    }

    #[test]
    fn core_marker_or_minimum_desktop_blocks_runtime_installation() {
        let official = Version::parse("0.1.1-rc.2").unwrap();
        for blocked in [
            candidate(
                "0.1.1-rc.2",
                "0.1.0",
                CoreCompatibility::DesktopRequired,
                SkinCompatibility::Verified,
                true,
            ),
            candidate(
                "0.1.1-rc.2",
                "0.2.0",
                CoreCompatibility::Compatible,
                SkinCompatibility::Verified,
                false,
            ),
        ] {
            assert!(matches!(
                decide_notice(None, &official, Some(&blocked)),
                UpdateNotice::DesktopRequired {
                    ref minimum_desktop,
                    ..
                } if minimum_desktop == &blocked.manifest.minimum_desktop_version.to_string()
            ));
        }
    }

    #[test]
    fn network_failure_with_installed_runtime_is_offline_without_losing_current() {
        let current = Version::parse("0.1.1-rc.1").unwrap();
        let notice = failed_notice(Some(&current), None, &SourceError::Network);
        assert!(matches!(
            notice,
            UpdateNotice::Offline {
                current: Some(ref installed),
                ref error_kind,
                ..
            } if installed == "0.1.1-rc.1" && error_kind == "network"
        ));
    }

    #[test]
    fn security_and_configuration_failures_are_not_reported_as_offline() {
        let current = Version::parse("0.1.1-rc.1").unwrap();
        for (error, expected_kind) in [
            (
                SourceError::CompatibilityVerification,
                "compatibility_verification",
            ),
            (
                SourceError::CompatibilityUnavailable,
                "compatibility_unavailable",
            ),
            (SourceError::InvalidConfiguration, "configuration"),
            (SourceError::ResponseTooLarge, "response_too_large"),
            (SourceError::HttpStatus { status: 403 }, "invalid_response"),
            (SourceError::InvalidResponse, "invalid_response"),
            (SourceError::InvalidVersion, "invalid_response"),
            (SourceError::InvalidIntegrity, "invalid_response"),
        ] {
            assert!(matches!(
                failed_notice(Some(&current), None, &error),
                UpdateNotice::CheckFailed {
                    current: Some(ref installed),
                    ref error_kind,
                    ..
                } if installed == "0.1.1-rc.1" && error_kind == expected_kind
            ));
        }
        for (error, expected_kind) in [
            (SourceError::Network, "network"),
            (SourceError::Timeout, "timeout"),
            (SourceError::RateLimited, "rate_limited"),
            (SourceError::ServerUnavailable, "server_unavailable"),
        ] {
            assert!(matches!(
                failed_notice(Some(&current), None, &error),
                UpdateNotice::Offline { ref error_kind, .. }
                    if error_kind == expected_kind
            ));
        }
    }

    #[tokio::test]
    async fn same_version_unverified_skin_is_visible_without_runtime_install_authorization() {
        let current = Version::parse("0.1.1-rc.2").unwrap();
        let unverified = candidate(
            "0.1.1-rc.2",
            "0.1.0",
            CoreCompatibility::Compatible,
            SkinCompatibility::Unverified,
            true,
        );
        assert!(matches!(
            decide_notice(Some(&current), &current, Some(&unverified)),
            UpdateNotice::SkinUnverified { .. }
        ));
        let coordinator = UpdateCoordinator::new(
            official("0.1.1-rc.2"),
            Arc::new(FixedCompatibility(Ok(Some(unverified.clone())))),
            UpdateStateStore::new(&unique_settings("same-version-skin")),
            Arc::new(FixedTime(100)),
            UpdateSchedule::default(),
            "stable".to_owned(),
        )
        .unwrap();
        let result = coordinator.check(Some(&current)).await.unwrap();
        assert!(matches!(result.notice, UpdateNotice::SkinUnverified { .. }));
        assert!(result.compatible_manifest.is_none());

        let verified = candidate(
            "0.1.1-rc.2",
            "0.1.0",
            CoreCompatibility::Compatible,
            SkinCompatibility::Verified,
            true,
        );
        assert!(matches!(
            decide_notice(Some(&current), &current, Some(&verified)),
            UpdateNotice::UpToDate { .. }
        ));
        let newer = Version::parse("0.1.1-rc.3").unwrap();
        assert!(matches!(
            decide_notice(Some(&newer), &current, Some(&unverified)),
            UpdateNotice::UpToDate { .. }
        ));
    }

    #[test]
    fn notification_keys_include_offline_reason_and_desktop_minimum() {
        let offline_network = UpdateNotice::Offline {
            current: Some("0.1.1-rc.1".to_owned()),
            version: None,
            error_kind: "network".to_owned(),
        };
        let offline_timeout = UpdateNotice::Offline {
            current: Some("0.1.1-rc.1".to_owned()),
            version: None,
            error_kind: "timeout".to_owned(),
        };
        assert_ne!(
            notification_key("stable", &offline_network),
            notification_key("stable", &offline_timeout)
        );

        let desktop_020 = UpdateNotice::DesktopRequired {
            current: None,
            official: "0.1.1-rc.2".to_owned(),
            compatible: "0.1.1-rc.2".to_owned(),
            minimum_desktop: "0.2.0".to_owned(),
        };
        let desktop_030 = UpdateNotice::DesktopRequired {
            current: None,
            official: "0.1.1-rc.2".to_owned(),
            compatible: "0.1.1-rc.2".to_owned(),
            minimum_desktop: "0.3.0".to_owned(),
        };
        assert_ne!(
            notification_key("stable", &desktop_020),
            notification_key("stable", &desktop_030)
        );
    }

    #[test]
    fn separates_official_available_from_runtime_available() {
        let official = Version::parse("0.2.0").unwrap();
        let current = Version::parse("0.1.1-rc.1").unwrap();
        assert!(matches!(
            decide_notice(Some(&current), &official, None),
            UpdateNotice::OfficialAvailable { .. }
        ));
        let compatible = candidate(
            "0.1.2",
            "0.1.0",
            CoreCompatibility::Compatible,
            SkinCompatibility::Verified,
            true,
        );
        assert!(matches!(
            decide_notice(Some(&current), &official, Some(&compatible)),
            UpdateNotice::RuntimeAvailable { .. }
        ));
    }

    #[test]
    fn fresh_install_and_up_to_date_boundaries_are_explicit() {
        let official = Version::parse("0.2.0").unwrap();
        assert!(matches!(
            decide_notice(None, &official, None),
            UpdateNotice::OfficialAvailable { current: None, .. }
        ));
        assert!(matches!(
            decide_notice(Some(&official), &official, None),
            UpdateNotice::UpToDate { .. }
        ));
    }

    #[test]
    fn rejects_compatible_version_newer_than_official_discovery() {
        let official = Version::parse("0.2.0").unwrap();
        let impossible = candidate(
            "0.3.0",
            "0.1.0",
            CoreCompatibility::Compatible,
            SkinCompatibility::Verified,
            true,
        );
        assert!(matches!(
            decide_notice(None, &official, Some(&impossible)),
            UpdateNotice::CheckFailed { .. }
        ));
    }

    #[test]
    fn persisted_keys_deduplicate_across_reload() {
        let notice = UpdateNotice::OfficialAvailable {
            current: Some("0.1.0".into()),
            official: "0.2.0".into(),
        };
        let key = notification_key("stable", &notice);
        let state = PersistedUpdateState {
            schema: 1,
            next_check_epoch_secs: 42,
            notification_keys: BTreeSet::from([key.clone()]),
        };
        let bytes = state.to_json().expect("状态可序列化");
        let loaded = PersistedUpdateState::from_json(&bytes).expect("状态可恢复");
        assert!(loaded.notification_keys.contains(&key));
    }

    #[test]
    fn notification_keys_remain_bounded_while_new_state_can_notify() {
        let mut keys = (0..128)
            .map(|index| format!("stable:1.0.{index}:runtime_available"))
            .collect::<BTreeSet<_>>();
        assert!(super::insert_bounded_notification_key(
            &mut keys,
            "stable:2.0.0:runtime_available".to_owned(),
        ));
        assert_eq!(keys.len(), 128);
        assert!(!super::insert_bounded_notification_key(
            &mut keys,
            "stable:2.0.0:runtime_available".to_owned(),
        ));
    }

    #[test]
    fn rejects_truncated_and_unknown_state_schema() {
        assert!(matches!(
            PersistedUpdateState::from_json(br#"{"schema":1"#),
            Err(UpdateStateError::InvalidState)
        ));
        assert!(matches!(
            PersistedUpdateState::from_json(
                br#"{"schema":2,"next_check_epoch_secs":0,"notification_keys":[]}"#
            ),
            Err(UpdateStateError::UnsupportedSchema { schema: 2 })
        ));
    }

    #[test]
    fn default_schedule_is_delayed_and_twelve_hourly() {
        let schedule = UpdateSchedule::default();
        assert!(schedule.startup_delay > Duration::ZERO);
        assert_eq!(schedule.interval, Duration::from_secs(12 * 60 * 60));
    }

    #[test]
    fn schedule_rejects_subsecond_interval_and_extreme_startup_delay() {
        for schedule in [
            UpdateSchedule {
                startup_delay: Duration::ZERO,
                interval: Duration::from_nanos(1),
            },
            UpdateSchedule {
                startup_delay: Duration::ZERO,
                interval: Duration::from_millis(1_500),
            },
            UpdateSchedule {
                startup_delay: Duration::from_secs(60 * 60 + 1),
                interval: Duration::from_secs(60),
            },
        ] {
            assert!(matches!(
                UpdateCoordinator::new(
                    official("0.2.0"),
                    no_compatibility(),
                    UpdateStateStore::new(&unique_settings("invalid-schedule")),
                    Arc::new(FixedTime(0)),
                    schedule,
                    "stable".to_owned(),
                ),
                Err(UpdateStateError::InvalidSchedule)
            ));
        }
    }

    #[tokio::test]
    async fn check_failure_is_not_reported_as_up_to_date() {
        let coordinator = UpdateCoordinator::new(
            Arc::new(FixedOfficial(Err(SourceError::Network))),
            no_compatibility(),
            UpdateStateStore::new(&unique_settings("failure")),
            Arc::new(FixedTime(100)),
            UpdateSchedule::default(),
            "stable".to_owned(),
        )
        .unwrap();
        let result = coordinator.check(None).await.unwrap();
        assert!(matches!(
            result.notice,
            UpdateNotice::Offline { version: None, ref error_kind, .. }
                if error_kind == "network"
        ));
        assert_eq!(result.next_check_epoch_secs, 43_300);
    }

    #[tokio::test]
    async fn notification_dedup_and_next_check_survive_restart() {
        let settings = unique_settings("restart");
        let first = UpdateCoordinator::new(
            official("0.2.0"),
            no_compatibility(),
            UpdateStateStore::new(&settings),
            Arc::new(FixedTime(1_000)),
            UpdateSchedule::default(),
            "stable".to_owned(),
        )
        .unwrap();
        let first_result = first.check(None).await.unwrap();
        assert!(first_result.should_notify);
        assert_eq!(first_result.next_check_epoch_secs, 44_200);

        let restarted = UpdateCoordinator::new(
            official("0.2.0"),
            no_compatibility(),
            UpdateStateStore::new(&settings),
            Arc::new(FixedTime(2_000)),
            UpdateSchedule::default(),
            "stable".to_owned(),
        )
        .unwrap();
        let second_result = restarted.check(None).await.unwrap();
        assert!(!second_result.should_notify);
        assert_eq!(second_result.next_check_epoch_secs, 45_200);
        assert!(!restarted.is_due().await.unwrap());
    }

    #[tokio::test]
    async fn failed_notification_delivery_releases_dedup_key_for_retry() {
        let settings = unique_settings("notification-retry");
        let coordinator = UpdateCoordinator::new(
            official("0.2.0"),
            no_compatibility(),
            UpdateStateStore::new(&settings),
            Arc::new(FixedTime(1_000)),
            UpdateSchedule::default(),
            "stable".to_owned(),
        )
        .unwrap();
        let first = coordinator.check(None).await.unwrap();
        assert!(first.should_notify);
        coordinator
            .release_notification(&first.notice)
            .await
            .unwrap();
        let retry = coordinator.check(None).await.unwrap();
        assert!(retry.should_notify);
    }

    #[tokio::test]
    async fn residual_temporary_state_does_not_block_future_saves() {
        let settings = unique_settings("temporary");
        tokio::fs::create_dir_all(&settings).await.unwrap();
        tokio::fs::write(settings.join("update-state.json.tmp"), b"diagnostic")
            .await
            .unwrap();
        let store = UpdateStateStore::new(&settings);
        store
            .save(&PersistedUpdateState::default())
            .await
            .expect("残留诊断文件不能永久阻塞更新状态");
    }

    #[tokio::test]
    async fn non_available_notice_never_exposes_installable_manifest() {
        let coordinator = UpdateCoordinator::new(
            official("0.2.0"),
            compatible("0.3.0"),
            UpdateStateStore::new(&unique_settings("unrelated-manifest")),
            Arc::new(FixedTime(100)),
            UpdateSchedule::default(),
            "stable".to_owned(),
        )
        .unwrap();
        let result = coordinator.check(None).await.unwrap();
        assert!(matches!(result.notice, UpdateNotice::CheckFailed { .. }));
        assert!(result.compatible_manifest.is_none());
    }

    #[tokio::test]
    async fn runtime_available_exposes_only_the_verified_manifest() {
        let coordinator = UpdateCoordinator::new(
            official("0.2.0"),
            compatible("0.1.2"),
            UpdateStateStore::new(&unique_settings("available-manifest")),
            Arc::new(FixedTime(100)),
            UpdateSchedule::default(),
            "stable".to_owned(),
        )
        .unwrap();
        let current = Version::parse("0.1.1-rc.1").unwrap();
        let result = coordinator.check(Some(&current)).await.unwrap();
        assert!(matches!(
            result.notice,
            UpdateNotice::RuntimeAvailable { .. }
        ));
        assert_eq!(
            result
                .compatible_manifest
                .expect("可用状态应保留已验证清单")
                .manifest
                .dsh_version
                .to_string(),
            "0.1.2"
        );
    }

    #[tokio::test]
    async fn concurrent_coordinators_share_the_process_lock_and_notify_once() {
        let settings = unique_settings("concurrent");
        let first = Arc::new(
            UpdateCoordinator::new(
                official("0.2.0"),
                no_compatibility(),
                UpdateStateStore::new(&settings),
                Arc::new(FixedTime(1_000)),
                UpdateSchedule::default(),
                "stable".to_owned(),
            )
            .unwrap(),
        );
        let second = Arc::new(
            UpdateCoordinator::new(
                official("0.2.0"),
                no_compatibility(),
                UpdateStateStore::new(&settings),
                Arc::new(FixedTime(1_000)),
                UpdateSchedule::default(),
                "stable".to_owned(),
            )
            .unwrap(),
        );
        let (left, right) = tokio::join!(first.check(None), second.check(None));
        let left = left.expect("并发检查一应完成");
        let right = right.expect("并发检查二应完成");
        assert_ne!(left.should_notify, right.should_notify);
        let state = UpdateStateStore::new(&settings).load().await.unwrap();
        assert_eq!(state.notification_keys.len(), 1);
    }

    #[tokio::test]
    async fn startup_check_skips_network_work_before_persisted_deadline() {
        let settings = unique_settings("startup-deadline");
        let store = UpdateStateStore::new(&settings);
        store
            .save(&PersistedUpdateState {
                schema: 1,
                next_check_epoch_secs: 500,
                notification_keys: BTreeSet::new(),
            })
            .await
            .unwrap();
        let coordinator = Arc::new(
            UpdateCoordinator::new(
                official("0.2.0"),
                no_compatibility(),
                store.clone(),
                Arc::new(FixedTime(100)),
                UpdateSchedule {
                    startup_delay: Duration::ZERO,
                    interval: Duration::from_secs(12 * 60 * 60),
                },
                "stable".to_owned(),
            )
            .unwrap(),
        );
        let sink = Arc::new(CountingDiagnostics::default());
        let diagnostics = DiagnosticContext::begin(TraceKind::Update, sink.clone());
        let result = UpdateCoordinator::spawn_startup_check(coordinator, None, diagnostics)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            ScheduledCheckResult::Skipped {
                next_check_epoch_secs: 500
            }
        ));
        assert_eq!(store.load().await.unwrap().next_check_epoch_secs, 500);
        assert_eq!(sink.0.load(Ordering::Relaxed), 2);
    }
}
