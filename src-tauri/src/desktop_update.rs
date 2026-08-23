use std::{
    fs::{File, OpenOptions},
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::domain::{
    DesktopRelease, DesktopUpdateEnvelope, DesktopUpdateErrorKind, DesktopUpdateState,
};

const DESKTOP_UPDATE_STATE_FILE: &str = "desktop-update-state.json";
const DESKTOP_UPDATE_TEMP_FILE: &str = ".desktop-update-state.tmp";
const DESKTOP_UPDATE_STATE_SCHEMA: u32 = 1;
const MAX_DESKTOP_UPDATE_STATE_BYTES: u64 = 64 * 1024;

/// 桌面更新后端的固定失败类型；动态网络或安装器正文不得越过此边界。
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DesktopUpdateError {
    #[error("desktop update is offline")]
    Offline,
    #[error("desktop update metadata is invalid")]
    InvalidMetadata,
    #[error("desktop update signature is invalid")]
    SignatureInvalid,
    #[error("desktop update installation failed")]
    InstallFailed,
}

impl DesktopUpdateError {
    fn kind(self) -> DesktopUpdateErrorKind {
        match self {
            Self::Offline => DesktopUpdateErrorKind::Offline,
            Self::InvalidMetadata => DesktopUpdateErrorKind::InvalidMetadata,
            Self::SignatureInvalid => DesktopUpdateErrorKind::SignatureInvalid,
            Self::InstallFailed => DesktopUpdateErrorKind::InstallFailed,
        }
    }
}

/// 可替换的桌面更新检查与安装边界。
pub trait DesktopUpdateBackend: Send + Sync {
    /// 检查独立桌面发布通道。
    ///
    /// :return: 没有新版本时返回 `None`，有新版本时返回已验证发布记录。
    /// :raises DesktopUpdateError: 网络、元数据或签名验证失败时返回固定类别。
    fn check<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<DesktopRelease>, DesktopUpdateError>> + Send + 'a>>;

    /// 下载并安装一个已经由检查阶段选定的桌面发布。
    ///
    /// :param release: 检查阶段返回且由控制器持有的发布记录。
    /// :return: 安装器成功接管更新时返回 `()`。
    /// :raises DesktopUpdateError: 下载、签名复核或安装失败时返回固定类别。
    fn install<'a>(
        &'a self,
        release: DesktopRelease,
    ) -> Pin<Box<dyn Future<Output = Result<(), DesktopUpdateError>> + Send + 'a>>;
}

/// Tauri updater 的 Rust-only 适配器；候选安装句柄不会暴露给任何 WebView。
pub struct TauriDesktopUpdateBackend {
    app: AppHandle,
    selected: Mutex<Option<Update>>,
}

impl TauriDesktopUpdateBackend {
    /// 创建只绑定当前应用句柄的 updater 后端。
    ///
    /// :param app: 当前 Tauri 应用句柄。
    /// :return: 尚未选择任何远程版本的后端。
    /// :raises: 构造过程不访问网络，不产生错误。
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            selected: Mutex::new(None),
        }
    }
}

