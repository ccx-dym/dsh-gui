use std::{
    fs::OpenOptions,
    io::Write,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tokio::sync::{Mutex, watch};
use url::Url;

#[cfg(any(not(debug_assertions), test))]
use crate::runtime::install_state::{ActiveDeployment, DataGeneration};
#[cfg(any(not(debug_assertions), test))]
use crate::update::activation::ActivationError;
#[cfg(not(debug_assertions))]
use crate::update::activation::{ActivationCheckpoint, PrecommitStage};
use crate::{
    diagnostics::{DiagnosticContext, FileDiagnosticSink, TraceKind},
    paths::{AppPaths, RuntimeLayout},
    runtime::install_state::{InstallStateError, InstallStateStore, InstalledRuntime},
    update::{
        archive::{
            ArchiveInstallPolicy, ArchiveInstallRequest, RuntimeArchiveInstaller,
            verify_installed_runtime_inventory,
        },
        coordinator::{
            ScheduledCheckResult, SystemUpdateTimeSource, UpdateCoordinator, UpdateSchedule,
            UpdateStateStore,
        },
        download::{
            ArtifactDownloader, DownloadCancellation, DownloadPolicy, DownloadProgress,
            DownloadProgressSink, DownloadRequest, HttpsDownloader,
        },
        manifest::{ManifestVerifier, VerifiedManifest},
        version_source::{
            NpmOfficialVersionSource, ReqwestSourceTransport, SignedCompatibilitySource,
            SourcePolicy,
        },
    },
};

#[cfg(any(not(debug_assertions), test))]
use crate::update::activation::{RuntimeActivator, SnapshotPolicy};
#[cfg(not(debug_assertions))]
use crate::update::{
    activation::{
        ActivationCheckpointSink, ActivationOutcome, ActivationRequest, RuntimeProbeAdapter,
    },
    probe::{ProbePolicy, RuntimeProbe},
};

const UPDATE_EVENT: &str = "update-state";
const DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct ProgressPublishThrottle {
    last_published: Option<Instant>,
}

impl ProgressPublishThrottle {
    fn delay_at(&self, now: Instant) -> Duration {
        self.last_published
            .map(|last| {
                DOWNLOAD_PROGRESS_INTERVAL.saturating_sub(now.saturating_duration_since(last))
            })
            .unwrap_or_default()
    }

    fn mark_published(&mut self, now: Instant) {
        self.last_published = Some(now);
    }
}

struct WatchDownloadProgressSink {
    sender: watch::Sender<DownloadProgress>,
}

impl DownloadProgressSink for WatchDownloadProgressSink {
    fn report(&self, downloaded_bytes: u64, total_bytes: Option<u64>) {
        // watch 只保留最新值，在 WebView 被挂起时也不会无界积压进度事件。
        self.sender.send_replace(DownloadProgress {
            downloaded_bytes,
            total_bytes,
        });
    }
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColdRecoveryPlan {
    RestartPrior,
    RetryFresh,
    RecoveryRequired,
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColdRecoveryResult {
    Retryable,
    RecoveryRequired,
}

#[cfg(any(not(debug_assertions), test))]
fn plan_cold_recovery(
    error: &ActivationError,
    prior_exists: bool,
    pointer_is_exact_prior: bool,
) -> ColdRecoveryPlan {
    let pointer_unchanged_failure = matches!(error, ActivationError::Precommit { .. });
    if !pointer_unchanged_failure || !pointer_is_exact_prior {
        ColdRecoveryPlan::RecoveryRequired
    } else if prior_exists {
        ColdRecoveryPlan::RestartPrior
    } else {
        ColdRecoveryPlan::RetryFresh
    }
}

#[cfg(any(not(debug_assertions), test))]
fn finish_cold_recovery(
    plan: ColdRecoveryPlan,
    prior_restart_succeeded: bool,
) -> ColdRecoveryResult {
    match plan {
        ColdRecoveryPlan::RestartPrior if prior_restart_succeeded => ColdRecoveryResult::Retryable,
        ColdRecoveryPlan::RetryFresh => ColdRecoveryResult::Retryable,
        ColdRecoveryPlan::RestartPrior | ColdRecoveryPlan::RecoveryRequired => {
            ColdRecoveryResult::RecoveryRequired
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateUiPhase {
    #[default]
    Unavailable,
    Uninstalled,
    Checking,
    UpToDate,
    OfficialAvailable,
    RuntimeAvailable,
    DesktopRequired,
    SkinUnverified,
    Offline,
    Downloading,
    Verifying,
    Probing,
    RestartPending,
    RollingBack,
    RecoveryRequired,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateUiState {
    pub revision: u64,
    pub phase: UpdateUiPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatible_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skin_compatible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_desktop_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub notifications_enabled: bool,
    pub should_notify: bool,
}

impl Default for UpdateUiState {
    fn default() -> Self {
        Self {
            revision: 0,
            phase: UpdateUiPhase::Unavailable,
            current_version: None,
            official_version: None,
            compatible_version: None,
            artifact_size: None,
            downloaded_bytes: None,
            download_percent: None,
            skin_compatible: None,
            compatibility_summary: None,
            minimum_desktop_version: None,
            error_code: None,
            notifications_enabled: true,
            should_notify: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStateEnvelope {
    pub revision: u64,
    pub state: UpdateUiState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingActivation {
    schema: u32,
    status: String,
    version: String,
    node_version: String,
    manifest_digest: String,
    generation: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActivationConfirmation {
    schema: u32,
    status: String,
    version: String,
    manifest_digest: String,
    generation: String,
    prior_pointer: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActivationReceipt {
    schema: u32,
    version: String,
    manifest_digest: String,
    generation: String,
    outcome: UpdateUiPhase,
}

pub struct UpdateUiController {
    paths: AppPaths,
    state: Mutex<UpdateUiState>,
    manifest: Mutex<Option<VerifiedManifest>>,
    pending: Mutex<Option<PendingActivation>>,
    operation: Mutex<()>,
}

impl UpdateUiController {
    /// 创建更新 UI 状态机；发布源缺失时明确保持 Unavailable。
    ///
    /// :param paths: 已解析并创建的应用私有目录。
    /// :return: 尚未访问网络的控制器。
    /// :raises: 此构造器不产生错误。
    pub fn new(paths: AppPaths) -> Self {
        let layout = RuntimeLayout::from_paths(&paths);
        let mut state = UpdateUiState::default();
        match InstallStateStore::new(layout).load() {
            Ok(active) => {
                state.phase = UpdateUiPhase::UpToDate;
                state.current_version = Some(active.runtime.version.to_string());
            }
            Err(InstallStateError::NotInstalled) => {
                state.phase = if release_config().is_some() {
                    UpdateUiPhase::Uninstalled
                } else {
                    UpdateUiPhase::Unavailable
                }
            }
            Err(_) => {
                state.phase = UpdateUiPhase::RecoveryRequired;
                state.error_code = Some("install_state_invalid".to_owned());
            }
        }
        Self {
            paths,
            state: Mutex::new(state),
            manifest: Mutex::new(None),
            pending: Mutex::new(None),
            operation: Mutex::new(()),
        }
    }

    async fn envelope(&self) -> UpdateStateEnvelope {
        let state = self.state.lock().await.clone();
        UpdateStateEnvelope {
            revision: state.revision,
            state,
        }
    }

    async fn publish(&self, app: &AppHandle, mut next: UpdateUiState) {
        let envelope = {
            let mut state = self.state.lock().await;
            next.revision = state.revision.saturating_add(1);
            *state = next.clone();
            UpdateStateEnvelope {
                revision: next.revision,
                state: next,
            }
        };
        let _ = app.emit(UPDATE_EVENT, envelope);
    }

    async fn require_revision(&self, expected: u64) -> Result<(), &'static str> {
        if self.state.lock().await.revision == expected {
            Ok(())
        } else {
            Err("update_state_stale")
        }
    }

    async fn manifest_for_install(&self) -> Result<VerifiedManifest, &'static str> {
        self.manifest
            .lock()
            .await
            .clone()
            .ok_or("compatible_update_missing")
    }

    async fn persist_confirmation_and_consume(
        &self,
        pending: &PendingActivation,
    ) -> Result<(), &'static str> {
        let manifest = self
            .manifest
            .lock()
            .await
            .clone()
            .ok_or("compatible_update_missing")?;
        persist_confirmation(&self.paths, &manifest, pending).map_err(|_| "update_failed")?;
        // 只有确认记录已经耐久落盘后才消费本次清单；失败时保留以允许安全重试。
        let mut stored = self.manifest.lock().await;
        if stored.as_ref() == Some(&manifest) {
            *stored = None;
        }
        Ok(())
    }

    #[cfg(not(debug_assertions))]
    async fn publish_cold_phase(
        &self,
        app: &AppHandle,
        phase: UpdateUiPhase,
        error_code: Option<&str>,
    ) {
        let mut next = self.state.lock().await.clone();
        next.phase = phase;
        next.error_code = error_code.map(str::to_owned);
        next.should_notify = false;
        self.publish(app, next).await;
    }
}

struct ReleaseConfig {
    registry: Url,
    manifest: Url,
    signature: Url,
    public_key: &'static str,
    channel: &'static str,
}

fn release_config() -> Option<ReleaseConfig> {
    let registry = Url::parse(option_env!("DSH_DESKTOP_NPM_REGISTRY_ROOT")?).ok()?;
    let manifest = Url::parse(option_env!("DSH_DESKTOP_COMPAT_MANIFEST_URL")?).ok()?;
    let signature = Url::parse(option_env!("DSH_DESKTOP_COMPAT_SIGNATURE_URL")?).ok()?;
    let public_key = option_env!("DSH_DESKTOP_COMPAT_PUBLIC_KEY")?;
    let channel = option_env!("DSH_DESKTOP_UPDATE_CHANNEL").unwrap_or("stable");
    Some(ReleaseConfig {
        registry,
        manifest,
        signature,
        public_key,
        channel,
    })
}

fn require_local(window: &WebviewWindow) -> Result<(), &'static str> {
    let url = window.url().map_err(|_| "update_origin_denied")?;
    if crate::update_command_allowed_for_url(url.as_str()) {
        Ok(())
    } else {
        Err("update_origin_denied")
    }
}

fn clear_notice_fields(state: &mut UpdateUiState) {
    // 每次检查都从空的结果槽开始，避免上一版本的安装授权、摘要或错误混入新结论。
    state.official_version = None;
    state.compatible_version = None;
    state.artifact_size = None;
    state.downloaded_bytes = None;
    state.download_percent = None;
    state.skin_compatible = None;
    state.compatibility_summary = None;
    state.minimum_desktop_version = None;
    state.error_code = None;
}

fn bounded_download_percent(downloaded_bytes: u64, total_bytes: Option<u64>) -> Option<u8> {
    let total_bytes = total_bytes.filter(|total| *total > 0)?;
    let bounded = downloaded_bytes.min(total_bytes) as u128;
    Some(((bounded * 100) / total_bytes as u128) as u8)
}

async fn publish_download_progress(
    controller: &UpdateUiController,
    app: &AppHandle,
    mut receiver: watch::Receiver<DownloadProgress>,
) {
    let mut throttle = ProgressPublishThrottle::default();
    while receiver.changed().await.is_ok() {
        let delay = throttle.delay_at(Instant::now());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let progress = *receiver.borrow_and_update();
        let mut state = controller.state.lock().await.clone();
        if !matches!(state.phase, UpdateUiPhase::Downloading) {
            return;
        }
        state.downloaded_bytes = Some(progress.downloaded_bytes);
        state.download_percent =
            bounded_download_percent(progress.downloaded_bytes, progress.total_bytes);
        controller.publish(app, state).await;
        throttle.mark_published(Instant::now());
    }
}

fn apply_notice(
    state: &mut UpdateUiState,
    notice: &crate::domain::UpdateNotice,
    manifest: Option<&VerifiedManifest>,
) {
    use crate::domain::UpdateNotice;

    clear_notice_fields(state);
    match notice {
        UpdateNotice::UpToDate { official, .. } => {
            state.phase = UpdateUiPhase::UpToDate;
            state.official_version = Some(official.clone());
        }
        UpdateNotice::OfficialAvailable { official, .. } => {
            state.phase = UpdateUiPhase::OfficialAvailable;
            state.official_version = Some(official.clone());
        }
        UpdateNotice::RuntimeAvailable {
            official,
            compatible,
            ..
        } => {
            state.phase = UpdateUiPhase::RuntimeAvailable;
            state.official_version = Some(official.clone());
            state.compatible_version = Some(compatible.clone());
            state.skin_compatible = Some(true);
        }
        UpdateNotice::DesktopRequired {
            official,
            compatible,
            minimum_desktop,
            ..
        } => {
            state.phase = UpdateUiPhase::DesktopRequired;
            state.official_version = Some(official.clone());
            state.compatible_version = Some(compatible.clone());
            state.minimum_desktop_version = Some(minimum_desktop.clone());
        }
        UpdateNotice::SkinUnverified {
            official,
            compatible,
            ..
        } => {
            state.phase = UpdateUiPhase::SkinUnverified;
            state.official_version = Some(official.clone());
            state.compatible_version = Some(compatible.clone());
            state.skin_compatible = Some(false);
        }
        UpdateNotice::Offline { error_kind, .. } => {
            state.phase = UpdateUiPhase::Offline;
            state.error_code = Some(error_kind.clone());
        }
        UpdateNotice::CheckFailed { error_kind, .. } => {
            state.phase = UpdateUiPhase::Failed;
            state.error_code = Some(error_kind.clone());
        }
    }
    if matches!(
        state.phase,
        UpdateUiPhase::RuntimeAvailable | UpdateUiPhase::SkinUnverified
    ) && let Some(manifest) = manifest
    {
        state.artifact_size = Some(manifest.manifest.artifact.size);
        state.compatibility_summary = Some(manifest.manifest.compatibility_summary.clone());
    }
}

#[tauri::command]
/// 获取更新状态的 revision 快照。
///
/// :param window: 发起调用的 WebView，用于纵深来源校验。
/// :param controller: 单一更新状态机。
/// :return: 当前 revision 与完整状态。
/// :raises: 非本地启动页调用返回 `update_origin_denied`。
pub async fn get_update_state(
    window: WebviewWindow,
    controller: tauri::State<'_, UpdateUiController>,
) -> Result<UpdateStateEnvelope, &'static str> {
    require_local(&window)?;
    Ok(controller.envelope().await)
}

#[tauri::command]
/// 手动检查官方版本与签名兼容清单。
///
/// :param window: 发起调用的本地启动页。
/// :param app: 状态事件与系统通知出口。
/// :param controller: 更新状态机及 manifest 暂存。
/// :param sink: 类型化本地诊断出口。
/// :param expected_revision: 前端最后观察到的 revision。
/// :return: 检查后的完整 revision 快照。
/// :raises: 来源、并发、旧 revision、配置或网络失败时返回固定错误码。
pub async fn check_updates(
    window: WebviewWindow,
    app: AppHandle,
    controller: tauri::State<'_, UpdateUiController>,
    sink: tauri::State<'_, FileDiagnosticSink>,
    expected_revision: u64,
) -> Result<UpdateStateEnvelope, &'static str> {
    require_local(&window)?;
    let _operation = controller.operation.try_lock().map_err(|_| "update_busy")?;
    controller.require_revision(expected_revision).await?;
    if !matches!(
        controller.state.lock().await.phase,
        UpdateUiPhase::Unavailable
            | UpdateUiPhase::Uninstalled
            | UpdateUiPhase::UpToDate
            | UpdateUiPhase::OfficialAvailable
            | UpdateUiPhase::RuntimeAvailable
            | UpdateUiPhase::DesktopRequired
            | UpdateUiPhase::SkinUnverified
            | UpdateUiPhase::Offline
            | UpdateUiPhase::Failed
    ) {
        return Err("update_transition_denied");
    }
    run_update_check(&app, &controller, &sink, false).await
}

async fn run_update_check(
    app: &AppHandle,
    controller: &UpdateUiController,
    sink: &FileDiagnosticSink,
    scheduled: bool,
) -> Result<UpdateStateEnvelope, &'static str> {
    if !scheduled {
        let mut next = controller.state.lock().await.clone();
        next.phase = UpdateUiPhase::Checking;
        next.should_notify = false;
        clear_notice_fields(&mut next);
        *controller.manifest.lock().await = None;
        controller.publish(app, next).await;
    }
    let Some(config) = release_config() else {
        if scheduled {
            return Ok(controller.envelope().await);
        }
        let mut unavailable = controller.state.lock().await.clone();
        clear_notice_fields(&mut unavailable);
        unavailable.phase = UpdateUiPhase::Unavailable;
        unavailable.error_code = Some("release_configuration_unavailable".to_owned());
        controller.publish(app, unavailable).await;
        return Err("update_unavailable");
    };
    let diagnostics = DiagnosticContext::begin(TraceKind::Update, Arc::new(sink.clone()));
    let attempt = async {
        let transport = Arc::new(
            ReqwestSourceTransport::new(Duration::from_secs(10))
                .map_err(|_| "update_unavailable")?,
        );
        let verifier = ManifestVerifier::new(
            config.public_key,
            Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| "update_unavailable")?,
        )
        .map_err(|_| "update_unavailable")?;
        let official = Arc::new(
            NpmOfficialVersionSource::new(
                config.registry,
                transport.clone(),
                SourcePolicy::default(),
            )
            .map_err(|_| "update_unavailable")?,
        );
        let compatibility = Arc::new(
            SignedCompatibilitySource::new(
                config.manifest,
                config.signature,
                transport,
                SourcePolicy::default(),
                verifier,
            )
            .map_err(|_| "update_unavailable")?,
        );
        let coordinator = UpdateCoordinator::new(
            official,
            compatibility,
            UpdateStateStore::new(&controller.paths.settings),
            Arc::new(SystemUpdateTimeSource),
            UpdateSchedule::default(),
            config.channel.to_owned(),
        )
        .map_err(|_| "update_unavailable")?;
        let current = InstallStateStore::new(RuntimeLayout::from_paths(&controller.paths))
            .load()
            .ok()
            .map(|active| active.runtime.version);
        let result = if scheduled {
            match coordinator
                .check_if_due(current.as_ref(), &diagnostics)
                .await
                .map_err(|_| "update_unavailable")?
            {
                ScheduledCheckResult::Skipped { .. } => {
                    return Ok((None, current, coordinator));
                }
                ScheduledCheckResult::Checked(result) => Some(*result),
            }
        } else {
            Some(
                coordinator
                    .check_with_context(current.as_ref(), &diagnostics)
                    .await
                    .map_err(|_| "update_unavailable")?,
            )
        };
        Ok::<_, &'static str>((result, current, coordinator))
    }
    .await;
    let (result, current, coordinator) = match attempt {
        Ok(value) => value,
        Err(code) => {
            *controller.manifest.lock().await = None;
            let mut failed = controller.state.lock().await.clone();
            clear_notice_fields(&mut failed);
            failed.phase = UpdateUiPhase::Failed;
            failed.error_code = Some(code.to_owned());
            controller.publish(app, failed).await;
            return Err(code);
        }
    };
    let Some(result) = result else {
        return Ok(controller.envelope().await);
    };
    let mut checked = controller.state.lock().await.clone();
    checked.current_version = current.map(|value| value.to_string());
    checked.should_notify = result.should_notify;
    let notification_notice = result.notice.clone();
    apply_notice(
        &mut checked,
        &result.notice,
        result.compatible_manifest.as_ref(),
    );
    *controller.manifest.lock().await = result.compatible_manifest;
    controller.publish(app, checked).await;
    let state = controller.envelope().await;
    if state.state.should_notify {
        let notification = match state.state.phase {
            UpdateUiPhase::OfficialAvailable => {
                Some(("DSH 新版本正在兼容验证", "验证完成后桌面端会开放安全安装。"))
            }
            UpdateUiPhase::RuntimeAvailable | UpdateUiPhase::SkinUnverified => Some((
                "DSH 兼容更新已就绪",
                "打开 DSH Desktop 查看版本并确认下载。",
            )),
            UpdateUiPhase::DesktopRequired => Some((
                "DSH Desktop 需要更新",
                "请先更新桌面客户端，再安装此 DSH 版本。",
            )),
            _ => None,
        };
        if let Some((title, body)) = notification
            && show_update_notification(app, title, body).await.is_err()
        {
            // 仅释放去重键，不改变已检查出的版本状态；下次周期可重试通知。
            let _ = coordinator.release_notification(&notification_notice).await;
        }
    }
    Ok(state)
}

async fn show_update_notification(
    app: &AppHandle,
    title: &'static str,
    body: &'static str,
) -> Result<(), ()> {
    let app_id = app.config().identifier.clone();
    let notification = tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new()
            .appname("DSH Desktop")
            .app_id(&app_id)
            .summary(title)
            .body(body)
            .action("open_updates", "打开更新中心")
            .show()
    })
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    let activation_app = app.clone();
    tokio::task::spawn_blocking(move || {
        let _ =
            notification.wait_for_response(move |response: &notify_rust::NotificationResponse| {
                if !matches!(
                    response,
                    notify_rust::NotificationResponse::Default
                        | notify_rust::NotificationResponse::Action(_)
                ) {
                    return;
                }
                // 任意 action 标识都只触发固定本地窗口；不解析、不导航 payload。
                if let Some(window) = activation_app.get_webview_window("updates") {
                    #[cfg(debug_assertions)]
                    let local_url = "http://127.0.0.1:1420/";
                    #[cfg(not(debug_assertions))]
                    let local_url = "tauri://localhost/index.html";
                    if let Ok(url) = tauri::Url::parse(local_url) {
                        let _ = window.navigate(url);
                    }
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            });
    });
    Ok(())
}

/// 启动由 Rust 持有的周期检查循环；持久化截止时间仍由 Task7 coordinator 决定。
///
/// :param app: 用于取得受管状态、发布 revision 事件和恢复固定本地更新窗口。
/// :return: 后台任务立即交还，不阻塞 Tauri setup。
/// :raises: 后台错误被收敛为安全状态或留待下一次持久化到期检查，不向 setup 抛出。
pub fn spawn_scheduled_update_checks(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            let controller = app.state::<UpdateUiController>();
            let sink = app.state::<FileDiagnosticSink>();
            if let Ok(_operation) = controller.operation.try_lock() {
                let phase = controller.state.lock().await.phase.clone();
                if matches!(
                    phase,
                    UpdateUiPhase::Unavailable
                        | UpdateUiPhase::Uninstalled
                        | UpdateUiPhase::UpToDate
                        | UpdateUiPhase::OfficialAvailable
                        | UpdateUiPhase::RuntimeAvailable
                        | UpdateUiPhase::DesktopRequired
                        | UpdateUiPhase::SkinUnverified
                        | UpdateUiPhase::Offline
                        | UpdateUiPhase::Failed
                ) {
                    // 15 分钟只读取一次本地 next_check；真正联网严格受 12 小时持久化
                    // deadline 控制，应用重启不会重置或提前该时间。
                    let _ = run_update_check(&app, &controller, &sink, true).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(15 * 60)).await;
        }
    });
}

#[tauri::command]
/// 下载、双重校验并封存已签名兼容 runtime。
///
/// :param window: 发起调用的本地启动页。
/// :param app: 只用于发布固定 update-state 事件。
/// :param controller: 持有检查阶段验证过的 manifest。
/// :param sink: 与下载、解压共享的类型化诊断出口。
/// :param expected_revision: 用户确认时看到的 revision。
/// :return: 已安全暂存、等待重启确认的状态快照。
/// :raises: 来源、并发、完整性或文件系统失败时返回固定错误码。
pub async fn install_compatible_update(
    window: WebviewWindow,
    app: AppHandle,
    controller: tauri::State<'_, UpdateUiController>,
    sink: tauri::State<'_, FileDiagnosticSink>,
    expected_revision: u64,
) -> Result<UpdateStateEnvelope, &'static str> {
    require_local(&window)?;
    let _operation = controller.operation.try_lock().map_err(|_| "update_busy")?;
    controller.require_revision(expected_revision).await?;
    if !matches!(
        controller.state.lock().await.phase,
        UpdateUiPhase::RuntimeAvailable | UpdateUiPhase::SkinUnverified
    ) {
        return Err("update_transition_denied");
    }
    let manifest = controller.manifest_for_install().await?;
    // phase、revision 与 operation gate 共同阻止重复下载；清单保留到确认记录落盘成功，
    // 从而让同一次受信安装事务能完成冷启动授权。
    let mut downloading = controller.state.lock().await.clone();
    downloading.phase = UpdateUiPhase::Downloading;
    downloading.downloaded_bytes = Some(0);
    downloading.download_percent = None;
    downloading.should_notify = false;
    controller.publish(&app, downloading).await;
    let diagnostics = DiagnosticContext::begin(TraceKind::Update, Arc::new(sink.inner().clone()));
    let pending_activation =
        match download_and_stage(&controller, &app, &manifest, &diagnostics).await {
            Ok(pending) => pending,
            Err(code) => {
                *controller.manifest.lock().await = None;
                let mut failed = controller.state.lock().await.clone();
                clear_notice_fields(&mut failed);
                failed.phase = UpdateUiPhase::Failed;
                failed.error_code = Some(code.to_owned());
                controller.publish(&app, failed).await;
                return Err(code);
            }
        };
    *controller.pending.lock().await = Some(pending_activation);
    let mut pending = controller.state.lock().await.clone();
    clear_notice_fields(&mut pending);
    pending.phase = UpdateUiPhase::RestartPending;
    controller.publish(&app, pending).await;
    Ok(controller.envelope().await)
}

async fn download_and_stage(
    controller: &UpdateUiController,
    app: &AppHandle,
    manifest: &VerifiedManifest,
    diagnostics: &DiagnosticContext,
) -> Result<PendingActivation, &'static str> {
    let runtime = InstalledRuntime::with_node_version(
        &manifest.manifest.dsh_version.to_string(),
        manifest.manifest_digest.clone(),
        &manifest.manifest.node_version.to_string(),
    )
    .map_err(|_| "update_failed")?;
    let layout = RuntimeLayout::from_paths(&controller.paths);
    let runtime_dir = layout.runtime_dir(&runtime);
    if runtime_dir.is_dir() {
        tokio::task::spawn_blocking(move || {
            verify_installed_runtime_inventory(
                &runtime_dir,
                ArchiveInstallPolicy::default(),
                || true,
            )
        })
        .await
        .map_err(|_| "update_failed")?
        .map_err(|_| "update_failed")?;
        return persist_pending(&controller.paths, manifest).map_err(|_| "update_failed");
    }
    let downloader =
        HttpsDownloader::new(DownloadPolicy::default()).map_err(|_| "update_failed")?;
    let (progress_sender, progress_receiver) = watch::channel(DownloadProgress::default());
    let download_future = async {
        let progress = WatchDownloadProgressSink {
            sender: progress_sender,
        };
        downloader
            .download(
                DownloadRequest {
                    artifact: &manifest.manifest.artifact,
                    updates_dir: &controller.paths.updates,
                    cancellation: DownloadCancellation::default(),
                    progress: &progress,
                },
                diagnostics,
            )
            .await
    };
    let progress_future = publish_download_progress(controller, app, progress_receiver);
    let (downloaded, ()) = tokio::join!(download_future, progress_future);
    let downloaded = downloaded.map_err(|_| "update_failed")?;
    let mut verifying = controller.state.lock().await.clone();
    verifying.phase = UpdateUiPhase::Verifying;
    verifying.downloaded_bytes = None;
    verifying.download_percent = None;
    controller.publish(app, verifying).await;
    let request = ArchiveInstallRequest::from_downloaded(
        downloaded,
        layout,
        runtime,
        manifest.manifest.node_version.clone(),
    );
    RuntimeArchiveInstaller::new(ArchiveInstallPolicy::default())
        .map_err(|_| "update_failed")?
        .install_with_context(request, diagnostics)
        .await
        .map_err(|_| "update_failed")?;
    persist_pending(&controller.paths, manifest).map_err(|_| "update_failed")
}

