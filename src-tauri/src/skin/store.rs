use super::model::{SkinDraft, SkinSettings, SkinStateEnvelope};
use crate::runtime::atomic_file::replace_file;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const SCHEMA_VERSION: u8 = 1;
const MAX_BLUR_PX: u8 = 32;
const MAX_MASK_OPACITY_PERCENT: u8 = 80;
const MIN_PANEL_OPACITY_PERCENT: u8 = 55;
const MAX_PANEL_OPACITY_PERCENT: u8 = 100;
const DIGEST_LENGTH: usize = 64;
const REGISTERED_EXTENSIONS: [&str; 3] = ["png", "jpg", "webp"];
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

/// 皮肤设置错误的稳定分类。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinErrorKind {
    InvalidSettings,
    ImageNotRegistered,
    RevisionConflict,
    RevisionExhausted,
    CorruptSettings,
    FileSystem,
}

/// 不泄露本地路径或图片名称的皮肤设置错误。
#[derive(Debug, Error)]
pub enum SkinError {
    #[error("皮肤设置超出允许范围")]
    InvalidSettings,
    #[error("皮肤图片尚未登记")]
    ImageNotRegistered,
    #[error("皮肤设置已被其他操作更新")]
    RevisionConflict,
    #[error("皮肤设置 revision 已耗尽")]
    RevisionExhausted,
    #[error("皮肤设置文件无效")]
    CorruptSettings,
    #[error("无法访问皮肤设置")]
    FileSystem,
}