impl DesktopUpdateBackend for TauriDesktopUpdateBackend {
    fn check<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<DesktopRelease>, DesktopUpdateError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut updater = self.app.updater_builder();
            if let Some(proxy) = crate::network_proxy::current_user_proxy() {
                updater = updater.proxy(proxy);
            }
            // 插件默认没有总超时；固定上限避免代理或网络异常让 UI 永久停在 checking。
            let updater = updater
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(classify_updater_check_error)?;
            let candidate = updater
                .check()
                .await
                .map_err(classify_updater_check_error)?;
            let Some(update) = candidate else {
                *self.selected.lock().await = None;
                return Ok(None);
            };
            let version =
                Version::parse(&update.version).map_err(|_| DesktopUpdateError::InvalidMetadata)?;
            let release = DesktopRelease {
                version,
                notes: update.body.clone(),
                published_at: update.date.map(|value| value.to_string()),
            };
            *self.selected.lock().await = Some(update);
            Ok(Some(release))
        })
    }

    fn install<'a>(
        &'a self,
        release: DesktopRelease,
    ) -> Pin<Box<dyn Future<Output = Result<(), DesktopUpdateError>> + Send + 'a>> {
        Box::pin(async move {
            let update = self
                .selected
                .lock()
                .await
                .take()
                .ok_or(DesktopUpdateError::InstallFailed)?;
            let selected_version =
                Version::parse(&update.version).map_err(|_| DesktopUpdateError::InvalidMetadata)?;
            if selected_version != release.version {
                return Err(DesktopUpdateError::InvalidMetadata);
            }
            // 先完整下载并校验 Tauri 签名，只有签名通过后才停止 DSH。这样断网或恶意
            // 制品不会打断当前任务；安装器接管失败时再尽力恢复原 runtime。
            let bytes = update
                .download(|_, _| {}, || {})
                .await
                .map_err(classify_updater_install_error)?;
            let runtime = self
                .app
                .try_state::<crate::app_controller::AppController>()
                .ok_or(DesktopUpdateError::InstallFailed)?;
            runtime
                .stop()
                .map_err(|_| DesktopUpdateError::InstallFailed)?;
            match update.install(&bytes) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = runtime.restart();
                    Err(classify_updater_install_error(error))
                }
            }
        })
    }
}

fn classify_updater_check_error(error: tauri_plugin_updater::Error) -> DesktopUpdateError {
    use tauri_plugin_updater::Error;

    match error {
        Error::Reqwest(_) | Error::Network(_) => DesktopUpdateError::Offline,
        Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) => {
            DesktopUpdateError::SignatureInvalid
        }
        Error::EmptyEndpoints
        | Error::Semver(_)
        | Error::Serialization(_)
        | Error::ReleaseNotFound
        | Error::UnsupportedArch
        | Error::UnsupportedOs
        | Error::UrlParse(_)
        | Error::TargetNotFound(_)
        | Error::TargetsNotFound(_)
        | Error::InsecureTransportProtocol => DesktopUpdateError::InvalidMetadata,
        _ => DesktopUpdateError::InstallFailed,
    }
}

fn classify_updater_install_error(error: tauri_plugin_updater::Error) -> DesktopUpdateError {
    use tauri_plugin_updater::Error;

    match error {
        Error::Reqwest(_) | Error::Network(_) => DesktopUpdateError::Offline,
        Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) => {
            DesktopUpdateError::SignatureInvalid
        }
        _ => DesktopUpdateError::InstallFailed,
    }
}

/// 本地命令共享的桌面更新服务。
pub struct DesktopUpdateService {
    controller: DesktopUpdateController,
    backend: TauriDesktopUpdateBackend,
}

impl DesktopUpdateService {
    /// 使用私有设置目录和当前客户端版本创建服务。
    ///
    /// :param settings_dir: 应用私有设置目录。
    /// :param current_version: 当前客户端版本。
    /// :param app: 当前 Tauri 应用句柄。
    /// :return: 相互隔离的状态机与 Tauri updater 后端。
    /// :raises: 构造过程不访问网络，不产生错误。
    pub fn new(settings_dir: PathBuf, current_version: Version, app: AppHandle) -> Self {
        Self {
            controller: DesktopUpdateController::new(settings_dir, current_version),
            backend: TauriDesktopUpdateBackend::new(app),
        }
    }
}

fn require_local_desktop_update(window: &WebviewWindow) -> Result<(), &'static str> {
    let url = window.url().map_err(|_| "desktop_update_origin_denied")?;
    if crate::update_command_allowed_for_url(url.as_str()) {
        Ok(())
    } else {
        Err("desktop_update_origin_denied")
    }
}

fn desktop_update_error_code(error: DesktopUpdateError) -> &'static str {
    match error {
        DesktopUpdateError::Offline => "desktop_update_offline",
        DesktopUpdateError::InvalidMetadata => "desktop_update_invalid_metadata",
        DesktopUpdateError::SignatureInvalid => "desktop_update_signature_invalid",
        DesktopUpdateError::InstallFailed => "desktop_update_install_failed",
    }
}