#[tauri::command]
/// 把已封存版本安排到下一次冷启动激活。
///
/// :param window: 发起调用的本地启动页。
/// :param app: 只用于发布确认后的状态事件。
/// :param controller: 更新状态机及受信 manifest。
/// :param expected_revision: 用户确认时看到的 revision。
/// :return: 不会热切换当前 runtime 的等待重启状态。
/// :raises: 来源、并发、旧 revision 或持久化失败时返回固定错误码。
pub async fn confirm_activation(
    window: WebviewWindow,
    app: AppHandle,
    controller: tauri::State<'_, UpdateUiController>,
    expected_revision: u64,
) -> Result<UpdateStateEnvelope, &'static str> {
    require_local(&window)?;
    let _operation = controller.operation.try_lock().map_err(|_| "update_busy")?;
    controller.require_revision(expected_revision).await?;
    let state = controller.state.lock().await.clone();
    if matches!(state.phase, UpdateUiPhase::RecoveryRequired)
        && state.error_code.as_deref() == Some("activation_retry_available")
    {
        let paths = controller.paths.clone();
        let retry = tokio::task::spawn_blocking(move || create_explicit_retry_attempt(&paths))
            .await
            .map_err(|_| "activation_recovery_required")?
            .map_err(|_| "activation_recovery_required")?;
        *controller.pending.lock().await = Some(retry);
        let mut confirmed = state;
        confirmed.phase = UpdateUiPhase::RestartPending;
        confirmed.error_code = Some("activation_confirmed".to_owned());
        controller.publish(&app, confirmed).await;
        return Ok(controller.envelope().await);
    }
    if !matches!(state.phase, UpdateUiPhase::RestartPending)
        || state.error_code.as_deref() == Some("activation_confirmed")
    {
        return Err("update_transition_denied");
    }
    let pending = controller
        .pending
        .lock()
        .await
        .clone()
        .ok_or("compatible_update_missing")?;
    controller
        .persist_confirmation_and_consume(&pending)
        .await?;
    let mut confirmed = controller.state.lock().await.clone();
    confirmed.phase = UpdateUiPhase::RestartPending;
    confirmed.error_code = Some("activation_confirmed".to_owned());
    controller.publish(&app, confirmed).await;
    Ok(controller.envelope().await)
}

