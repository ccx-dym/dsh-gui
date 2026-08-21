use std::{
    fs::OpenOptions,
    io::Write,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tokio::sync::Mutex;
use url::Url;

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
            ArtifactDownloader, DownloadCancellation, DownloadPolicy, DownloadRequest,
            HttpsDownloader,
        },
        manifest::{ManifestVerifier, VerifiedManifest},
        version_source::{
            NpmOfficialVersionSource, ReqwestSourceTransport, SignedCompatibilitySource,
            SourcePolicy,
        },
    },
};

#[cfg(not(debug_assertions))]
use crate::update::{
    activation::{
        ActivationCheckpointSink, ActivationOutcome, ActivationRequest, RuntimeActivator,
        RuntimeProbeAdapter, SnapshotPolicy,
    },
    probe::{ProbePolicy, RuntimeProbe},
};

const UPDATE_EVENT: &str = "update-state";

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
    OfficialAwaitingCompatibility,
    CompatibleAvailable,
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
    pub compatibility_summary: Option<String>,
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
            compatibility_summary: None,
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
            | UpdateUiPhase::OfficialAwaitingCompatibility
            | UpdateUiPhase::CompatibleAvailable
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
        *controller.manifest.lock().await = None;
        controller.publish(app, next).await;
    }
    let Some(config) = release_config() else {
        if scheduled {
            return Ok(controller.envelope().await);
        }
        let mut unavailable = controller.state.lock().await.clone();
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
    use crate::domain::UpdateNotice;
    match result.notice {
        UpdateNotice::UpToDate { official, .. } => {
            checked.phase = UpdateUiPhase::UpToDate;
            checked.official_version = Some(official);
        }
        UpdateNotice::OfficialAwaitingCompatibility { official, .. } => {
            checked.phase = UpdateUiPhase::OfficialAwaitingCompatibility;
            checked.official_version = Some(official);
        }
        UpdateNotice::CompatibleAvailable {
            official,
            compatible,
            ..
        } => {
            checked.phase = UpdateUiPhase::CompatibleAvailable;
            checked.official_version = Some(official);
            checked.compatible_version = Some(compatible);
            if let Some(manifest) = &result.compatible_manifest {
                checked.artifact_size = Some(manifest.manifest.artifact.size);
                checked.compatibility_summary =
                    Some(manifest.manifest.compatibility_summary.clone());
            }
        }
        UpdateNotice::CheckFailed { .. } => {
            checked.phase = UpdateUiPhase::Failed;
            checked.error_code = Some("update_check_failed".to_owned());
        }
    }
    *controller.manifest.lock().await = result.compatible_manifest;
    controller.publish(app, checked).await;
    let state = controller.envelope().await;
    if state.state.should_notify {
        let notification = match state.state.phase {
            UpdateUiPhase::OfficialAwaitingCompatibility => {
                Some(("DSH 新版本正在兼容验证", "验证完成后桌面端会开放安全安装。"))
            }
            UpdateUiPhase::CompatibleAvailable => Some((
                "DSH 兼容更新已就绪",
                "打开 DSH Desktop 查看版本并确认下载。",
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
                        | UpdateUiPhase::OfficialAwaitingCompatibility
                        | UpdateUiPhase::CompatibleAvailable
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
        UpdateUiPhase::CompatibleAvailable
    ) {
        return Err("update_transition_denied");
    }
    let manifest = controller
        .manifest
        .lock()
        .await
        .clone()
        .ok_or("compatible_update_missing")?;
    let mut downloading = controller.state.lock().await.clone();
    downloading.phase = UpdateUiPhase::Downloading;
    downloading.should_notify = false;
    controller.publish(&app, downloading).await;
    let diagnostics = DiagnosticContext::begin(TraceKind::Update, Arc::new(sink.inner().clone()));
    let pending_activation =
        match download_and_stage(&controller, &app, &manifest, &diagnostics).await {
            Ok(pending) => pending,
            Err(code) => {
                let mut failed = controller.state.lock().await.clone();
                failed.phase = UpdateUiPhase::Failed;
                failed.error_code = Some(code.to_owned());
                controller.publish(&app, failed).await;
                return Err(code);
            }
        };
    *controller.pending.lock().await = Some(pending_activation);
    let mut pending = controller.state.lock().await.clone();
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
    let downloaded = downloader
        .download(
            DownloadRequest {
                artifact: &manifest.manifest.artifact,
                updates_dir: &controller.paths.updates,
                cancellation: DownloadCancellation::default(),
            },
            diagnostics,
        )
        .await
        .map_err(|_| "update_failed")?;
    let mut verifying = controller.state.lock().await.clone();
    verifying.phase = UpdateUiPhase::Verifying;
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
    let manifest = controller
        .manifest
        .lock()
        .await
        .clone()
        .ok_or("compatible_update_missing")?;
    let pending = controller
        .pending
        .lock()
        .await
        .clone()
        .ok_or("compatible_update_missing")?;
    persist_confirmation(&controller.paths, &manifest, &pending).map_err(|_| "update_failed")?;
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
        // 没有 terminal receipt 却已有 candidate，表示在 journal 前崩溃或外部篡改；
        // 保留现场并停止自动重试，避免每次启动形成激活循环。
        write_activation_receipt(
            &update_controller.paths,
            &pending,
            &UpdateUiPhase::RecoveryRequired,
        )
        .map_err(|_| "activation_recovery_required")?;
        update_controller
            .publish_cold_phase(
                app,
                UpdateUiPhase::RecoveryRequired,
                Some("candidate_without_terminal_journal"),
            )
            .await;
        return Err("activation_recovery_required");
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
            if document.schema != 1
                || document.status != "confirmed"
                || document.version != pending.version
                || document.manifest_digest != pending.manifest_digest
                || document.generation != pending.generation
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
        schema: 1,
        status: "confirmed".to_owned(),
        version: pending.version.clone(),
        manifest_digest: pending.manifest_digest.clone(),
        generation: pending.generation.clone(),
    })
    .map_err(std::io::Error::other)?;
    write_new_or_matching(&confirmation_path(paths, pending), &bytes)
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
        if confirmation.schema != 1
            || confirmation.status != "confirmed"
            || confirmation.version != pending.version
            || confirmation.manifest_digest != pending.manifest_digest
            || confirmation.generation != pending.generation
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
        ColdRecoveryPlan, ColdRecoveryResult, PendingActivation, UpdateUiController, UpdateUiPhase,
        activation_receipt, confirmation_path, finish_cold_recovery, load_single_confirmed_pending,
        pending_path, plan_cold_recovery, write_activation_receipt,
    };
    use crate::paths::AppPaths;
    use crate::update::activation::{
        ActivationCheckpoint, ActivationError, ActivationFailure, ActivationFailureStage,
    };
    use crate::update::probe::ProbeError;
    use std::time::{SystemTime, UNIX_EPOCH};

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
                r#"{{"schema":1,"status":"confirmed","version":"0.1.2","manifest_digest":"{}","generation":"{}"}}"#,
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
                r#"{{"schema":1,"status":"confirmed","version":"{}","manifest_digest":"{}","generation":"{}"}}"#,
                pending.version, pending.manifest_digest, pending.generation
            ),
        )
        .unwrap();
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