/// 获取桌面客户端更新状态。
///
/// :param window: 发起调用的本地 WebView。
/// :param app: 桌面更新状态事件出口。
/// :param service: 桌面更新服务。
/// :return: 当前独立 revision 快照。
/// :raises: 非本地来源返回固定拒绝码。
#[tauri::command]
pub async fn get_desktop_update_state(
    window: WebviewWindow,
    service: tauri::State<'_, DesktopUpdateService>,
) -> Result<DesktopUpdateEnvelope, &'static str> {
    require_local_desktop_update(&window)?;
    Ok(service.controller.snapshot().await)
}

/// 检查签名桌面发布通道。
///
/// :param window: 发起调用的本地 WebView。
/// :param app: 桌面更新状态事件出口。
/// :param service: 桌面更新服务。
/// :return: 检查后的独立 revision 快照。
/// :raises: 来源、网络、元数据或状态持久化失败时返回固定错误码。
#[tauri::command]
pub async fn check_desktop_update(
    window: WebviewWindow,
    app: AppHandle,
    service: tauri::State<'_, DesktopUpdateService>,
) -> Result<DesktopUpdateEnvelope, &'static str> {
    require_local_desktop_update(&window)?;
    let result = service.controller.check(&service.backend).await;
    let snapshot = match result {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let snapshot = service.controller.snapshot().await;
            let _ = app.emit("desktop-update-state", &snapshot);
            return Err(desktop_update_error_code(error));
        }
    };
    let _ = app.emit("desktop-update-state", &snapshot);
    Ok(snapshot)
}

/// 安装最后一次检查选定的签名客户端版本。
///
/// :param window: 发起调用的本地 WebView。
/// :param service: 桌面更新服务。
/// :param expected_revision: 前端最后观察到的 revision。
/// :return: 安装器接管后的独立 revision 快照。
/// :raises: 来源、旧 revision、签名、下载或安装失败时返回固定错误码。
#[tauri::command]
pub async fn install_desktop_update(
    window: WebviewWindow,
    app: AppHandle,
    service: tauri::State<'_, DesktopUpdateService>,
    expected_revision: u64,
) -> Result<DesktopUpdateEnvelope, &'static str> {
    require_local_desktop_update(&window)?;
    let result = service
        .controller
        .install(expected_revision, &service.backend)
        .await;
    let snapshot = match result {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let snapshot = service.controller.snapshot().await;
            let _ = app.emit("desktop-update-state", &snapshot);
            return Err(desktop_update_error_code(error));
        }
    };
    let _ = app.emit("desktop-update-state", &snapshot);
    Ok(snapshot)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedDesktopUpdateState {
    schema: u32,
    revision: u64,
    state: DesktopUpdateState,
}

/// 仅管理桌面客户端更新的状态机；不持有任何 runtime 或用户数据路径。
pub struct DesktopUpdateController {
    state_file: PathBuf,
    current_version: Version,
    state: Mutex<DesktopUpdateEnvelope>,
    selected_release: Mutex<Option<DesktopRelease>>,
    operation: Mutex<()>,
}

impl DesktopUpdateController {
    /// 在设置目录下创建桌面更新控制器。
    ///
    /// 调用方只能提供设置目录；状态文件名由控制器固定，避免把任意外部路径当作写入目标。
    ///
    /// :param settings_dir: 应用已解析的私有设置目录。
    /// :param current_version: 当前正在运行的桌面客户端版本，用于拒绝回放与降级。
    /// :return: 从严格状态文件恢复或以不可用状态启动的控制器。
    /// :raises: 构造过程不传播动态错误；损坏状态安全降级为固定失败状态。
    pub fn new(settings_dir: PathBuf, current_version: Version) -> Self {
        let state_file = settings_dir.join(DESKTOP_UPDATE_STATE_FILE);
        let state = load_state(&state_file);
        Self {
            state_file,
            current_version,
            state: Mutex::new(state),
            selected_release: Mutex::new(None),
            operation: Mutex::new(()),
        }
    }

    /// 返回桌面更新状态的独立 revision 快照。
    ///
    /// :return: 当前完整状态；不会读取或修改 runtime 与用户数据。
    /// :raises: 内存快照不产生错误。
    pub async fn snapshot(&self) -> DesktopUpdateEnvelope {
        self.state.lock().await.clone()
    }