fn persist_pending(
    paths: &AppPaths,
    manifest: &VerifiedManifest,
) -> std::io::Result<PendingActivation> {
    std::fs::create_dir_all(&paths.settings)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let pending = pending_document(manifest, nonce);
    write_new_or_matching(
        &pending_path(paths, &pending),
        &serde_json::to_vec(&pending).map_err(std::io::Error::other)?,
    )?;
    Ok(pending)
}

fn pending_document(manifest: &VerifiedManifest, attempt: u128) -> PendingActivation {
    PendingActivation {
        schema: 1,
        status: "downloaded".to_owned(),
        version: manifest.manifest.dsh_version.to_string(),
        node_version: manifest.manifest.node_version.to_string(),
        manifest_digest: manifest.manifest_digest.clone(),
        // generation 只使用签名 manifest digest，避免 semver build metadata 中的 `+`
        // 落入 Windows 目录标识并保持同一发布记录的幂等身份。
        generation: format!("generation-{}-{attempt}", &manifest.manifest_digest[..24]),
    }
}

fn pending_path(paths: &AppPaths, pending: &PendingActivation) -> std::path::PathBuf {
    paths.settings.join(format!(
        "pending-activation-{}-{}-{}.json",
        pending.version,
        &pending.manifest_digest[..12],
        pending.generation
    ))
}

