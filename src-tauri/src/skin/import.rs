use super::model::{SkinFormat, SkinImage};
use super::store::SkinError;
use image::{ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAX_SKIN_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_SKIN_EDGE: u32 = 7680;
pub const MAX_SKIN_PIXELS: u64 = 7680 * 4320;

/// 将用户选中的图片验证后复制到固定托管目录。
#[derive(Clone, Debug)]
pub struct SkinImporter {
    root: PathBuf,
}

impl SkinImporter {
    /// 创建绑定到预定义皮肤目录的导入器。
    ///
    /// :param root: `AppPaths.skins` 提供的固定托管目录。
    /// :return: 尚未读取任何图片的导入器。
    /// :raises: 构造过程只保存路径，不产生错误。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 在阻塞线程中验证并不可变地导入一张本地图片。
    ///
    /// :param source: 原生选择器返回的用户图片路径。
    /// :return: 只引用托管副本的图片元数据。
    /// :raises SkinError: 文件超限、格式不支持、解码失败、尺寸越界或路径身份异常时返回稳定错误。
    pub async fn import(&self, source: PathBuf) -> Result<SkinImage, SkinError> {
        let root = self.root.clone();
        tauri::async_runtime::spawn_blocking(move || import_blocking(&root, &source))
            .await
            .map_err(|_| SkinError::Worker)?
    }
}

fn import_blocking(root: &Path, source: &Path) -> Result<SkinImage, SkinError> {
    // 目录 guard 拒绝 DELETE share，确保摘要路径始终落在最初验证的托管目录。
    let root_guard = open_directory_guard(root)?;
    let mut source_guard = open_regular_guard(source)?;
    if source_guard.size > MAX_SKIN_BYTES {
        return Err(SkinError::TooLarge);
    }
    let bytes = read_bounded(&mut source_guard.file, MAX_SKIN_BYTES)?;
    if bytes.len() as u64 > MAX_SKIN_BYTES {
        return Err(SkinError::TooLarge);
    }
    validate_regular_guard(&source_guard, source)?;
    if bytes.len() as u64 != source_guard.size {
        return Err(SkinError::FileSystem);
    }

    let (format, image_format) = detect_format(&bytes)?;
    let (width, height) = dimensions(&bytes, image_format)?;
    validate_dimensions(width, height)?;
    decode_fully(&bytes, image_format, width, height)?;

    let digest = format!("{:x}", Sha256::digest(&bytes));
    let destination = root.join(format!("{digest}.{}", format.extension()));
    validate_directory_guard(&root_guard, root)?;
    persist_immutable(&destination, &bytes)?;
    validate_directory_guard(&root_guard, root)?;

    Ok(SkinImage {
        digest,
        format,
        width,
        height,
        byte_size: bytes.len() as u64,
        path: destination,
    })
}

fn detect_format(bytes: &[u8]) -> Result<(SkinFormat, ImageFormat), SkinError> {
    match image::guess_format(bytes).map_err(|_| SkinError::UnsupportedFormat)? {
        ImageFormat::Png => Ok((SkinFormat::Png, ImageFormat::Png)),
        ImageFormat::Jpeg => Ok((SkinFormat::Jpeg, ImageFormat::Jpeg)),
        ImageFormat::WebP => Ok((SkinFormat::Webp, ImageFormat::WebP)),
        _ => Err(SkinError::UnsupportedFormat),
    }
}

fn dimensions(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32), SkinError> {
    ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| SkinError::Decode)
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), SkinError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(SkinError::Dimensions)?;
    if width == 0
        || height == 0
        || width > MAX_SKIN_EDGE
        || height > MAX_SKIN_EDGE
        || pixels > MAX_SKIN_PIXELS
    {
        return Err(SkinError::Dimensions);
    }
    Ok(())
}

fn decode_fully(
    bytes: &[u8],
    format: ImageFormat,
    expected_width: u32,
    expected_height: u32,
) -> Result<(), SkinError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SKIN_EDGE);
    limits.max_image_height = Some(MAX_SKIN_EDGE);
    // 允许最坏的 RGBA16 输出并预留输入缓冲，同时仍将单次解码控制在固定上界内。
    limits.max_alloc = Some(
        MAX_SKIN_PIXELS
            .saturating_mul(8)
            .saturating_add(MAX_SKIN_BYTES),
    );
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| SkinError::Decode)?;
    if decoded.width() != expected_width || decoded.height() != expected_height {
        return Err(SkinError::Decode);
    }
    Ok(())
}