    /// 串行检查桌面客户端发布通道。
    ///
    /// :param backend: 只负责桌面发布的可注入后端。
    /// :return: `up_to_date` 或 `available` 的新 revision 快照。
    /// :raises DesktopUpdateError: 并发、持久化或后端失败时返回固定类别并失败关闭。
    pub async fn check<B: DesktopUpdateBackend + ?Sized>(
        &self,
        backend: &B,
    ) -> Result<DesktopUpdateEnvelope, DesktopUpdateError> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| DesktopUpdateError::InstallFailed)?;
        self.transition(DesktopUpdateState::Checking).await?;
        *self.selected_release.lock().await = None;

        match backend.check().await {
            Ok(Some(release)) => {
                if release.version.cmp_precedence(&self.current_version)
                    != std::cmp::Ordering::Greater
                {
                    self.transition(DesktopUpdateState::Failed {
                        error_kind: DesktopUpdateErrorKind::InvalidMetadata,
                    })
                    .await?;
                    return Err(DesktopUpdateError::InvalidMetadata);
                }
                let state = DesktopUpdateState::Available {
                    version: release.version.to_string(),
                    notes: release.notes.clone(),
                    published_at: release.published_at.clone(),
                };
                let envelope = self.transition(state).await?;
                *self.selected_release.lock().await = Some(release);
                Ok(envelope)
            }
            Ok(None) => self.transition(DesktopUpdateState::UpToDate).await,
            Err(error) => {
                self.transition(DesktopUpdateState::Failed {
                    error_kind: error.kind(),
                })
                .await?;
                Err(error)
            }
        }
    }

    /// 安装检查阶段选定的桌面客户端发布。
    ///
    /// :param expected_revision: 调用方最后看到的桌面更新 revision。
    /// :param backend: 只负责桌面安装的可注入后端。
    /// :return: 安装器成功接管后处于 `installing` 的新 revision 快照。
    /// :raises DesktopUpdateError: revision、并发、候选、持久化或安装失败时返回固定类别。
    pub async fn install<B: DesktopUpdateBackend + ?Sized>(
        &self,
        expected_revision: u64,
        backend: &B,
    ) -> Result<DesktopUpdateEnvelope, DesktopUpdateError> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| DesktopUpdateError::InstallFailed)?;
        let current = self.snapshot().await;
        if current.revision != expected_revision
            || !matches!(current.state, DesktopUpdateState::Available { .. })
        {
            return Err(DesktopUpdateError::InstallFailed);
        }
        let release = self
            .selected_release
            .lock()
            .await
            .clone()
            .ok_or(DesktopUpdateError::InstallFailed)?;
        let version = release.version.to_string();
        self.transition(DesktopUpdateState::Downloading {
            version: version.clone(),
        })
        .await?;
        match backend.install(release).await {
            Ok(()) => {
                *self.selected_release.lock().await = None;
                self.transition(DesktopUpdateState::Installing { version })
                    .await
            }
            Err(error) => {
                *self.selected_release.lock().await = None;
                self.transition(DesktopUpdateState::Failed {
                    error_kind: error.kind(),
                })
                .await?;
                Err(error)
            }
        }
    }

    async fn transition(
        &self,
        next_state: DesktopUpdateState,
    ) -> Result<DesktopUpdateEnvelope, DesktopUpdateError> {
        let current = self.state.lock().await.clone();
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(DesktopUpdateError::InstallFailed)?;
        let next = DesktopUpdateEnvelope {
            revision,
            state: next_state,
        };
        persist_state(&self.state_file, &next)?;
        *self.state.lock().await = next.clone();
        Ok(next)
    }
}