#[cfg(not(debug_assertions))]
#[derive(Clone, Copy, Debug, Default)]
struct ProductionCheckpoints;

#[cfg(not(debug_assertions))]
impl ActivationCheckpointSink for ProductionCheckpoints {
    fn reached(&self, _checkpoint: ActivationCheckpoint) -> Result<(), ActivationError> {
        Ok(())
    }
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Debug, Eq, PartialEq)]
enum OrphanCandidateDisposition {
    RestartPrior(ActiveDeployment),
    RetryFresh,
}

/// 对 CandidatePrepared 与 JournalPrepared 之间的磁盘残留做只读、失败关闭分类。
///
/// validator 在生产环境同时复核签名 runtime inventory 与 candidate snapshot inventory；
/// 测试可注入确定性校验器以直接覆盖真实 pending/confirmation/pointer 文件矩阵。
#[cfg(any(not(debug_assertions), test))]
fn inspect_orphan_candidate_with(
    paths: &AppPaths,
    pending: &PendingActivation,
    validate_inventory: impl FnOnce(&InstalledRuntime, &DataGeneration) -> Result<(), ()>,
) -> Result<OrphanCandidateDisposition, &'static str> {
    let expected_pending =
        serde_json::to_vec(pending).map_err(|_| "activation_recovery_required")?;
    if std::fs::read(pending_path(paths, pending)).map_err(|_| "activation_recovery_required")?
        != expected_pending
        || activation_receipt(paths, pending).exists()
    {
        return Err("activation_recovery_required");
    }
    let confirmation: ActivationConfirmation = serde_json::from_slice(
        &std::fs::read(confirmation_path(paths, pending))
            .map_err(|_| "activation_recovery_required")?,
    )
    .map_err(|_| "activation_recovery_required")?;
    if confirmation.schema != 2
        || confirmation.status != "confirmed"
        || confirmation.version != pending.version
        || confirmation.manifest_digest != pending.manifest_digest
        || confirmation.generation != pending.generation
        || confirmation.prior_pointer != current_pointer_identity(paths)?
    {
        return Err("activation_recovery_required");
    }

    // 复用 Task9 唯一严格 parser：历史终态可共存，任何未决或损坏 journal 失败关闭。
    RuntimeActivator::new(RuntimeLayout::from_paths(paths), SnapshotPolicy::default())
        .map_err(|_| "activation_recovery_required")?
        .ensure_terminal_journal_history()
        .map_err(|_| "activation_recovery_required")?;

    let runtime = InstalledRuntime::with_node_version(
        &pending.version,
        pending.manifest_digest.clone(),
        &pending.node_version,
    )
    .map_err(|_| "activation_recovery_required")?;
    let candidate =
        DataGeneration::new(&pending.generation).map_err(|_| "activation_recovery_required")?;
    validate_inventory(&runtime, &candidate).map_err(|_| "activation_recovery_required")?;

    match InstallStateStore::new(RuntimeLayout::from_paths(paths)).load() {
        Ok(active) if active.runtime != runtime || active.data.id != pending.generation => {
            Ok(OrphanCandidateDisposition::RestartPrior(active))
        }
        Err(InstallStateError::NotInstalled) => Ok(OrphanCandidateDisposition::RetryFresh),
        Ok(_) | Err(_) => Err("activation_recovery_required"),
    }
}

#[cfg(any(not(debug_assertions), test))]
async fn recover_orphan_candidate_with<Validate, Restart, RestartFuture>(
    paths: &AppPaths,
    pending: &PendingActivation,
    validate_inventory: Validate,
    restart_prior: Restart,
) -> Result<OrphanCandidateDisposition, &'static str>
where
    Validate: FnOnce(&InstalledRuntime, &DataGeneration) -> Result<(), ()>,
    Restart: FnOnce(ActiveDeployment) -> RestartFuture,
    RestartFuture: std::future::Future<Output = Result<(), ()>>,
{
    let disposition = inspect_orphan_candidate_with(paths, pending, validate_inventory)?;
    if let OrphanCandidateDisposition::RestartPrior(prior) = &disposition
        && restart_prior(prior.clone()).await.is_err()
    {
        // 旧版无法可靠恢复时，该 attempt 必须先成为终态；即使 durable 写入本身
        // 失败也只返回同一个稳定恢复码，绝不把动态错误或“成功”暴露给上层。
        let _ = write_activation_receipt(paths, pending, &UpdateUiPhase::RecoveryRequired);
        return Err("activation_recovery_required");
    }
    write_activation_receipt(paths, pending, &UpdateUiPhase::Failed)
        .map_err(|_| "activation_recovery_required")?;
    Ok(disposition)
}

/// 在 supervisor 启动前恢复旧事务或激活已确认的暂存版本。
///
/// 在线命令只创建 append-only pending/confirmation；本函数是唯一切换入口。
///
/// :param app_controller: 尚未启动 supervisor 的生命周期控制器。
/// :param update_controller: 更新状态与私有目录。
/// :param diagnostics: 冷启动全链路共享的类型化诊断上下文。
/// :return: runtime 已按恢复、激活或普通 active pointer 启动，或明确保持未安装。
/// :raises: 任何不一致都收敛为 RecoveryRequired 并返回稳定错误。
#[cfg(not(debug_assertions))]
pub async fn cold_bootstrap(
    app: &AppHandle,
    app_controller: &crate::app_controller::AppController,
    update_controller: &UpdateUiController,
    diagnostics: &DiagnosticContext,
) -> Result<(), &'static str> {
    let _update_operation = update_controller.operation.lock().await;
    let result = cold_bootstrap_inner(app, app_controller, update_controller, diagnostics).await;
    if let Err(code) = result {
        update_controller
            .publish_cold_phase(app, UpdateUiPhase::RecoveryRequired, Some(code))
            .await;
    }
    result
}