fn persist_immutable(destination: &Path, expected: &[u8]) -> Result<(), SkinError> {
    match create_new_regular(destination) {
        Ok(mut created) => {
            created
                .file
                .write_all(expected)
                .map_err(|_| SkinError::FileSystem)?;
            created.file.sync_all().map_err(|_| SkinError::FileSystem)?;
            let (identity, size) = validated_handle_metadata(&created.file, false)?;
            if identity != created.identity || size != expected.len() as u64 {
                return Err(SkinError::FileSystem);
            }
            created.size = size;
            validate_file_contents(&mut created, expected)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = open_regular_guard(destination)?;
            validate_file_contents(&mut existing, expected)?;
            validate_regular_guard(&existing, destination)
        }
        Err(_) => Err(SkinError::FileSystem),
    }
}

fn validate_file_contents(guard: &mut RegularFileGuard, expected: &[u8]) -> Result<(), SkinError> {
    if guard.size != expected.len() as u64 || guard.size > MAX_SKIN_BYTES {
        return Err(SkinError::FileSystem);
    }
    guard
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|_| SkinError::FileSystem)?;
    let actual = read_bounded(&mut guard.file, MAX_SKIN_BYTES)?;
    if actual != expected
        || format!("{:x}", Sha256::digest(&actual)) != format!("{:x}", Sha256::digest(expected))
    {
        return Err(SkinError::FileSystem);
    }
    Ok(())
}

fn read_bounded(file: &mut File, limit: u64) -> Result<Vec<u8>, SkinError> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SkinError::FileSystem)?;
    Ok(bytes)
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
}

#[derive(Debug)]
struct RegularFileGuard {
    file: File,
    identity: FileIdentity,
    size: u64,
}

fn open_directory_guard(path: &Path) -> Result<DirectoryGuard, SkinError> {
    let file = open_directory_file(path).map_err(|_| SkinError::FileSystem)?;
    let (identity, _) = validated_handle_metadata(&file, true)?;
    Ok(DirectoryGuard { file, identity })
}

fn validate_directory_guard(guard: &DirectoryGuard, path: &Path) -> Result<(), SkinError> {
    let (held_identity, _) = validated_handle_metadata(&guard.file, true)?;
    let current = open_directory_guard(path)?;
    if held_identity != guard.identity || current.identity != guard.identity {
        return Err(SkinError::FileSystem);
    }
    Ok(())
}

fn open_regular_guard(path: &Path) -> Result<RegularFileGuard, SkinError> {
    open_regular_guard_io(path).map_err(|_| SkinError::FileSystem)
}

fn open_regular_guard_io(path: &Path) -> std::io::Result<RegularFileGuard> {
    let file = open_regular_file(path)?;
    let (identity, size) = validated_handle_metadata_io(&file, false)?;
    Ok(RegularFileGuard {
        file,
        identity,
        size,
    })
}

fn create_new_regular(path: &Path) -> std::io::Result<RegularFileGuard> {
    let file = create_new_regular_file(path)?;
    let (identity, size) = validated_handle_metadata_io(&file, false)?;
    Ok(RegularFileGuard {
        file,
        identity,
        size,
    })
}

fn validate_regular_guard(guard: &RegularFileGuard, path: &Path) -> Result<(), SkinError> {
    let (held_identity, held_size) = validated_handle_metadata(&guard.file, false)?;
    let current = open_regular_guard(path)?;
    if held_identity != guard.identity
        || held_size != guard.size
        || current.identity != guard.identity
        || current.size != guard.size
    {
        return Err(SkinError::FileSystem);
    }
    Ok(())
}

fn validated_handle_metadata(
    file: &File,
    directory: bool,
) -> Result<(FileIdentity, u64), SkinError> {
    validated_handle_metadata_io(file, directory).map_err(|_| SkinError::FileSystem)
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
fn create_new_regular_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

#[cfg(not(windows))]
fn create_new_regular_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
}

#[cfg(windows)]
fn validated_handle_metadata_io(
    file: &File,
    directory: bool,
) -> std::io::Result<(FileIdentity, u64)> {
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
            "皮肤图片路径安全属性无效",
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
fn validated_handle_metadata_io(
    file: &File,
    directory: bool,
) -> std::io::Result<(FileIdentity, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if metadata.is_dir() != directory
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "皮肤图片路径安全属性无效",
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