fn load_state(path: &Path) -> DesktopUpdateEnvelope {
    let Some(parent) = path.parent() else {
        return failed_initial_state();
    };
    let directory_guard = match open_directory_guard(parent) {
        Ok(guard) => guard,
        Err(()) if !parent.exists() => return DesktopUpdateEnvelope::default(),
        Err(()) => return failed_initial_state(),
    };
    let mut guard = match open_optional_state_file(path) {
        Ok(Some(guard)) => guard,
        Ok(None) => return DesktopUpdateEnvelope::default(),
        Err(()) => return failed_initial_state(),
    };
    let Ok(bytes) = read_state_bounded(&mut guard) else {
        return failed_initial_state();
    };
    if validate_directory_guard(&directory_guard, parent).is_err()
        || validate_state_guard(&guard, path).is_err()
    {
        return failed_initial_state();
    }
    let Ok(persisted) = serde_json::from_slice::<PersistedDesktopUpdateState>(&bytes) else {
        return failed_initial_state();
    };
    if persisted.schema != DESKTOP_UPDATE_STATE_SCHEMA {
        return failed_initial_state();
    }
    let state = match persisted.state {
        DesktopUpdateState::Checking
        | DesktopUpdateState::Available { .. }
        | DesktopUpdateState::Downloading { .. }
        | DesktopUpdateState::Installing { .. } => DesktopUpdateState::Unavailable,
        stable => stable,
    };
    DesktopUpdateEnvelope {
        revision: persisted.revision,
        state,
    }
}

fn failed_initial_state() -> DesktopUpdateEnvelope {
    DesktopUpdateEnvelope {
        revision: 0,
        state: DesktopUpdateState::Failed {
            error_kind: DesktopUpdateErrorKind::InvalidMetadata,
        },
    }
}

fn persist_state(path: &Path, envelope: &DesktopUpdateEnvelope) -> Result<(), DesktopUpdateError> {
    let parent = path.parent().ok_or(DesktopUpdateError::InstallFailed)?;
    std::fs::create_dir_all(parent).map_err(|_| DesktopUpdateError::InstallFailed)?;
    let directory_guard =
        open_directory_guard(parent).map_err(|_| DesktopUpdateError::InstallFailed)?;
    let target_guard =
        open_optional_state_file(path).map_err(|_| DesktopUpdateError::InstallFailed)?;
    let mut bytes = serde_json::to_vec(&PersistedDesktopUpdateState {
        schema: DESKTOP_UPDATE_STATE_SCHEMA,
        revision: envelope.revision,
        state: envelope.state.clone(),
    })
    .map_err(|_| DesktopUpdateError::InstallFailed)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_DESKTOP_UPDATE_STATE_BYTES {
        return Err(DesktopUpdateError::InstallFailed);
    }

    // 固定临时槽在失败后可安全复用，不会随重试累积。必须先通过句柄检查，
    // 再经同一句柄截断，避免跟随残留的链接或 reparse point 写入替代目标。
    let temporary = parent.join(DESKTOP_UPDATE_TEMP_FILE);
    let mut file =
        open_temporary_slot(&temporary).map_err(|_| DesktopUpdateError::InstallFailed)?;
    let (initial_identity, _) =
        validated_handle_metadata(&file, false).map_err(|_| DesktopUpdateError::InstallFailed)?;
    file.set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|_| DesktopUpdateError::InstallFailed)?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| DesktopUpdateError::InstallFailed)?;
    let (temporary_identity, temporary_size) =
        validated_handle_metadata(&file, false).map_err(|_| DesktopUpdateError::InstallFailed)?;
    if temporary_identity != initial_identity || temporary_size != bytes.len() as u64 {
        return Err(DesktopUpdateError::InstallFailed);
    }

    let mut temporary_guard = StateFileGuard {
        file,
        identity: temporary_identity,
        size: temporary_size,
    };
    validate_guard_contents(&mut temporary_guard, &bytes)
        .map_err(|_| DesktopUpdateError::InstallFailed)?;

    // 所有可能失败的内容与身份检查均在提交前完成。Windows 临时句柄拒绝
    // DELETE share，并由该句柄直接改名，因此不存在关闭句柄后的 source path-swap。
    validate_directory_guard(&directory_guard, parent)
        .map_err(|_| DesktopUpdateError::InstallFailed)?;
    match target_guard {
        Some(guard) => {
            validate_state_guard(&guard, path).map_err(|_| DesktopUpdateError::InstallFailed)?;
            drop(guard);
        }
        None => {
            if open_optional_state_file(path)
                .map_err(|_| DesktopUpdateError::InstallFailed)?
                .is_some()
            {
                return Err(DesktopUpdateError::InstallFailed);
            }
        }
    }
    commit_temporary_file(temporary_guard.file, &temporary, path)
        .map_err(|_| DesktopUpdateError::InstallFailed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    volume: u64,
    index: u128,
}