#[cfg(not(debug_assertions))]
async fn cold_bootstrap_inner(
    app: &AppHandle,
    app_controller: &crate::app_controller::AppController,
    update_controller: &UpdateUiController,
    diagnostics: &DiagnosticContext,
) -> Result<(), &'static str> {
    let layout = RuntimeLayout::from_paths(&update_controller.paths);
    let activator = RuntimeActivator::new(layout.clone(), SnapshotPolicy::default())
        .map_err(|_| "activation_recovery_required")?;

    let recovery_session = app_controller
        .begin_activation()
        .map_err(|_| "activation_recovery_required")?;
    match activator
        .recover_with_context(recovery_session, diagnostics)
        .await
    {
        Ok(ActivationOutcome::Activated) => {
            if let Some(pending) = load_single_confirmed_pending(&update_controller.paths)? {
                write_activation_receipt(
                    &update_controller.paths,
                    &pending,
                    &UpdateUiPhase::UpToDate,
                )
                .map_err(|_| "activation_recovery_required")?;
            }
            update_controller
                .publish_cold_phase(app, UpdateUiPhase::UpToDate, None)
                .await;
            return Ok(());
        }
        Ok(ActivationOutcome::RolledBack { .. }) => {
            if let Some(pending) = load_single_confirmed_pending(&update_controller.paths)? {
                write_activation_receipt(
                    &update_controller.paths,
                    &pending,
                    &UpdateUiPhase::RollingBack,
                )
                .map_err(|_| "activation_recovery_required")?;
            }
            update_controller
                .publish_cold_phase(app, UpdateUiPhase::RollingBack, Some("rollback_completed"))
                .await;
            return Ok(());
        }
        Ok(ActivationOutcome::FreshInstallFailed { .. }) => {
            if let Some(pending) = load_single_confirmed_pending(&update_controller.paths)? {
                write_activation_receipt(
                    &update_controller.paths,
                    &pending,
                    &UpdateUiPhase::Failed,
                )
                .map_err(|_| "activation_recovery_required")?;
            }
            update_controller
                .publish_cold_phase(app, UpdateUiPhase::Failed, Some("fresh_install_failed"))
                .await;
            return Ok(());
        }
        Ok(ActivationOutcome::NothingToRecover) => {}
        Err(_) => {
            update_controller
                .publish_cold_phase(
                    app,
                    UpdateUiPhase::RecoveryRequired,
                    Some("activation_recovery_required"),
                )
                .await;
            return Err("activation_recovery_required");
        }
    }

    let Some(pending) = load_single_confirmed_pending(&update_controller.paths)? else {
        return match InstallStateStore::new(layout).load() {
            Ok(_) => app_controller
                .start_active_runtime()
                .map_err(|_| "runtime_start_failed"),
            Err(InstallStateError::NotInstalled) => Ok(()),
            Err(_) => {
                update_controller
                    .publish_cold_phase(
                        app,
                        UpdateUiPhase::RecoveryRequired,
                        Some("install_state_invalid"),
                    )
                    .await;
                Err("activation_recovery_required")
            }
        };
    };
    if let Ok(active) = InstallStateStore::new(layout.clone()).load()
        && active.runtime.version.to_string() == pending.version
        && active.runtime.manifest_digest == pending.manifest_digest
        && active.data.id == pending.generation
    {
        // pointer 已提交但进程可能在 receipt 前崩溃；承认权威精确配对，禁止重复建 candidate。
        write_activation_receipt(&update_controller.paths, &pending, &UpdateUiPhase::UpToDate)
            .map_err(|_| "activation_recovery_required")?;
        app_controller
            .start_active_runtime()
            .map_err(|_| "runtime_start_failed")?;
        update_controller
            .publish_cold_phase(app, UpdateUiPhase::UpToDate, None)
            .await;
        return Ok(());
    }
    if activation_receipt(&update_controller.paths, &pending).is_file() {
        return match InstallStateStore::new(layout).load() {
            // terminal receipt may represent Activated or RolledBack; pointer remains authoritative.
            Ok(_) => app_controller
                .start_active_runtime()
                .map_err(|_| "runtime_start_failed"),
            Err(InstallStateError::NotInstalled) => Ok(()),
            Err(_) => {
                update_controller
                    .publish_cold_phase(
                        app,
                        UpdateUiPhase::RecoveryRequired,
                        Some("activation_receipt_mismatch"),
                    )
                    .await;
                Err("activation_recovery_required")
            }
        };
    }

    let candidate = crate::runtime::install_state::DataGeneration::new(&pending.generation)
        .map_err(|_| "activation_recovery_required")?;
    if layout.generation_dir(&candidate).exists() {
        recover_orphan_candidate_with(
            &update_controller.paths,
            &pending,
            |runtime, candidate| {
                verify_installed_runtime_inventory(
                    &layout.runtime_dir(runtime),
                    ArchiveInstallPolicy::default(),
                    || true,
                )
                .map_err(|_| ())?;
                activator
                    .validate_prepared_candidate(candidate)
                    .map_err(|_| ())
            },
            |prior| async move {
                app_controller
                    .start_exact_active_runtime(&prior)
                    .await
                    .map_err(|_| ())
            },
        )
        .await?;
        // candidate 只作为崩溃证据保留；显式重试会生成全新的 generation id。
        update_controller
            .publish_cold_phase(
                app,
                UpdateUiPhase::Failed,
                Some("activation_retry_available"),
            )
            .await;
        return Ok(());
    }

    let workspace = update_controller.paths.dsh_home.join("projects/default");
    std::fs::create_dir_all(&workspace).map_err(|_| "activation_recovery_required")?;
    crate::update::probe::secure_private_windows_dacl(&workspace)
        .map_err(|_| "activation_recovery_required")?;
    activator
        .save_trusted_workspace(&workspace)
        .map_err(|_| "activation_recovery_required")?;
    let runtime = InstalledRuntime::with_node_version(
        &pending.version,
        pending.manifest_digest.clone(),
        &pending.node_version,
    )
    .map_err(|_| "activation_recovery_required")?;
    let probe = RuntimeProbeAdapter::new(
        RuntimeProbe::new(ProbePolicy::default()).map_err(|_| "activation_recovery_required")?,
    );
    let request = ActivationRequest {
        runtime,
        candidate,
        activated_at: format!(
            "epoch-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "activation_recovery_required")?
                .as_secs()
        ),
    };
    update_controller
        .publish_cold_phase(app, UpdateUiPhase::Probing, None)
        .await;
    let store = InstallStateStore::new(layout.clone());
    let prior = match store.load() {
        Ok(active) => Some(active),
        Err(InstallStateError::NotInstalled) => None,
        Err(_) => return Err("activation_recovery_required"),
    };
    let session = app_controller
        .begin_activation()
        .map_err(|_| "activation_recovery_required")?;
    let outcome = activator
        .activate_with_context(
            session,
            request,
            &probe,
            &ProductionCheckpoints,
            diagnostics,
        )
        .await;
    if let Err(error) = &outcome {
        let pointer_is_exact_prior = match (&prior, store.load()) {
            (Some(expected), Ok(actual)) => &actual == expected,
            (None, Err(InstallStateError::NotInstalled)) => true,
            _ => false,
        };
        let plan = plan_cold_recovery(error, prior.is_some(), pointer_is_exact_prior);
        let journal_prepared = matches!(
            error,
            ActivationError::Precommit {
                stage: PrecommitStage::JournalPrepared,
                ..
            }
        );
        let restarted = if journal_prepared && !matches!(plan, ColdRecoveryPlan::RecoveryRequired) {
            match app_controller.begin_activation() {
                Ok(recovery_session) => matches!(
                    activator
                        .recover_with_context(recovery_session, diagnostics)
                        .await,
                    Ok(ActivationOutcome::RolledBack { .. })
                        | Ok(ActivationOutcome::FreshInstallFailed { .. })
                ),
                Err(_) => false,
            }
        } else {
            match plan {
                ColdRecoveryPlan::RestartPrior => app_controller
                    .start_exact_active_runtime(prior.as_ref().expect("plan requires prior"))
                    .await
                    .is_ok(),
                ColdRecoveryPlan::RetryFresh => true,
                ColdRecoveryPlan::RecoveryRequired => false,
            }
        };
        if matches!(
            finish_cold_recovery(plan, restarted),
            ColdRecoveryResult::Retryable
        ) {
            write_activation_receipt(&update_controller.paths, &pending, &UpdateUiPhase::Failed)
                .map_err(|_| "activation_recovery_required")?;
            update_controller
                .publish_cold_phase(
                    app,
                    UpdateUiPhase::RecoveryRequired,
                    Some("activation_retry_available"),
                )
                .await;
            return Ok(());
        }
    }
    let phase = match outcome {
        Ok(ActivationOutcome::Activated) => UpdateUiPhase::UpToDate,
        Ok(ActivationOutcome::RolledBack { .. }) => UpdateUiPhase::RollingBack,
        Ok(ActivationOutcome::FreshInstallFailed { .. }) => UpdateUiPhase::Failed,
        Ok(ActivationOutcome::NothingToRecover) => UpdateUiPhase::Failed,
        Err(_) => UpdateUiPhase::RecoveryRequired,
    };
    write_activation_receipt(&update_controller.paths, &pending, &phase)
        .map_err(|_| "activation_recovery_required")?;
    update_controller
        .publish_cold_phase(app, phase.clone(), None)
        .await;
    if matches!(phase, UpdateUiPhase::RecoveryRequired) {
        Err("activation_recovery_required")
    } else {
        Ok(())
    }
}

#[cfg(any(not(debug_assertions), test))]
fn load_single_confirmed_pending(
    paths: &AppPaths,
) -> Result<Option<PendingActivation>, &'static str> {
    let entries = std::fs::read_dir(&paths.settings).map_err(|_| "activation_recovery_required")?;
    let mut confirmed = Vec::new();
    let mut retryable_failure = false;
    let mut unresolved_recovery = false;
    for entry in entries {
        let entry = entry.map_err(|_| "activation_recovery_required")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("pending-activation-") || !name.ends_with(".json") {
            continue;
        }
        let bytes = std::fs::read(entry.path()).map_err(|_| "activation_recovery_required")?;
        let pending: PendingActivation =
            serde_json::from_slice(&bytes).map_err(|_| "activation_recovery_required")?;
        if pending.schema != 1 || pending.status != "downloaded" {
            return Err("activation_recovery_required");
        }
        InstalledRuntime::with_node_version(
            &pending.version,
            pending.manifest_digest.clone(),
            &pending.node_version,
        )
        .map_err(|_| "activation_recovery_required")?;
        crate::runtime::install_state::DataGeneration::new(&pending.generation)
            .map_err(|_| "activation_recovery_required")?;
        let confirmation = confirmation_path(paths, &pending);
        if confirmation.is_file() {
            let document: ActivationConfirmation = serde_json::from_slice(
                &std::fs::read(&confirmation).map_err(|_| "activation_recovery_required")?,
            )
            .map_err(|_| "activation_recovery_required")?;
            if document.schema != 2
                || document.status != "confirmed"
                || document.version != pending.version
                || document.manifest_digest != pending.manifest_digest
                || document.generation != pending.generation
                || !valid_pointer_identity(&document.prior_pointer)
            {
                return Err("activation_recovery_required");
            }
            let receipt = activation_receipt(paths, &pending);
            if receipt.is_file() {
                let receipt: ActivationReceipt = serde_json::from_slice(
                    &std::fs::read(&receipt).map_err(|_| "activation_recovery_required")?,
                )
                .map_err(|_| "activation_recovery_required")?;
                if receipt.schema != 1
                    || receipt.version != pending.version
                    || receipt.manifest_digest != pending.manifest_digest
                    || receipt.generation != pending.generation
                    || !matches!(
                        receipt.outcome,
                        UpdateUiPhase::UpToDate
                            | UpdateUiPhase::RollingBack
                            | UpdateUiPhase::Failed
                            | UpdateUiPhase::RecoveryRequired
                    )
                {
                    return Err("activation_recovery_required");
                }
                match receipt.outcome {
                    UpdateUiPhase::UpToDate | UpdateUiPhase::RollingBack => continue,
                    UpdateUiPhase::Failed => {
                        retryable_failure = true;
                        continue;
                    }
                    UpdateUiPhase::RecoveryRequired => {
                        unresolved_recovery = true;
                        continue;
                    }
                    _ => return Err("activation_recovery_required"),
                }
            }
            confirmed.push(pending);
        }
    }
    if confirmed.len() > 1 {
        Err("activation_recovery_required")
    } else if let Some(pending) = confirmed.pop() {
        // 只有新的显式 confirmation 能越过旧失败 receipt；未确认的新 pending
        // 不会清除 RecoveryRequired，也不会形成自动冷启动重试循环。
        Ok(Some(pending))
    } else if unresolved_recovery {
        Err("activation_recovery_required")
    } else if retryable_failure {
        Err("activation_retry_available")
    } else {
        Ok(None)
    }
}