impl SkinError {
    /// 返回可安全提供给 UI 和诊断系统的稳定错误分类。
    ///
    /// :return: 不包含动态文件系统上下文的错误分类。
    /// :raises: 枚举值已封闭，此函数不产生错误。
    pub fn kind(&self) -> SkinErrorKind {
        match self {
            Self::InvalidSettings => SkinErrorKind::InvalidSettings,
            Self::ImageNotRegistered => SkinErrorKind::ImageNotRegistered,
            Self::RevisionConflict => SkinErrorKind::RevisionConflict,
            Self::RevisionExhausted => SkinErrorKind::RevisionExhausted,
            Self::CorruptSettings => SkinErrorKind::CorruptSettings,
            Self::FileSystem => SkinErrorKind::FileSystem,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedSkinState {
    schema: u8,
    revision: u64,
    settings: SkinSettings,
}

#[derive(Debug)]
struct LoadedSkinState {
    envelope: SkinStateEnvelope,
    target_guard: Option<SettingsFileGuard>,
}

/// 串行化 revision 更新并原子持久化的皮肤设置仓库。
#[derive(Clone, Debug)]
pub struct SkinStore {
    settings_root: PathBuf,
    skins_root: PathBuf,
    operation: Arc<Mutex<()>>,
}

impl SkinStore {
    /// 创建绑定到固定设置目录和皮肤托管目录的仓库。
    ///
    /// :param settings_root: `skin.json` 所在的预定义漫游设置目录。
    /// :param skins_root: 只包含按摘要命名图片的预定义本地皮肤目录。
    /// :return: 尚未访问文件系统的仓库实例。
    /// :raises: 此构造函数只保存路径，不产生错误。
    pub fn new(settings_root: PathBuf, skins_root: PathBuf) -> Self {
        Self {
            settings_root,
            skins_root,
            operation: Arc::new(Mutex::new(())),
        }
    }

    /// 读取严格 schema 的当前皮肤快照；文件不存在时返回 revision 0 的默认值。
    ///
    /// :return: 当前 revision 与已验证设置。
    /// :raises SkinError: 目录身份异常、JSON 损坏、schema 不兼容或设置越界时返回稳定错误。
    pub fn load(&self) -> Result<SkinStateEnvelope, SkinError> {
        let _guard = self.operation.lock().map_err(|_| SkinError::FileSystem)?;
        let directory_guard = open_directory_guard(&self.settings_root)?;
        Ok(self.load_guarded(&directory_guard)?.envelope)
    }

    /// 在 revision 匹配时保存经严格校验的皮肤草稿。
    ///
    /// :param expected_revision: 调用方最后观察到的 revision。
    /// :param draft: 设置窗口提交的完整类型化草稿。
    /// :return: revision 加一后的持久化快照。
    /// :raises SkinError: revision 过期、图片未登记、设置越界或原子写入失败时返回稳定错误。
    pub fn save(
        &self,
        expected_revision: u64,
        draft: SkinDraft,
    ) -> Result<SkinStateEnvelope, SkinError> {
        let _guard = self.operation.lock().map_err(|_| SkinError::FileSystem)?;
        let directory_guard = open_directory_guard(&self.settings_root)?;
        let current = self.load_guarded(&directory_guard)?;
        if current.envelope.revision != expected_revision {
            return Err(SkinError::RevisionConflict);
        }
        self.validate_draft(&draft)?;
        let revision = current
            .envelope
            .revision
            .checked_add(1)
            .ok_or(SkinError::RevisionExhausted)?;
        let next = SkinStateEnvelope {
            revision,
            settings: draft.into(),
        };
        self.persist(&directory_guard, current.target_guard, &next)?;
        Ok(next)
    }

    /// 在 revision 匹配时恢复默认视觉设置，不删除任何已导入图片。
    ///
    /// :param expected_revision: 调用方最后观察到的 revision。
    /// :return: revision 加一、指向默认设置的新快照。
    /// :raises SkinError: revision 过期或原子写入失败时返回稳定错误。
    pub fn reset(&self, expected_revision: u64) -> Result<SkinStateEnvelope, SkinError> {
        let defaults = SkinSettings::default();
        self.save(
            expected_revision,
            SkinDraft {
                immersive: defaults.immersive,
                image_digest: defaults.image_digest,
                fit: defaults.fit,
                position: defaults.position,
                blur_px: defaults.blur_px,
                mask_tone: defaults.mask_tone,
                mask_opacity_percent: defaults.mask_opacity_percent,
                panel_opacity_percent: defaults.panel_opacity_percent,
            },
        )
    }

    fn settings_path(&self) -> PathBuf {
        self.settings_root.join("skin.json")
    }

    fn load_guarded(&self, directory_guard: &DirectoryGuard) -> Result<LoadedSkinState, SkinError> {
        validate_directory_guard(directory_guard, &self.settings_root)?;
        let path = self.settings_path();
        let mut target_guard = match open_optional_settings_file(&path)? {
            Some(guard) => guard,
            None => {
                return Ok(LoadedSkinState {
                    envelope: SkinStateEnvelope::default(),
                    target_guard: None,
                });
            }
        };
        let bytes = read_settings_bounded(&mut target_guard)?;
        let persisted: PersistedSkinState =
            serde_json::from_slice(&bytes).map_err(|_| SkinError::CorruptSettings)?;
        if persisted.schema != SCHEMA_VERSION {
            return Err(SkinError::CorruptSettings);
        }
        let envelope = SkinStateEnvelope {
            revision: persisted.revision,
            settings: persisted.settings,
        };
        self.validate_loaded_settings(&envelope.settings)?;
        Ok(LoadedSkinState {
            envelope,
            target_guard: Some(target_guard),
        })
    }

    fn validate_loaded_settings(&self, settings: &SkinSettings) -> Result<(), SkinError> {
        match self.validate_settings(settings) {
            Ok(()) => Ok(()),
            Err(SkinError::InvalidSettings | SkinError::ImageNotRegistered) => {
                Err(SkinError::CorruptSettings)
            }
            Err(error) => Err(error),
        }
    }

    fn validate_settings(&self, settings: &SkinSettings) -> Result<(), SkinError> {
        self.validate_fields(
            settings.immersive,
            settings.image_digest.as_deref(),
            settings.blur_px,
            settings.mask_opacity_percent,
            settings.panel_opacity_percent,
        )
    }

    fn validate_draft(&self, draft: &SkinDraft) -> Result<(), SkinError> {
        self.validate_fields(
            draft.immersive,
            draft.image_digest.as_deref(),
            draft.blur_px,
            draft.mask_opacity_percent,
            draft.panel_opacity_percent,
        )
    }

    fn validate_fields(
        &self,
        immersive: bool,
        image_digest: Option<&str>,
        blur_px: u8,
        mask_opacity_percent: u8,
        panel_opacity_percent: u8,
    ) -> Result<(), SkinError> {
        if blur_px > MAX_BLUR_PX
            || mask_opacity_percent > MAX_MASK_OPACITY_PERCENT
            || !(MIN_PANEL_OPACITY_PERCENT..=MAX_PANEL_OPACITY_PERCENT)
                .contains(&panel_opacity_percent)
            || image_digest.is_some_and(|digest| !is_canonical_digest(digest))
        {
            return Err(SkinError::InvalidSettings);
        }
        if immersive {
            let digest = image_digest.ok_or(SkinError::ImageNotRegistered)?;
            if !self.is_registered(digest)? {
                return Err(SkinError::ImageNotRegistered);
            }
        }
        Ok(())
    }

    fn is_registered(&self, digest: &str) -> Result<bool, SkinError> {
        let _skins_guard = open_directory_guard(&self.skins_root)?;
        for extension in REGISTERED_EXTENSIONS {
            let candidate = self.skins_root.join(format!("{digest}.{extension}"));
            match open_regular_file(&candidate) {
                Ok(file) => {
                    validated_handle_metadata(&file, false).map_err(|_| SkinError::FileSystem)?;
                    return Ok(true);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(SkinError::FileSystem),
            }
        }
        Ok(false)
    }

    fn persist(
        &self,
        directory_guard: &DirectoryGuard,
        target_guard: Option<SettingsFileGuard>,
        envelope: &SkinStateEnvelope,
    ) -> Result<(), SkinError> {
        validate_directory_guard(directory_guard, &self.settings_root)?;
        let destination = self.settings_path();
        let persisted = PersistedSkinState {
            schema: SCHEMA_VERSION,
            revision: envelope.revision,
            settings: envelope.settings.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&persisted).map_err(|_| SkinError::FileSystem)?;
        bytes.push(b'\n');
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SkinError::FileSystem)?
            .as_nanos();
        let temporary = self
            .settings_root
            .join(format!(".skin-{}-{nonce}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| SkinError::FileSystem)?;
        file.write_all(&bytes).map_err(|_| SkinError::FileSystem)?;
        file.sync_all().map_err(|_| SkinError::FileSystem)?;
        let (temporary_identity, temporary_size) =
            validated_handle_metadata(&file, false).map_err(|_| SkinError::FileSystem)?;
        if temporary_size != bytes.len() as u64 {
            return Err(SkinError::FileSystem);
        }
        drop(file);

        // 根目录 guard 在整个事务中拒绝 DELETE share，确保临时文件与目标始终处于
        // 同一个已验证目录。已有目标也保持 guard，直到原子替换前最后一次身份复核。
        validate_directory_guard(directory_guard, &self.settings_root)?;
        let temporary_guard =
            validate_temporary_file(&temporary, temporary_identity, temporary_size, &bytes)?;
        match target_guard {
            Some(guard) => {
                validate_settings_guard(&guard, &destination)?;
                // Windows 替换目标要求释放拒绝 DELETE share 的目标句柄；释放后立即执行
                // 单一 replace，不再进行可能扩大竞态窗口的路径操作。
                drop(guard);
            }
            None => {
                if open_optional_settings_file(&destination)?.is_some() {
                    return Err(SkinError::FileSystem);
                }
            }
        }
        // MoveFileExW 需要源文件允许 DELETE share；复核句柄释放后不再执行其他路径操作。
        drop(temporary_guard);
        replace_file(&temporary, &destination).map_err(|_| SkinError::FileSystem)?;
        let mut committed = open_settings_file(&destination)?;
        if committed.identity != temporary_identity || committed.size != temporary_size {
            return Err(SkinError::FileSystem);
        }
        validate_guard_contents(&mut committed, &bytes)?;
        Ok(())
    }
}

fn is_canonical_digest(value: &str) -> bool {
    value.len() == DIGEST_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
struct SettingsFileGuard {
    file: File,
    identity: FileIdentity,
    size: u64,
}

fn open_directory_guard(path: &Path) -> Result<DirectoryGuard, SkinError> {
    let file = open_directory_file(path).map_err(|_| SkinError::FileSystem)?;
    let (identity, _) =
        validated_handle_metadata(&file, true).map_err(|_| SkinError::FileSystem)?;
    let canonical = path.canonicalize().map_err(|_| SkinError::FileSystem)?;
    Ok(DirectoryGuard {
        file,
        identity,
        canonical,
    })
}

fn validate_directory_guard(guard: &DirectoryGuard, path: &Path) -> Result<(), SkinError> {
    let (held_identity, _) =
        validated_handle_metadata(&guard.file, true).map_err(|_| SkinError::FileSystem)?;
    let current = open_directory_guard(path)?;
    if held_identity != guard.identity
        || current.identity != guard.identity
        || current.canonical != guard.canonical
    {
        return Err(SkinError::FileSystem);
    }
    Ok(())
}

fn open_optional_settings_file(path: &Path) -> Result<Option<SettingsFileGuard>, SkinError> {
    match open_settings_file_io(path) {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(SkinError::FileSystem),
    }
}

fn open_settings_file(path: &Path) -> Result<SettingsFileGuard, SkinError> {
    open_settings_file_io(path).map_err(|_| SkinError::FileSystem)
}

fn open_settings_file_io(path: &Path) -> std::io::Result<SettingsFileGuard> {
    let file = open_regular_file(path)?;
    let (identity, size) = validated_handle_metadata(&file, false)?;
    Ok(SettingsFileGuard {
        file,
        identity,
        size,
    })
}

fn validate_settings_guard(guard: &SettingsFileGuard, path: &Path) -> Result<(), SkinError> {
    let (held_identity, _) =
        validated_handle_metadata(&guard.file, false).map_err(|_| SkinError::FileSystem)?;
    let current = open_settings_file(path)?;
    if held_identity != guard.identity || current.identity != guard.identity {
        return Err(SkinError::FileSystem);
    }
    Ok(())
}

fn validate_temporary_file(
    path: &Path,
    expected_identity: FileIdentity,
    expected_size: u64,
    expected_bytes: &[u8],
) -> Result<SettingsFileGuard, SkinError> {
    let mut current = open_settings_file(path)?;
    if current.identity != expected_identity || current.size != expected_size {
        return Err(SkinError::FileSystem);
    }
    validate_guard_contents(&mut current, expected_bytes)?;
    Ok(current)
}

fn validate_guard_contents(
    guard: &mut SettingsFileGuard,
    expected_bytes: &[u8],
) -> Result<(), SkinError> {
    if expected_bytes.len() as u64 > MAX_SETTINGS_BYTES || guard.size != expected_bytes.len() as u64
    {
        return Err(SkinError::FileSystem);
    }
    guard
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|_| SkinError::FileSystem)?;
    let mut actual = Vec::with_capacity(expected_bytes.len());
    (&mut guard.file)
        .take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut actual)
        .map_err(|_| SkinError::FileSystem)?;
    if actual != expected_bytes {
        return Err(SkinError::FileSystem);
    }
    Ok(())
}

fn read_settings_bounded(guard: &mut SettingsFileGuard) -> Result<Vec<u8>, SkinError> {
    if guard.size > MAX_SETTINGS_BYTES {
        return Err(SkinError::CorruptSettings);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(guard.size).unwrap_or(0));
    (&mut guard.file)
        .take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SkinError::FileSystem)?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(SkinError::CorruptSettings);
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
            "皮肤设置路径安全属性无效",
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
            "皮肤设置路径安全属性无效",
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

#[cfg(all(test, windows))]
mod tests {
    use super::{open_directory_guard, open_settings_file, validate_temporary_file};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_root(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dsh-desktop-skin-guard-{}-{name}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("应创建隔离测试目录");
        root
    }

    #[test]
    fn settings_directory_guard_denies_replacement_until_commit_finishes() {
        let root = test_root("directory");
        let guarded = root.join("settings");
        let replacement = root.join("replacement");
        fs::create_dir(&guarded).expect("应创建设置目录");
        let guard = open_directory_guard(&guarded).expect("应持有目录 guard");

        assert!(fs::rename(&guarded, &replacement).is_err());
        drop(guard);
        fs::rename(&guarded, &replacement).expect("释放 guard 后应允许移动目录");
    }

    #[test]
    fn settings_target_guard_denies_replacement_until_commit_finishes() {
        let root = test_root("target");
        let guarded = root.join("skin.json");
        let replacement = root.join("replacement.json");
        fs::write(&guarded, b"{}").expect("应创建设置文件");
        let guard = open_settings_file(&guarded).expect("应持有目标 guard");

        assert!(fs::rename(&guarded, &replacement).is_err());
        drop(guard);
        fs::rename(&guarded, &replacement).expect("释放 guard 后应允许移动目标");
    }

    #[test]
    fn temporary_identity_check_rejects_same_length_path_replacement() {
        let root = test_root("temporary-replacement");
        let temporary = root.join(".skin-test.tmp");
        let original = root.join("original.tmp");
        fs::write(&temporary, b"trusted-settings").expect("应创建可信临时文件");
        let created = open_settings_file(&temporary).expect("应记录创建身份");
        let identity = created.identity;
        let size = created.size;
        drop(created);

        fs::rename(&temporary, &original).expect("应保留原临时文件夹具");
        fs::write(&temporary, b"hostile-settings").expect("应写入同长度替换文件");

        assert!(
            validate_temporary_file(&temporary, identity, size, b"trusted-settings").is_err(),
            "路径被同长度文件交换后必须失败关闭"
        );
    }

    #[test]
    fn temporary_content_check_rejects_same_inode_same_length_rewrite() {
        let root = test_root("temporary-rewrite");
        let temporary = root.join(".skin-test.tmp");
        let expected = b"trusted-settings";
        fs::write(&temporary, expected).expect("应创建可信临时文件");
        let created = open_settings_file(&temporary).expect("应记录创建身份");
        let identity = created.identity;
        let size = created.size;
        drop(created);

        fs::write(&temporary, b"hostile-settings").expect("应原地写入同长度内容");

        assert!(
            validate_temporary_file(&temporary, identity, size, expected).is_err(),
            "同一 inode 的等长内容改写必须失败关闭"
        );
    }
}