#[derive(Debug)]
struct DirectoryGuard {
    file: File,
    identity: FileIdentity,
    canonical: PathBuf,
}

#[derive(Debug)]
struct StateFileGuard {
    file: File,
    identity: FileIdentity,
    size: u64,
}

fn open_directory_guard(path: &Path) -> Result<DirectoryGuard, ()> {
    let file = open_directory_file(path).map_err(|_| ())?;
    let (identity, _) = validated_handle_metadata(&file, true).map_err(|_| ())?;
    let canonical = path.canonicalize().map_err(|_| ())?;
    Ok(DirectoryGuard {
        file,
        identity,
        canonical,
    })
}

fn validate_directory_guard(guard: &DirectoryGuard, path: &Path) -> Result<(), ()> {
    let (held_identity, _) = validated_handle_metadata(&guard.file, true).map_err(|_| ())?;
    let current = open_directory_guard(path)?;
    if held_identity != guard.identity
        || current.identity != guard.identity
        || current.canonical != guard.canonical
    {
        return Err(());
    }
    Ok(())
}

fn open_optional_state_file(path: &Path) -> Result<Option<StateFileGuard>, ()> {
    match open_state_file_io(path) {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

fn open_state_file(path: &Path) -> Result<StateFileGuard, ()> {
    open_state_file_io(path).map_err(|_| ())
}

fn open_state_file_io(path: &Path) -> std::io::Result<StateFileGuard> {
    let file = open_regular_file(path)?;
    let (identity, size) = validated_handle_metadata(&file, false)?;
    Ok(StateFileGuard {
        file,
        identity,
        size,
    })
}

fn validate_state_guard(guard: &StateFileGuard, path: &Path) -> Result<(), ()> {
    let (held_identity, _) = validated_handle_metadata(&guard.file, false).map_err(|_| ())?;
    let current = open_state_file(path)?;
    if held_identity != guard.identity || current.identity != guard.identity {
        return Err(());
    }
    Ok(())
}

fn validate_guard_contents(guard: &mut StateFileGuard, expected_bytes: &[u8]) -> Result<(), ()> {
    if expected_bytes.len() as u64 > MAX_DESKTOP_UPDATE_STATE_BYTES
        || guard.size != expected_bytes.len() as u64
    {
        return Err(());
    }
    guard.file.seek(SeekFrom::Start(0)).map_err(|_| ())?;
    let mut actual = Vec::with_capacity(expected_bytes.len());
    (&mut guard.file)
        .take(MAX_DESKTOP_UPDATE_STATE_BYTES + 1)
        .read_to_end(&mut actual)
        .map_err(|_| ())?;
    if actual != expected_bytes {
        return Err(());
    }
    Ok(())
}

#[cfg(windows)]
fn open_temporary_slot(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ,
    };

    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0 | DELETE.0)
        // 拒绝其他句柄写入、删除或换名，保持被验证 source 的路径身份直到提交。
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH).0)
        .open(path)
}

#[cfg(not(windows))]
fn open_temporary_slot(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
}