fn activation_receipt(paths: &AppPaths, pending: &PendingActivation) -> std::path::PathBuf {
    paths.settings.join(format!(
        "activation-outcome-{}-{}-{}.json",
        pending.version,
        &pending.manifest_digest[..12],
        pending.generation
    ))
}

fn confirmation_path(paths: &AppPaths, pending: &PendingActivation) -> std::path::PathBuf {
    paths.settings.join(format!(
        "activation-confirmed-{}-{}-{}.json",
        pending.version,
        &pending.manifest_digest[..12],
        pending.generation
    ))
}

#[cfg(any(not(debug_assertions), test))]
fn write_activation_receipt(
    paths: &AppPaths,
    pending: &PendingActivation,
    phase: &UpdateUiPhase,
) -> std::io::Result<()> {
    let path = activation_receipt(paths, pending);
    let bytes = serde_json::to_vec(&ActivationReceipt {
        schema: 1,
        version: pending.version.clone(),
        manifest_digest: pending.manifest_digest.clone(),
        generation: pending.generation.clone(),
        outcome: phase.clone(),
    })
    .map_err(std::io::Error::other)?;
    write_atomic_or_matching(&path, &bytes)
}

fn persist_confirmation(
    paths: &AppPaths,
    manifest: &VerifiedManifest,
    pending: &PendingActivation,
) -> std::io::Result<()> {
    let expected_pending = serde_json::to_vec(pending).map_err(std::io::Error::other)?;
    if std::fs::read(pending_path(paths, pending))? != expected_pending
        || pending.version != manifest.manifest.dsh_version.to_string()
        || pending.node_version != manifest.manifest.node_version.to_string()
        || pending.manifest_digest != manifest.manifest_digest
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pending activation does not match verified manifest",
        ));
    }
    let runtime = InstalledRuntime::with_node_version(
        &manifest.manifest.dsh_version.to_string(),
        manifest.manifest_digest.clone(),
        &manifest.manifest.node_version.to_string(),
    )
    .map_err(std::io::Error::other)?;
    if !RuntimeLayout::from_paths(paths)
        .runtime_dir(&runtime)
        .is_dir()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "staged runtime is missing",
        ));
    }
    persist_confirmation_record(paths, pending)
}

fn persist_confirmation_record(
    paths: &AppPaths,
    pending: &PendingActivation,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(&ActivationConfirmation {
        schema: 2,
        status: "confirmed".to_owned(),
        version: pending.version.clone(),
        manifest_digest: pending.manifest_digest.clone(),
        generation: pending.generation.clone(),
        prior_pointer: current_pointer_identity(paths).map_err(std::io::Error::other)?,
    })
    .map_err(std::io::Error::other)?;
    write_new_or_matching(&confirmation_path(paths, pending), &bytes)
}

fn current_pointer_identity(paths: &AppPaths) -> Result<String, &'static str> {
    let layout = RuntimeLayout::from_paths(paths);
    match InstallStateStore::new(layout.clone()).load() {
        Ok(_) => {
            let bytes = std::fs::read(layout.deployment_file())
                .map_err(|_| "activation_recovery_required")?;
            Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
        }
        Err(InstallStateError::NotInstalled) => Ok("uninstalled".to_owned()),
        Err(_) => Err("activation_recovery_required"),
    }
}

fn valid_pointer_identity(value: &str) -> bool {
    value == "uninstalled"
        || value.strip_prefix("sha256:").is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn create_explicit_retry_attempt(paths: &AppPaths) -> Result<PendingActivation, &'static str> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(&paths.settings).map_err(|_| "activation_recovery_required")? {
        let entry = entry.map_err(|_| "activation_recovery_required")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("pending-activation-") || !name.ends_with(".json") {
            continue;
        }
        let pending: PendingActivation = serde_json::from_slice(
            &std::fs::read(entry.path()).map_err(|_| "activation_recovery_required")?,
        )
        .map_err(|_| "activation_recovery_required")?;
        if pending.schema != 1 || pending.status != "downloaded" {
            return Err("activation_recovery_required");
        }
        InstalledRuntime::with_node_version(
            &pending.version,
            pending.manifest_digest.clone(),
            &pending.node_version,
        )
        .map_err(|_| "activation_recovery_required")?;
        crate::runtime::install_state::DataGeneration::new(&pending.generation)
            .map_err(|_| "activation_recovery_required")?;
        let receipt_path = activation_receipt(paths, &pending);
        let confirmation_path = confirmation_path(paths, &pending);
        if !receipt_path.is_file() || !confirmation_path.is_file() {
            continue;
        }
        let confirmation: ActivationConfirmation = serde_json::from_slice(
            &std::fs::read(confirmation_path).map_err(|_| "activation_recovery_required")?,
        )
        .map_err(|_| "activation_recovery_required")?;
        if confirmation.schema != 2
            || confirmation.status != "confirmed"
            || confirmation.version != pending.version
            || confirmation.manifest_digest != pending.manifest_digest
            || confirmation.generation != pending.generation
            || !valid_pointer_identity(&confirmation.prior_pointer)
        {
            return Err("activation_recovery_required");
        }
        let receipt: ActivationReceipt = serde_json::from_slice(
            &std::fs::read(receipt_path).map_err(|_| "activation_recovery_required")?,
        )
        .map_err(|_| "activation_recovery_required")?;
        if receipt.schema != 1
            || receipt.version != pending.version
            || receipt.manifest_digest != pending.manifest_digest
            || receipt.generation != pending.generation
        {
            return Err("activation_recovery_required");
        }
        if matches!(receipt.outcome, UpdateUiPhase::Failed) {
            let attempt = pending
                .generation
                .rsplit('-')
                .next()
                .and_then(|value| value.parse::<u128>().ok())
                .unwrap_or_default();
            candidates.push((attempt, pending));
        }
    }
    let (_, mut pending) = candidates
        .into_iter()
        .max_by_key(|(attempt, _)| *attempt)
        .ok_or("activation_recovery_required")?;
    let runtime = InstalledRuntime::with_node_version(
        &pending.version,
        pending.manifest_digest.clone(),
        &pending.node_version,
    )
    .map_err(|_| "activation_recovery_required")?;
    verify_installed_runtime_inventory(
        &RuntimeLayout::from_paths(paths).runtime_dir(&runtime),
        ArchiveInstallPolicy::default(),
        || true,
    )
    .map_err(|_| "activation_recovery_required")?;
    let attempt = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "activation_recovery_required")?
        .as_nanos();
    pending.generation = format!("generation-{}-{attempt}", &pending.manifest_digest[..24]);
    write_new_or_matching(
        &pending_path(paths, &pending),
        &serde_json::to_vec(&pending).map_err(|_| "activation_recovery_required")?,
    )
    .map_err(|_| "activation_recovery_required")?;
    persist_confirmation_record(paths, &pending).map_err(|_| "activation_recovery_required")?;
    Ok(pending)
}