#[cfg(windows)]
fn commit_temporary_file(file: File, _source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
    };

    let target_wide = target.as_os_str().encode_wide().collect::<Vec<_>>();
    let name_bytes = target_wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| std::io::Error::other("桌面更新目标路径过长"))?;
    // Win32 缓冲区必须覆盖完整结构体，并在可变文件名之后留出已清零的 NUL 空间。
    // 只按 FileName 字段偏移计算会漏掉结构体尾部对齐，部分过滤驱动会因此偶发拒绝。
    let buffer_bytes = std::mem::size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .ok_or_else(|| std::io::Error::other("桌面更新重命名缓冲区过大"))?;
    let words = buffer_bytes
        .checked_add(std::mem::size_of::<usize>() - 1)
        .ok_or_else(|| std::io::Error::other("桌面更新重命名缓冲区过大"))?
        / std::mem::size_of::<usize>();
    let mut buffer = vec![0_usize; words];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let information_size = u32::try_from(buffer_bytes)
        .map_err(|_| std::io::Error::other("桌面更新重命名缓冲区过大"))?;
    let file_name_length =
        u32::try_from(name_bytes).map_err(|_| std::io::Error::other("桌面更新目标路径过长"))?;

    // SAFETY: 缓冲区按 usize 对齐且容量覆盖 FILE_RENAME_INFO 的可变长 FileName；
    // `file` 在调用期间保持有效，并持有 DELETE 权限。
    let rename = || unsafe {
        std::ptr::write(information, FILE_RENAME_INFO::default());
        // 经典 FileRenameInfo 在 Windows 10/11 上对“目标尚不存在”和“替换已有目标”
        // 都使用同一稳定语义；扩展 POSIX flags 在部分文件系统过滤驱动下会偶发返回
        // ERROR_INVALID_NAME。替换仍由已验证的 source 句柄直接完成，不重新解析 source。
        (*information).Anonymous.ReplaceIfExists = true;
        (*information).RootDirectory = HANDLE::default();
        (*information).FileNameLength = file_name_length;
        std::ptr::copy_nonoverlapping(
            target_wide.as_ptr(),
            (*information).FileName.as_mut_ptr(),
            target_wide.len(),
        );
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfo,
            information.cast(),
            information_size,
        )
        .map_err(std::io::Error::other)
    };
    let mut last_error = None;
    for attempt in 0..5 {
        match rename() {
            Ok(()) => {
                // 提交已经完成；后续刷新只能尽力而为，绝不能把成功提交报告成失败并造成
                // 内存 revision 落后于磁盘 revision。
                let _ = file.sync_all();
                return Ok(());
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < 4 {
                    // 防病毒/索引器可能短暂持有旧目标。只重试同一个已验证句柄，
                    // 不重新解析 source path，因此不会重新引入 path-swap 窗口。
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("桌面更新原子替换失败")))
}

#[cfg(not(windows))]
fn commit_temporary_file(file: File, source: &Path, target: &Path) -> std::io::Result<()> {
    drop(file);
    crate::runtime::atomic_file::replace_file(source, target)
}

fn read_state_bounded(guard: &mut StateFileGuard) -> Result<Vec<u8>, ()> {
    if guard.size > MAX_DESKTOP_UPDATE_STATE_BYTES {
        return Err(());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(guard.size).unwrap_or(0));
    (&mut guard.file)
        .take(MAX_DESKTOP_UPDATE_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_DESKTOP_UPDATE_STATE_BYTES {
        return Err(());
    }
    Ok(bytes)
}

#[cfg(windows)]
fn open_directory_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
}

#[cfg(not(windows))]
fn open_directory_file(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(windows)]
fn open_regular_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

#[cfg(not(windows))]
fn open_regular_file(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(windows)]
fn validated_handle_metadata(file: &File, directory: bool) -> std::io::Result<(FileIdentity, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` 在调用期间保持打开，输出结构体已按 Win32 要求初始化。
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(std::io::Error::other)?;
    let attributes = information.dwFileAttributes;
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if is_directory != directory
        || attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || (!directory && information.nNumberOfLinks != 1)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "桌面更新状态路径安全属性无效",
        ));
    }
    Ok((
        FileIdentity {
            volume: u64::from(information.dwVolumeSerialNumber),
            index: (u128::from(information.nFileIndexHigh) << 32)
                | u128::from(information.nFileIndexLow),
        },
        (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow),
    ))
}

#[cfg(not(windows))]
fn validated_handle_metadata(file: &File, directory: bool) -> std::io::Result<(FileIdentity, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if metadata.is_dir() != directory
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "桌面更新状态路径安全属性无效",
        ));
    }
    Ok((
        FileIdentity {
            volume: metadata.dev(),
            index: u128::from(metadata.ino()),
        },
        metadata.len(),
    ))
}