fn write_atomic_or_matching(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if path.is_file() {
        return if std::fs::read(path)? == bytes {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "activation record conflict",
            ))
        };
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let temporary = path.with_extension(format!("json.tmp-{}-{nonce}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    crate::runtime::atomic_file::replace_file(&temporary, path)
}

fn write_new_or_matching(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_or_matching(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        ColdRecoveryPlan, ColdRecoveryResult, OrphanCandidateDisposition, PendingActivation,
        ProgressPublishThrottle, UpdateUiController, UpdateUiPhase, UpdateUiState,
        activation_receipt, apply_notice, bounded_download_percent, confirmation_path,
        finish_cold_recovery, inspect_orphan_candidate_with, load_single_confirmed_pending,
        pending_path, persist_pending, plan_cold_recovery, recover_orphan_candidate_with,
        write_activation_receipt,
    };

    #[test]
    fn download_percent_is_bounded_and_unknown_totals_stay_indeterminate() {
        assert_eq!(bounded_download_percent(0, Some(100)), Some(0));
        assert_eq!(bounded_download_percent(50, Some(100)), Some(50));
        assert_eq!(bounded_download_percent(150, Some(100)), Some(100));
        assert_eq!(
            bounded_download_percent(u64::MAX, Some(u64::MAX)),
            Some(100)
        );
        assert_eq!(bounded_download_percent(50, None), None);
        assert_eq!(bounded_download_percent(50, Some(0)), None);

        let state = UpdateUiState {
            downloaded_bytes: Some(50),
            download_percent: Some(50),
            skin_compatible: Some(false),
            ..UpdateUiState::default()
        };
        let value = serde_json::to_value(state).expect("下载状态必须可序列化");
        assert_eq!(value["downloadedBytes"], 50);
        assert_eq!(value["downloadPercent"], 50);
        assert_eq!(value["skinCompatible"], false);
    }

    #[test]
    fn download_progress_publication_is_limited_to_ten_events_per_second() {
        let started = std::time::Instant::now();
        let mut throttle = ProgressPublishThrottle::default();
        assert_eq!(throttle.delay_at(started), Duration::ZERO);
        throttle.mark_published(started);
        assert_eq!(throttle.delay_at(started), Duration::from_millis(100));
        assert_eq!(
            throttle.delay_at(started + Duration::from_millis(60)),
            Duration::from_millis(40)
        );
        assert_eq!(
            throttle.delay_at(started + Duration::from_millis(100)),
            Duration::ZERO
        );
    }
    use crate::domain::UpdateNotice;
    use crate::paths::{AppPaths, RuntimeLayout};
    use crate::runtime::install_state::{
        ActiveDeployment, DataGeneration, InstallStateStore, InstalledRuntime,
    };
    use crate::update::activation::{
        ActivationCheckpoint, ActivationError, ActivationFailure, ActivationFailureStage,
    };
    use crate::update::manifest::{
        CompatibilityManifest, CoreCompatibility, RuntimeArtifact, SkinCompatibility,
        VerifiedManifest,
    };
    use crate::update::probe::ProbeError;
    use semver::Version;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use url::Url;

    #[test]
    fn notices_keep_distinct_ui_phases_and_clear_stale_artifact_fields() {
        let cases = [
            (
                UpdateNotice::OfficialAvailable {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                },
                UpdateUiPhase::OfficialAvailable,
                None,
            ),
            (
                UpdateNotice::RuntimeAvailable {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                    compatible: "0.1.1-rc.2".to_owned(),
                },
                UpdateUiPhase::RuntimeAvailable,
                Some(true),
            ),
            (
                UpdateNotice::DesktopRequired {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                    compatible: "0.1.1-rc.2".to_owned(),
                    minimum_desktop: "0.2.0".to_owned(),
                },
                UpdateUiPhase::DesktopRequired,
                None,
            ),
            (
                UpdateNotice::SkinUnverified {
                    current: None,
                    official: "0.1.1-rc.2".to_owned(),
                    compatible: "0.1.1-rc.2".to_owned(),
                },
                UpdateUiPhase::SkinUnverified,
                Some(false),
            ),
            (
                UpdateNotice::Offline {
                    current: Some("0.1.1-rc.1".to_owned()),
                    version: None,
                    error_kind: "network".to_owned(),
                },
                UpdateUiPhase::Offline,
                None,
            ),
        ];

        for (notice, expected_phase, expected_skin_compatible) in cases {
            let mut state = UpdateUiState {
                compatible_version: Some("stale".to_owned()),
                artifact_size: Some(99),
                downloaded_bytes: Some(50),
                download_percent: Some(50),
                skin_compatible: Some(true),
                compatibility_summary: Some("stale".to_owned()),
                minimum_desktop_version: Some("9.9.9".to_owned()),
                error_code: Some("stale_error".to_owned()),
                ..UpdateUiState::default()
            };
            apply_notice(&mut state, &notice, None);

            assert!(
                std::mem::discriminant(&state.phase) == std::mem::discriminant(&expected_phase)
            );
            assert_ne!(state.compatible_version.as_deref(), Some("stale"));
            assert_eq!(state.artifact_size, None);
            assert_eq!(state.downloaded_bytes, None);
            assert_eq!(state.download_percent, None);
            assert_eq!(state.skin_compatible, expected_skin_compatible);
            assert_eq!(state.compatibility_summary, None);
            assert_ne!(state.minimum_desktop_version.as_deref(), Some("9.9.9"));
            assert_ne!(state.error_code.as_deref(), Some("stale_error"));
        }
    }

    fn test_verified_manifest() -> VerifiedManifest {
        VerifiedManifest {
            manifest: CompatibilityManifest {
                schema: 2,
                dsh_version: Version::parse("0.1.1-rc.2").unwrap(),
                node_version: Version::parse("24.15.0").unwrap(),
                minimum_desktop_version: Version::parse("0.1.0").unwrap(),
                core_compatibility: CoreCompatibility::Compatible,
                skin_compatibility: SkinCompatibility::Verified,
                platform: "windows".to_owned(),
                arch: "x86_64".to_owned(),
                artifact: RuntimeArtifact {
                    url: Url::parse("https://updates.example.invalid/runtime.zip").unwrap(),
                    size: 10,
                    sha256: [3_u8; 32],
                },
                verified_at: "2026-08-22T00:00:00Z".to_owned(),
                compatibility_summary: "verified".to_owned(),
            },
            manifest_digest: "a".repeat(64),
            desktop_version_supported: true,
        }
    }

    #[tokio::test]
    async fn install_retains_manifest_until_confirmation_is_persisted() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-confirm-manifest-{nonce}"));
        let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
        let controller = UpdateUiController::new(paths.clone());
        let manifest = test_verified_manifest();
        *controller.manifest.lock().await = Some(manifest.clone());
        let pending = persist_pending(&paths, &manifest).expect("pending fixture");

        assert_eq!(controller.manifest_for_install().await.unwrap(), manifest);
        assert!(controller.manifest.lock().await.is_some());
        assert_eq!(
            controller.persist_confirmation_and_consume(&pending).await,
            Err("update_failed")
        );
        assert!(controller.manifest.lock().await.is_some());

        let runtime =
            InstalledRuntime::with_node_version("0.1.1-rc.2", "a".repeat(64), "24.15.0").unwrap();
        std::fs::create_dir_all(RuntimeLayout::from_paths(&paths).runtime_dir(&runtime)).unwrap();
        controller
            .persist_confirmation_and_consume(&pending)
            .await
            .expect("确认持久化应成功");
        assert!(controller.manifest.lock().await.is_none());
        assert!(confirmation_path(&paths, &pending).is_file());
    }

    #[tokio::test]
    async fn missing_release_configuration_is_explicitly_unavailable() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-update-ui-{nonce}"));
        let controller = UpdateUiController::new(AppPaths::from_roots(
            &root.join("roaming"),
            &root.join("local"),
        ));
        assert!(matches!(
            controller.envelope().await.state.phase,
            UpdateUiPhase::Unavailable
        ));
    }

    #[tokio::test]
    async fn stale_revision_and_concurrent_operation_fail_closed() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-update-revision-{nonce}"));
        let controller = UpdateUiController::new(AppPaths::from_roots(
            &root.join("roaming"),
            &root.join("local"),
        ));
        assert_eq!(
            controller.require_revision(1).await,
            Err("update_state_stale")
        );
        let _first = controller.operation.try_lock().expect("first operation");
        assert!(controller.operation.try_lock().is_err());
    }

    #[test]
    fn cold_boot_accepts_only_one_strict_confirmed_pending_record() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-update-cold-{nonce}"));
        let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
        std::fs::create_dir_all(&paths.settings).unwrap();
        let pending = PendingActivation {
            schema: 1,
            status: "downloaded".to_owned(),
            version: "0.1.2".to_owned(),
            node_version: "24.15.0".to_owned(),
            manifest_digest: "a".repeat(64),
            generation: "generation-0.1.2-aaaaaaaaaaaa".to_owned(),
        };
        std::fs::write(
            pending_path(&paths, &pending),
            serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        assert!(load_single_confirmed_pending(&paths).unwrap().is_none());
        std::fs::write(confirmation_path(&paths, &pending), b"confirmed").unwrap();
        assert_eq!(
            load_single_confirmed_pending(&paths),
            Err("activation_recovery_required")
        );
        std::fs::write(
            confirmation_path(&paths, &pending),
            format!(
                r#"{{"schema":2,"status":"confirmed","version":"0.1.2","manifest_digest":"{}","generation":"{}","prior_pointer":"uninstalled"}}"#,
                "a".repeat(64), pending.generation
            ),
        )
        .unwrap();
        assert_eq!(
            load_single_confirmed_pending(&paths)
                .unwrap()
                .expect("confirmed pending")
                .version,
            "0.1.2"
        );
    }

    #[test]
    fn activation_receipt_is_atomically_committed_and_append_only() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-update-receipt-{nonce}"));
        let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
        std::fs::create_dir_all(&paths.settings).unwrap();
        let pending = PendingActivation {
            schema: 1,
            status: "downloaded".to_owned(),
            version: "0.1.2".to_owned(),
            node_version: "24.15.0".to_owned(),
            manifest_digest: "b".repeat(64),
            generation: "generation-bbbbbbbbbbbbbbbbbbbbbbbb-1".to_owned(),
        };

        write_activation_receipt(&paths, &pending, &UpdateUiPhase::UpToDate).unwrap();
        let committed = activation_receipt(&paths, &pending);
        assert!(committed.is_file());
        write_activation_receipt(&paths, &pending, &UpdateUiPhase::UpToDate).unwrap();
        assert!(
            write_activation_receipt(&paths, &pending, &UpdateUiPhase::Failed).is_err(),
            "已有终态 receipt 不得被另一结果覆盖"
        );
    }

    #[test]
    fn failed_receipt_requires_a_new_explicit_attempt_before_retry() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-update-retry-{nonce}"));
        let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
        std::fs::create_dir_all(&paths.settings).unwrap();
        let first = PendingActivation {
            schema: 1,
            status: "downloaded".to_owned(),
            version: "0.1.2".to_owned(),
            node_version: "24.15.0".to_owned(),
            manifest_digest: "c".repeat(64),
            generation: "generation-cccccccccccccccccccccccc-1".to_owned(),
        };
        write_pending_and_confirmation(&paths, &first);
        write_activation_receipt(&paths, &first, &UpdateUiPhase::Failed).unwrap();
        assert_eq!(
            load_single_confirmed_pending(&paths),
            Err("activation_retry_available")
        );

        let mut retry = first.clone();
        retry.generation = "generation-cccccccccccccccccccccccc-2".to_owned();
        write_pending_and_confirmation(&paths, &retry);
        assert_eq!(
            load_single_confirmed_pending(&paths)
                .unwrap()
                .expect("新确认的 attempt 应可重试")
                .generation,
            retry.generation
        );
    }

    fn write_pending_and_confirmation(paths: &AppPaths, pending: &PendingActivation) {
        std::fs::write(
            pending_path(paths, pending),
            serde_json::to_vec(pending).unwrap(),
        )
        .unwrap();
        std::fs::write(
            confirmation_path(paths, pending),
            format!(
                r#"{{"schema":2,"status":"confirmed","version":"{}","manifest_digest":"{}","generation":"{}","prior_pointer":"{}"}}"#,
                pending.version,
                pending.manifest_digest,
                pending.generation,
                super::current_pointer_identity(paths).unwrap()
            ),
        )
        .unwrap();
    }

    fn orphan_candidate_fixture(
        label: &str,
        with_prior: bool,
    ) -> (AppPaths, PendingActivation, Option<ActiveDeployment>) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dsh-orphan-{label}-{nonce}"));
        let paths = AppPaths::from_roots(&root.join("roaming"), &root.join("local"));
        paths.ensure_exists().unwrap();
        let pending = PendingActivation {
            schema: 1,
            status: "downloaded".to_owned(),
            version: "0.2.0".to_owned(),
            node_version: "24.15.0".to_owned(),
            manifest_digest: "d".repeat(64),
            generation: format!("generation-dddddddddddddddddddddddd-{nonce}"),
        };
        std::fs::write(
            pending_path(&paths, &pending),
            serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        let layout = RuntimeLayout::from_paths(&paths);
        std::fs::create_dir_all(
            layout.generation_dir(&DataGeneration::new(&pending.generation).unwrap()),
        )
        .unwrap();
        let prior = with_prior.then(|| {
            let runtime =
                InstalledRuntime::with_node_version("0.1.0", "e".repeat(64), "24.15.0").unwrap();
            let data = DataGeneration::new("generation-prior").unwrap();
            std::fs::create_dir_all(layout.runtime_dir(&runtime)).unwrap();
            std::fs::create_dir_all(layout.generation_dir(&data)).unwrap();
            let workspace = paths.dsh_home.join("projects/prior");
            std::fs::create_dir_all(&workspace).unwrap();
            let active = ActiveDeployment::with_project_workspace(
                runtime,
                data,
                "epoch-prior".to_owned(),
                workspace,
            );
            InstallStateStore::new(layout.clone())
                .save(&active)
                .unwrap();
            InstallStateStore::new(layout.clone()).load().unwrap()
        });
        std::fs::write(
            confirmation_path(&paths, &pending),
            serde_json::to_vec(&super::ActivationConfirmation {
                schema: 2,
                status: "confirmed".to_owned(),
                version: pending.version.clone(),
                manifest_digest: pending.manifest_digest.clone(),
                generation: pending.generation.clone(),
                prior_pointer: super::current_pointer_identity(&paths).unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        (paths, pending, prior)
    }

    fn write_activation_journal(paths: &AppPaths, state: &str, label: &str) {
        let root = paths.settings.join("activation-journal");
        std::fs::create_dir_all(&root).unwrap();
        let document = serde_json::json!({
            "schema": 2,
            "activation_id": format!("history-{label}"),
            "prior": null,
            "target": {
                "runtime_version": "0.0.9",
                "manifest_digest": "9".repeat(64),
                "node_version": "24.15.0",
                "data_id": "generation-history",
                "activated_at": "epoch-history",
                "project_workspace": paths.dsh_home.join("projects/history").to_string_lossy()
            },
            "state": state,
            "failure": null
        });
        std::fs::write(
            root.join(format!("history-{label}.json")),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn historical_active_journal_allows_retryable_orphan_and_restarts_exact_prior() {
        let (paths, pending, prior) = orphan_candidate_fixture("active", true);
        write_activation_journal(&paths, "active", "active");
        std::fs::write(
            paths
                .settings
                .join("activation-journal/diagnostic-note.txt"),
            b"ignored non-journal entry",
        )
        .unwrap();
        let restarts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&restarts);
        let expected = prior.clone().unwrap();
        let result = recover_orphan_candidate_with(
            &paths,
            &pending,
            |_, _| Ok(()),
            move |actual| async move {
                assert_eq!(actual, expected);
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("valid residual");
        assert_eq!(
            result,
            OrphanCandidateDisposition::RestartPrior(prior.unwrap())
        );
        assert_eq!(restarts.load(Ordering::SeqCst), 1);
        let receipt: super::ActivationReceipt =
            serde_json::from_slice(&std::fs::read(activation_receipt(&paths, &pending)).unwrap())
                .unwrap();
        assert!(matches!(receipt.outcome, UpdateUiPhase::Failed));
    }

    #[tokio::test]
    async fn failed_orphan_prior_restart_is_terminal_and_never_retried_on_next_load() {
        let (paths, pending, _) = orphan_candidate_fixture("restart-failed", true);
        let restarts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&restarts);
        assert_eq!(
            recover_orphan_candidate_with(
                &paths,
                &pending,
                |_, _| Ok(()),
                move |_| async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Err(())
                },
            )
            .await,
            Err("activation_recovery_required")
        );
        assert_eq!(restarts.load(Ordering::SeqCst), 1);
        let receipt: super::ActivationReceipt =
            serde_json::from_slice(&std::fs::read(activation_receipt(&paths, &pending)).unwrap())
                .unwrap();
        assert!(matches!(receipt.outcome, UpdateUiPhase::RecoveryRequired));

        // 第二次 cold load 必须在 receipt 门禁处停止，不再把 attempt 交给 restart。
        if let Ok(Some(next)) = load_single_confirmed_pending(&paths) {
            let observed = Arc::clone(&restarts);
            let _ = recover_orphan_candidate_with(
                &paths,
                &next,
                |_, _| Ok(()),
                move |_| async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
        }
        assert_eq!(
            load_single_confirmed_pending(&paths),
            Err("activation_recovery_required")
        );
        assert_eq!(restarts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn historical_rolled_back_journal_allows_new_orphan_classification() {
        let (paths, pending, prior) = orphan_candidate_fixture("rolled-back", true);
        write_activation_journal(&paths, "rolled_back", "rolled-back");
        assert_eq!(
            inspect_orphan_candidate_with(&paths, &pending, |_, _| Ok(())).unwrap(),
            OrphanCandidateDisposition::RestartPrior(prior.unwrap())
        );
    }

    #[test]
    fn unresolved_journal_states_reject_orphan_classification() {
        for state in ["prepared", "recovery_required"] {
            let (paths, pending, _) = orphan_candidate_fixture(state, true);
            write_activation_journal(&paths, state, state);
            assert_eq!(
                inspect_orphan_candidate_with(&paths, &pending, |_, _| Ok(())),
                Err("activation_recovery_required")
            );
        }
    }

    #[test]
    fn malformed_or_unknown_journal_fails_closed() {
        for (label, bytes) in [
            ("truncated", b"{".as_slice()),
            (
                "unknown-schema",
                br#"{"schema":99,"activation_id":"unknown","prior":null,"target":{"runtime_version":"0.0.9","manifest_digest":"9999999999999999999999999999999999999999999999999999999999999999","node_version":"24.15.0","data_id":"generation-history","activated_at":"epoch-history","project_workspace":"C:\\workspace"},"state":"active","failure":null}"#,
            ),
        ] {
            let (paths, pending, _) = orphan_candidate_fixture(label, true);
            let root = paths.settings.join("activation-journal");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(format!("{label}.json")), bytes).unwrap();
            assert_eq!(
                inspect_orphan_candidate_with(&paths, &pending, |_, _| Ok(())),
                Err("activation_recovery_required")
            );
        }
    }

    #[test]
    fn orphan_candidate_without_journal_keeps_fresh_install_uninstalled() {
        let (paths, pending, _) = orphan_candidate_fixture("fresh", false);
        assert_eq!(
            inspect_orphan_candidate_with(&paths, &pending, |_, _| Ok(())).unwrap(),
            OrphanCandidateDisposition::RetryFresh
        );
    }

    #[test]
    fn orphan_candidate_with_invalid_pointer_requires_recovery() {
        let (paths, pending, _) = orphan_candidate_fixture("invalid", false);
        std::fs::write(
            RuntimeLayout::from_paths(&paths).deployment_file(),
            b"truncated",
        )
        .unwrap();
        assert_eq!(
            inspect_orphan_candidate_with(&paths, &pending, |_, _| Ok(())),
            Err("activation_recovery_required")
        );
    }

    #[test]
    fn orphan_candidate_with_valid_but_mismatched_pointer_requires_recovery() {
        let (paths, pending, _) = orphan_candidate_fixture("mismatch", true);
        let layout = RuntimeLayout::from_paths(&paths);
        let runtime =
            InstalledRuntime::with_node_version("0.1.1", "f".repeat(64), "24.15.0").unwrap();
        let data = DataGeneration::new("generation-other").unwrap();
        std::fs::create_dir_all(layout.runtime_dir(&runtime)).unwrap();
        std::fs::create_dir_all(layout.generation_dir(&data)).unwrap();
        let workspace = paths.dsh_home.join("projects/other");
        std::fs::create_dir_all(&workspace).unwrap();
        InstallStateStore::new(layout)
            .save(&ActiveDeployment::with_project_workspace(
                runtime,
                data,
                "epoch-other".to_owned(),
                workspace,
            ))
            .unwrap();
        assert_eq!(
            inspect_orphan_candidate_with(&paths, &pending, |_, _| Ok(())),
            Err("activation_recovery_required")
        );
    }

    #[test]
    fn precommit_activation_failures_are_retryable_only_with_an_exact_pointer() {
        for source in [
            ActivationError::Interrupted {
                checkpoint: ActivationCheckpoint::CandidatePrepared,
            },
            ActivationError::SnapshotLimit,
            ActivationError::UnsafeSnapshot,
            ActivationError::ProbeRejected,
            ActivationError::Probe(ProbeError::Cancelled),
            ActivationError::WorkerFailed,
            ActivationError::Io(std::io::Error::other("injected precommit I/O")),
        ] {
            let error = ActivationError::Precommit {
                stage: crate::update::activation::PrecommitStage::Candidate,
                source: Box::new(source),
            };
            assert_eq!(
                plan_cold_recovery(&error, true, true),
                ColdRecoveryPlan::RestartPrior
            );
            assert_eq!(
                plan_cold_recovery(&error, false, true),
                ColdRecoveryPlan::RetryFresh
            );
            assert_eq!(
                plan_cold_recovery(&error, true, false),
                ColdRecoveryPlan::RecoveryRequired
            );
        }
        assert_eq!(
            plan_cold_recovery(
                &ActivationError::Interrupted {
                    checkpoint: ActivationCheckpoint::PointerCommitted,
                },
                true,
                true,
            ),
            ColdRecoveryPlan::RecoveryRequired
        );
        assert_eq!(
            plan_cold_recovery(
                &ActivationError::RecoveryRequired {
                    failure: ActivationFailure {
                        stage: ActivationFailureStage::RecoveryResume,
                        error_code: "runtime_start_failed".to_owned(),
                    },
                    recovery_code: "pointer_mismatch".to_owned(),
                },
                true,
                true,
            ),
            ColdRecoveryPlan::RecoveryRequired
        );
    }

    #[test]
    fn prior_restart_failure_escalates_to_recovery_required() {
        assert_eq!(
            finish_cold_recovery(ColdRecoveryPlan::RestartPrior, true),
            ColdRecoveryResult::Retryable
        );
        assert_eq!(
            finish_cold_recovery(ColdRecoveryPlan::RestartPrior, false),
            ColdRecoveryResult::RecoveryRequired
        );
        assert_eq!(
            finish_cold_recovery(ColdRecoveryPlan::RetryFresh, false),
            ColdRecoveryResult::Retryable
        );
    }
}
