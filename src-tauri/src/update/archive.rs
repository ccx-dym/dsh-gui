use crate::paths::RuntimeLayout;
use crate::runtime::install_state::InstalledRuntime;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const INVENTORY_FILE: &str = "inventory.json";

/// 解压阶段的资源上限；同时约束 ZIP 元数据与实际输出流。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveInstallPolicy {
    pub max_files: usize,
    pub max_unpacked_bytes: u64,
}

impl Default for ArchiveInstallPolicy {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_unpacked_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

/// 一次已验证压缩包的不可变安装请求。
#[derive(Clone, Debug)]
pub struct ArchiveInstallRequest {
    pub archive_path: PathBuf,
    pub layout: RuntimeLayout,
    pub runtime: InstalledRuntime,
    pub node_version: Version,
    pub trace_id: String,
}

/// 已通过内容闭包校验并原子封存的运行时目录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledRuntimeArchive {
    pub runtime: InstalledRuntime,
    pub runtime_dir: PathBuf,
    pub file_count: usize,
    pub unpacked_bytes: u64,
}

/// 安全解压、payload 校验或不可变目录封存失败。
#[derive(Debug, Error)]
pub enum ArchiveInstallError {
    #[error("解压策略字段无效: {field}")]
    InvalidPolicy { field: &'static str },
    #[error("安装 trace_id 无效")]
    InvalidTraceId,
    #[error("运行时压缩包不是有效 ZIP")]
    InvalidArchive,
    #[error("运行时压缩包包含不安全路径")]
    UnsafeEntry,
    #[error("运行时压缩包包含不支持的条目类型")]
    UnsupportedEntry,
    #[error("运行时压缩包包含重复文件")]
    DuplicateEntry,
    #[error("运行时压缩包包含 Windows 大小写碰撞")]
    CaseCollision,
    #[error("运行时压缩包包含文件与目录前缀冲突")]
    PathConflict,
    #[error("运行时压缩包文件数量超过上限")]
    FileCountLimit,
    #[error("运行时压缩包展开大小超过上限")]
    UnpackedSizeLimit,
    #[error("运行时目标版本已经存在")]
    TargetAlreadyExists,
    #[error("运行时 staging 已经存在")]
    StagingAlreadyExists,
    #[error("运行时根目录或 staging 边界不安全")]
    UnsafeFilesystemBoundary,
    #[error("运行时缺少必要组件: {component}")]
    RequiredPayloadMissing { component: &'static str },
    #[error("DSH package name/version 与兼容清单不一致")]
    PackageMismatch,
    #[error("运行时 inventory.json 结构无效")]
    InvalidInventory,
    #[error("运行时 payload 与 inventory.json 不一致")]
    InventoryMismatch,
    #[error("运行时安装 I/O 失败（{operation}）")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("运行时解压 worker 异常终止")]
    Worker,
}

/// 将签名下载产物安全安装到版本化 runtime 目录。
#[derive(Clone, Debug)]
pub struct RuntimeArchiveInstaller {
    policy: ArchiveInstallPolicy,
}

impl RuntimeArchiveInstaller {
    /// 创建具有明确文件数和展开大小上限的安装器。
    ///
    /// :param policy: 解压前元数据扫描和解压流共同使用的资源上限。
    /// :return: 可在 Tokio blocking worker 中执行安装的对象。
    /// :raises ArchiveInstallError: 任一上限为零时返回 `InvalidPolicy`。
    pub fn new(policy: ArchiveInstallPolicy) -> Result<Self, ArchiveInstallError> {
        if policy.max_files == 0 {
            return Err(ArchiveInstallError::InvalidPolicy { field: "max_files" });
        }
        if policy.max_unpacked_bytes == 0 {
            return Err(ArchiveInstallError::InvalidPolicy {
                field: "max_unpacked_bytes",
            });
        }
        Ok(Self { policy })
    }

    /// 解压、核对 inventory 与官方入口，再原子封存不可变 runtime。
    ///
    /// 压缩和磁盘操作全部移入 blocking worker，避免阻塞 Tauri/Tokio 事件循环。失败时
    /// staging 会原样保留给后续结构化诊断；源 `.verified` 文件也始终保留。
    ///
    /// :param request: 已验证压缩包、固定版本布局、Node/DSH 版本和安全 trace 标识。
    /// :return: rename 成功后实际不可变目录及 inventory 统计。
    /// :raises ArchiveInstallError: ZIP 路径/类型/资源上限、payload 闭包、版本、文件系统
    ///   边界或原子 rename 不符合约束时返回稳定错误类别。
    pub async fn install(
        &self,
        request: ArchiveInstallRequest,
    ) -> Result<InstalledRuntimeArchive, ArchiveInstallError> {
        let policy = self.policy;
        tokio::task::spawn_blocking(move || install_blocking(request, policy))
            .await
            .map_err(|_| ArchiveInstallError::Worker)?
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug)]
struct RegisteredPath {
    normalized: String,
    kind: EntryKind,
    explicit: bool,
}

#[derive(Clone, Debug)]
struct ScannedEntry {
    index: usize,
    relative_path: PathBuf,
    normalized: String,
    kind: EntryKind,
    declared_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileRecord {
    size: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryEntry {
    path: String,
    size: u64,
    sha256: String,
}

fn install_blocking(
    request: ArchiveInstallRequest,
    policy: ArchiveInstallPolicy,
) -> Result<InstalledRuntimeArchive, ArchiveInstallError> {
    validate_trace_id(&request.trace_id)?;
    validate_source_archive(&request.archive_path)?;

    let requested_target = request.layout.runtime_dir(&request.runtime);
    let runtime_root = requested_target
        .parent()
        .ok_or(ArchiveInstallError::UnsafeFilesystemBoundary)?;
    fs::create_dir_all(runtime_root).map_err(|source| io_error("create_runtime_root", source))?;
    reject_reparse(runtime_root)?;
    let canonical_root = fs::canonicalize(runtime_root)
        .map_err(|source| io_error("canonicalize_runtime_root", source))?;
    if requested_target.exists() || fs::symlink_metadata(&requested_target).is_ok() {
        return Err(ArchiveInstallError::TargetAlreadyExists);
    }

    let target = canonical_root.join(request.runtime.version.to_string());
    let staging = canonical_root.join(format!(".staging-{}", request.trace_id));
    if fs::symlink_metadata(&staging).is_ok() {
        return Err(ArchiveInstallError::StagingAlreadyExists);
    }
    fs::create_dir(&staging).map_err(|source| io_error("create_staging", source))?;
    reject_reparse(&staging)?;
    let canonical_staging =
        fs::canonicalize(&staging).map_err(|source| io_error("canonicalize_staging", source))?;
    if canonical_staging.parent() != Some(canonical_root.as_path()) {
        return Err(ArchiveInstallError::UnsafeFilesystemBoundary);
    }

    let reported_entries = reported_zip_entry_count(&request.archive_path)?;
    let archive_file = File::open(&request.archive_path)
        .map_err(|source| io_error("open_verified_archive", source))?;
    let mut archive =
        zip::ZipArchive::new(archive_file).map_err(|_| ArchiveInstallError::InvalidArchive)?;
    // zip crate 按名称存储中央目录，重复名称会覆盖旧项；EOCD 计数是发现该别名的独立边界。
    if reported_entries != archive.len() {
        return Err(ArchiveInstallError::DuplicateEntry);
    }
    let scanned = scan_archive(&mut archive, policy)?;
    let actual = extract_archive(&mut archive, &scanned, &canonical_staging, policy)?;
    validate_payload(
        &canonical_staging,
        &actual,
        &request.runtime.version,
        &request.node_version,
    )?;

    // 目标与 staging 位于同一已规范化父目录，rename 因而是最终单步可见性边界。
    if fs::symlink_metadata(&target).is_ok() {
        return Err(ArchiveInstallError::TargetAlreadyExists);
    }
    fs::rename(&canonical_staging, &target).map_err(|source| io_error("seal_runtime", source))?;
    Ok(InstalledRuntimeArchive {
        runtime: request.runtime,
        runtime_dir: target,
        file_count: actual.len(),
        unpacked_bytes: actual.values().map(|record| record.size).sum(),
    })
}

fn scan_archive(
    archive: &mut zip::ZipArchive<File>,
    policy: ArchiveInstallPolicy,
) -> Result<Vec<ScannedEntry>, ArchiveInstallError> {
    let mut registry = BTreeMap::<String, RegisteredPath>::new();
    let mut scanned = Vec::with_capacity(archive.len());
    let mut file_count = 0_usize;
    let mut unpacked_bytes = 0_u64;

    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| ArchiveInstallError::InvalidArchive)?;
        if entry.is_symlink() {
            return Err(ArchiveInstallError::UnsupportedEntry);
        }
        let kind = if entry.is_dir() {
            EntryKind::Directory
        } else if entry.is_file() {
            EntryKind::File
        } else {
            return Err(ArchiveInstallError::UnsupportedEntry);
        };
        let (relative_path, normalized, segments) = normalize_entry(entry.name(), kind)?;
        register_path(&mut registry, &normalized, &segments, kind)?;
        let declared_size = entry.size();
        if kind == EntryKind::File {
            file_count = file_count
                .checked_add(1)
                .ok_or(ArchiveInstallError::FileCountLimit)?;
            if file_count > policy.max_files {
                return Err(ArchiveInstallError::FileCountLimit);
            }
            unpacked_bytes = unpacked_bytes
                .checked_add(declared_size)
                .ok_or(ArchiveInstallError::UnpackedSizeLimit)?;
            if unpacked_bytes > policy.max_unpacked_bytes {
                return Err(ArchiveInstallError::UnpackedSizeLimit);
            }
        }
        scanned.push(ScannedEntry {
            index,
            relative_path,
            normalized,
            kind,
            declared_size,
        });
    }
    Ok(scanned)
}

fn normalize_entry(
    raw_name: &str,
    kind: EntryKind,
) -> Result<(PathBuf, String, Vec<String>), ArchiveInstallError> {
    if raw_name.is_empty()
        || raw_name.starts_with('/')
        || raw_name.starts_with('\\')
        || raw_name.contains('\\')
        || raw_name.chars().any(char::is_control)
    {
        return Err(ArchiveInstallError::UnsafeEntry);
    }
    let name = if kind == EntryKind::Directory {
        raw_name
            .strip_suffix('/')
            .ok_or(ArchiveInstallError::UnsafeEntry)?
    } else {
        raw_name
    };
    if name.is_empty() || name.ends_with('/') {
        return Err(ArchiveInstallError::UnsafeEntry);
    }

    let segments = name
        .split('/')
        .map(validate_segment)
        .collect::<Result<Vec<_>, _>>()?;
    let mut relative_path = PathBuf::new();
    for segment in &segments {
        relative_path.push(segment);
    }
    let normalized = segments.join("/");
    Ok((relative_path, normalized, segments))
}

fn validate_segment(segment: &str) -> Result<String, ArchiveInstallError> {
    if segment.is_empty()
        || matches!(segment, "." | "..")
        || segment.ends_with(['.', ' '])
        || segment.contains(':')
    {
        return Err(ArchiveInstallError::UnsafeEntry);
    }
    let device_stem = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(
        device_stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (device_stem.len() == 4
        && (device_stem.starts_with("COM") || device_stem.starts_with("LPT"))
        && matches!(device_stem.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(ArchiveInstallError::UnsafeEntry);
    }
    Ok(segment.to_owned())
}

fn register_path(
    registry: &mut BTreeMap<String, RegisteredPath>,
    normalized: &str,
    segments: &[String],
    kind: EntryKind,
) -> Result<(), ArchiveInstallError> {
    let mut display_prefix = String::new();
    for (index, segment) in segments.iter().enumerate() {
        if !display_prefix.is_empty() {
            display_prefix.push('/');
        }
        display_prefix.push_str(segment);
        let canonical = display_prefix.to_lowercase();
        let is_final = index + 1 == segments.len();
        let expected_kind = if is_final { kind } else { EntryKind::Directory };
        match registry.get_mut(&canonical) {
            Some(existing) if existing.normalized != display_prefix => {
                return Err(ArchiveInstallError::CaseCollision);
            }
            Some(existing) if existing.kind != expected_kind => {
                return Err(ArchiveInstallError::PathConflict);
            }
            Some(_) if is_final && expected_kind == EntryKind::File => {
                return Err(ArchiveInstallError::DuplicateEntry);
            }
            Some(existing) if is_final && existing.explicit => {
                return Err(ArchiveInstallError::DuplicateEntry);
            }
            Some(existing) if is_final => existing.explicit = true,
            Some(_) => {}
            None => {
                registry.insert(
                    canonical,
                    RegisteredPath {
                        normalized: display_prefix.clone(),
                        kind: expected_kind,
                        explicit: is_final,
                    },
                );
            }
        }
    }
    debug_assert_eq!(normalized, display_prefix);
    Ok(())
}

fn extract_archive(
    archive: &mut zip::ZipArchive<File>,
    scanned: &[ScannedEntry],
    staging: &Path,
    policy: ArchiveInstallPolicy,
) -> Result<BTreeMap<String, FileRecord>, ArchiveInstallError> {
    let mut actual = BTreeMap::new();
    let mut actual_total = 0_u64;
    for scanned_entry in scanned {
        let output = staging.join(&scanned_entry.relative_path);
        if output
            .parent()
            .is_none_or(|parent| !parent.starts_with(staging))
        {
            return Err(ArchiveInstallError::UnsafeFilesystemBoundary);
        }
        if scanned_entry.kind == EntryKind::Directory {
            fs::create_dir_all(&output).map_err(|source| io_error("create_directory", source))?;
            validate_created_parent(staging, &output)?;
            continue;
        }
        let parent = output
            .parent()
            .ok_or(ArchiveInstallError::UnsafeFilesystemBoundary)?;
        fs::create_dir_all(parent).map_err(|source| io_error("create_parent", source))?;
        validate_created_parent(staging, parent)?;

        let mut input = archive
            .by_index(scanned_entry.index)
            .map_err(|_| ArchiveInstallError::InvalidArchive)?;
        let mut output_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .map_err(|source| io_error("create_file", source))?;
        let remaining = policy
            .max_unpacked_bytes
            .checked_sub(actual_total)
            .ok_or(ArchiveInstallError::UnpackedSizeLimit)?;
        let mut bounded = (&mut input).take(remaining.saturating_add(1));
        let copied = std::io::copy(&mut bounded, &mut output_file)
            .map_err(|source| io_error("extract_file", source))?;
        if copied > remaining {
            return Err(ArchiveInstallError::UnpackedSizeLimit);
        }
        if copied != scanned_entry.declared_size {
            return Err(ArchiveInstallError::InvalidArchive);
        }
        output_file
            .flush()
            .map_err(|source| io_error("flush_file", source))?;
        output_file
            .sync_all()
            .map_err(|source| io_error("sync_file", source))?;
        actual_total = actual_total
            .checked_add(copied)
            .ok_or(ArchiveInstallError::UnpackedSizeLimit)?;
        let record = FileRecord {
            size: copied,
            sha256: digest_file(&output)?,
        };
        actual.insert(scanned_entry.normalized.clone(), record);
    }
    Ok(actual)
}

fn validate_created_parent(staging: &Path, parent: &Path) -> Result<(), ArchiveInstallError> {
    let canonical =
        fs::canonicalize(parent).map_err(|source| io_error("canonicalize_parent", source))?;
    validate_canonical_child(staging, &canonical)?;
    let relative = parent
        .strip_prefix(staging)
        .map_err(|_| ArchiveInstallError::UnsafeFilesystemBoundary)?;
    let mut cursor = staging.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        reject_reparse(&cursor)?;
    }
    Ok(())
}

fn validate_canonical_child(staging: &Path, canonical: &Path) -> Result<(), ArchiveInstallError> {
    if !canonical.starts_with(staging) {
        return Err(ArchiveInstallError::UnsafeFilesystemBoundary);
    }
    Ok(())
}

fn validate_payload(
    staging: &Path,
    actual: &BTreeMap<String, FileRecord>,
    dsh_version: &Version,
    node_version: &Version,
) -> Result<(), ArchiveInstallError> {
    let node_path = format!("node-v{node_version}-win-x64/node.exe");
    let package_path = "app/node_modules/@deepseek-ai/dsh/package.json";
    let cli_path = "app/node_modules/@deepseek-ai/dsh/lib/bin.js";
    for (path, component) in [
        (node_path.as_str(), "node"),
        (package_path, "package"),
        (cli_path, "cli"),
        (INVENTORY_FILE, "inventory"),
    ] {
        if !actual.contains_key(path) {
            return Err(ArchiveInstallError::RequiredPayloadMissing { component });
        }
    }

    let package_bytes =
        fs::read(staging.join(package_path)).map_err(|source| io_error("read_package", source))?;
    let package: serde_json::Value =
        serde_json::from_slice(&package_bytes).map_err(|_| ArchiveInstallError::PackageMismatch)?;
    if package.get("name").and_then(serde_json::Value::as_str) != Some("@deepseek-ai/dsh")
        || package.get("version").and_then(serde_json::Value::as_str)
            != Some(dsh_version.to_string().as_str())
    {
        return Err(ArchiveInstallError::PackageMismatch);
    }

    let inventory_bytes = fs::read(staging.join(INVENTORY_FILE))
        .map_err(|source| io_error("read_inventory", source))?;
    let inventory: Vec<InventoryEntry> = serde_json::from_slice(&inventory_bytes)
        .map_err(|_| ArchiveInstallError::InvalidInventory)?;
    let mut expected = BTreeMap::new();
    for item in inventory {
        let (_, normalized, _) = normalize_entry(&item.path, EntryKind::File)
            .map_err(|_| ArchiveInstallError::InvalidInventory)?;
        if normalized == INVENTORY_FILE || !is_canonical_sha256(&item.sha256) {
            return Err(ArchiveInstallError::InvalidInventory);
        }
        if expected
            .insert(
                normalized,
                FileRecord {
                    size: item.size,
                    sha256: item.sha256,
                },
            )
            .is_some()
        {
            return Err(ArchiveInstallError::InvalidInventory);
        }
    }
    let actual_payload = actual
        .iter()
        .filter(|(path, _)| path.as_str() != INVENTORY_FILE)
        .map(|(path, record)| (path.clone(), record.clone()))
        .collect::<BTreeMap<_, _>>();
    if expected != actual_payload {
        return Err(ArchiveInstallError::InventoryMismatch);
    }
    Ok(())
}

fn validate_source_archive(path: &Path) -> Result<(), ArchiveInstallError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_error("metadata_verified_archive", source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
        return Err(ArchiveInstallError::UnsafeFilesystemBoundary);
    }
    Ok(())
}

fn reported_zip_entry_count(path: &Path) -> Result<usize, ArchiveInstallError> {
    const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
    const EOCD_FIXED_SIZE: usize = 22;
    const MAX_COMMENT_SIZE: u64 = u16::MAX as u64;
    let mut file = File::open(path).map_err(|source| io_error("open_zip_footer", source))?;
    let file_len = file
        .seek(SeekFrom::End(0))
        .map_err(|source| io_error("seek_zip_footer", source))?;
    let tail_len = file_len.min(MAX_COMMENT_SIZE + EOCD_FIXED_SIZE as u64) as usize;
    if tail_len < EOCD_FIXED_SIZE {
        return Err(ArchiveInstallError::InvalidArchive);
    }
    file.seek(SeekFrom::End(-(tail_len as i64)))
        .map_err(|source| io_error("seek_zip_footer", source))?;
    let mut tail = vec![0_u8; tail_len];
    file.read_exact(&mut tail)
        .map_err(|source| io_error("read_zip_footer", source))?;
    for offset in (0..=tail.len().saturating_sub(EOCD_FIXED_SIZE)).rev() {
        if &tail[offset..offset + 4] != EOCD_SIGNATURE {
            continue;
        }
        let read_u16 = |start: usize| u16::from_le_bytes([tail[start], tail[start + 1]]);
        let comment_len = usize::from(read_u16(offset + 20));
        if offset + EOCD_FIXED_SIZE + comment_len != tail.len() {
            continue;
        }
        let disk = read_u16(offset + 4);
        let central_disk = read_u16(offset + 6);
        let entries_on_disk = read_u16(offset + 8);
        let total_entries = read_u16(offset + 10);
        if disk != 0
            || central_disk != 0
            || entries_on_disk != total_entries
            || total_entries == u16::MAX
        {
            return Err(ArchiveInstallError::InvalidArchive);
        }
        return Ok(usize::from(total_entries));
    }
    Err(ArchiveInstallError::InvalidArchive)
}

fn validate_trace_id(trace_id: &str) -> Result<(), ArchiveInstallError> {
    let valid = !trace_id.is_empty()
        && trace_id.len() <= 128
        && trace_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        return Err(ArchiveInstallError::InvalidTraceId);
    }
    Ok(())
}

fn reject_reparse(path: &Path) -> Result<(), ArchiveInstallError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_error("metadata_boundary", source))?;
    if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
        return Err(ArchiveInstallError::UnsafeFilesystemBoundary);
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

fn digest_file(path: &Path) -> Result<String, ArchiveInstallError> {
    let mut file = File::open(path).map_err(|source| io_error("hash_open", source))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_error("hash_read", source))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn io_error(operation: &'static str, source: std::io::Error) -> ArchiveInstallError {
    ArchiveInstallError::Io { operation, source }
}

#[cfg(test)]
mod tests {
    use super::{
        ArchiveInstallError, ArchiveInstallPolicy, ArchiveInstallRequest, RuntimeArchiveInstaller,
    };
    use crate::paths::{AppPaths, RuntimeLayout};
    use crate::runtime::install_state::InstalledRuntime;
    use semver::Version;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    const DSH_VERSION: &str = "0.1.1-rc.1";
    const NODE_VERSION: &str = "24.15.0";

    #[derive(Clone)]
    struct Entry<'a> {
        name: &'a str,
        bytes: &'a [u8],
        unix_mode: Option<u32>,
    }

    fn test_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dsh-archive-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn runtime() -> InstalledRuntime {
        InstalledRuntime::new(DSH_VERSION, "a".repeat(64)).expect("runtime")
    }

    fn layout(root: &Path) -> RuntimeLayout {
        RuntimeLayout::from_paths(&AppPaths::from_roots(root, root))
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn valid_payload() -> Vec<(String, Vec<u8>)> {
        vec![
            (
                "node-v24.15.0-win-x64/node.exe".to_owned(),
                b"node".to_vec(),
            ),
            (
                "app/node_modules/@deepseek-ai/dsh/package.json".to_owned(),
                br#"{"name":"@deepseek-ai/dsh","version":"0.1.1-rc.1"}"#.to_vec(),
            ),
            (
                "app/node_modules/@deepseek-ai/dsh/lib/bin.js".to_owned(),
                b"export {};".to_vec(),
            ),
            ("THIRD_PARTY_NOTICES.json".to_owned(), b"[]".to_vec()),
        ]
    }

    fn archive_bytes(payload: &[(String, Vec<u8>)]) -> Vec<u8> {
        let inventory: Vec<serde_json::Value> = payload
            .iter()
            .map(|(path, bytes)| {
                serde_json::json!({
                    "path": path,
                    "size": bytes.len(),
                    "sha256": sha256_hex(bytes),
                })
            })
            .collect();
        archive_with_inventory(payload, &inventory)
    }

    fn archive_with_inventory(
        payload: &[(String, Vec<u8>)],
        inventory: &[serde_json::Value],
    ) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            for (path, bytes) in payload {
                writer
                    .start_file(path, SimpleFileOptions::default())
                    .expect("start payload");
                writer.write_all(bytes).expect("write payload");
            }
            writer
                .start_file("inventory.json", SimpleFileOptions::default())
                .expect("start inventory");
            writer
                .write_all(&serde_json::to_vec(&inventory).expect("inventory json"))
                .expect("write inventory");
            writer.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn duplicate_name_archive() -> Vec<u8> {
        let mut bytes = custom_archive(&[
            Entry {
                name: "same1",
                bytes: b"a",
                unix_mode: None,
            },
            Entry {
                name: "same2",
                bytes: b"b",
                unix_mode: None,
            },
        ]);
        for offset in 0..=bytes.len() - 5 {
            if &bytes[offset..offset + 5] == b"same2" {
                bytes[offset..offset + 5].copy_from_slice(b"same1");
            }
        }
        bytes
    }

    fn custom_archive(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            for entry in entries {
                let mut options = SimpleFileOptions::default();
                if let Some(mode) = entry.unix_mode {
                    options = options.unix_permissions(mode);
                }
                if entry
                    .unix_mode
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
                {
                    writer
                        .add_symlink(
                            entry.name,
                            std::str::from_utf8(entry.bytes).expect("symlink target"),
                            options,
                        )
                        .expect("add symlink");
                    continue;
                }
                writer.start_file(entry.name, options).expect("start entry");
                writer.write_all(entry.bytes).expect("write entry");
            }
            writer.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn request(root: &Path, archive_path: PathBuf, trace_id: &str) -> ArchiveInstallRequest {
        ArchiveInstallRequest {
            archive_path,
            layout: layout(root),
            runtime: runtime(),
            node_version: Version::parse(NODE_VERSION).expect("node version"),
            trace_id: trace_id.to_owned(),
        }
    }

    async fn install_bytes(
        label: &str,
        bytes: &[u8],
        policy: ArchiveInstallPolicy,
    ) -> Result<super::InstalledRuntimeArchive, ArchiveInstallError> {
        let root = test_root(label);
        fs::create_dir_all(&root).expect("root");
        let archive_path = root.join("artifact.verified");
        fs::write(&archive_path, bytes).expect("archive");
        RuntimeArchiveInstaller::new(policy)
            .expect("policy")
            .install(request(&root, archive_path, "trace_01"))
            .await
    }

    #[tokio::test]
    async fn installs_verified_payload_into_immutable_version_directory() {
        let root = test_root("success");
        fs::create_dir_all(&root).expect("root");
        let bytes = archive_bytes(&valid_payload());
        let archive_path = root.join("artifact.verified");
        fs::write(&archive_path, bytes).expect("archive");

        let installed = RuntimeArchiveInstaller::new(ArchiveInstallPolicy::default())
            .expect("policy")
            .install(request(&root, archive_path.clone(), "trace_ok"))
            .await
            .expect("install");

        assert_eq!(installed.runtime, runtime());
        assert!(installed.runtime_dir.ends_with("dsh/0.1.1-rc.1"));
        assert_eq!(
            fs::read(installed.runtime_dir.join("node-v24.15.0-win-x64/node.exe")).expect("node"),
            b"node"
        );
        assert_eq!(
            fs::read(archive_path).expect("source retained"),
            archive_bytes(&valid_payload())
        );
    }

    #[tokio::test]
    async fn rejects_paths_that_can_escape_or_alias_on_windows() {
        for (index, name) in [
            "../escape",
            "/absolute",
            "C:/drive",
            r"..\escape",
            "safe/file:stream",
            "safe/trailing.",
            "safe/trailing ",
            "CON",
            "nul.txt",
            "safe/COM1.log",
        ]
        .iter()
        .enumerate()
        {
            let bytes = custom_archive(&[Entry {
                name,
                bytes: b"x",
                unix_mode: None,
            }]);
            let error = install_bytes(
                &format!("unsafe-{index}"),
                &bytes,
                ArchiveInstallPolicy::default(),
            )
            .await
            .expect_err("unsafe path");
            assert!(matches!(error, ArchiveInstallError::UnsafeEntry));
            assert!(!error.to_string().contains(name));
        }
    }

    #[tokio::test]
    async fn rejects_symlinks_duplicates_case_collisions_and_prefix_conflicts() {
        let cases = [
            custom_archive(&[Entry {
                name: "link",
                bytes: b"target",
                unix_mode: Some(0o120777),
            }]),
            duplicate_name_archive(),
            custom_archive(&[
                Entry {
                    name: "App/file",
                    bytes: b"a",
                    unix_mode: None,
                },
                Entry {
                    name: "app/other",
                    bytes: b"b",
                    unix_mode: None,
                },
            ]),
            custom_archive(&[
                Entry {
                    name: "prefix",
                    bytes: b"a",
                    unix_mode: None,
                },
                Entry {
                    name: "prefix/file",
                    bytes: b"b",
                    unix_mode: None,
                },
            ]),
        ];

        for (index, bytes) in cases.iter().enumerate() {
            let error = install_bytes(
                &format!("collision-{index}"),
                bytes,
                ArchiveInstallPolicy::default(),
            )
            .await
            .expect_err("unsafe archive structure");
            assert!(
                matches!(
                    error,
                    ArchiveInstallError::UnsupportedEntry
                        | ArchiveInstallError::DuplicateEntry
                        | ArchiveInstallError::CaseCollision
                        | ArchiveInstallError::PathConflict
                ),
                "case {index}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn enforces_entry_count_and_unpacked_size_limits() {
        let payload = valid_payload();
        let bytes = archive_bytes(&payload);
        let count_error = install_bytes(
            "count-limit",
            &bytes,
            ArchiveInstallPolicy {
                max_files: 4,
                max_unpacked_bytes: 1024,
            },
        )
        .await
        .expect_err("inventory is fifth file");
        assert!(matches!(count_error, ArchiveInstallError::FileCountLimit));

        let size_error = install_bytes(
            "size-limit",
            &bytes,
            ArchiveInstallPolicy {
                max_files: 10,
                max_unpacked_bytes: 8,
            },
        )
        .await
        .expect_err("payload exceeds limit");
        assert!(matches!(size_error, ArchiveInstallError::UnpackedSizeLimit));
    }

    #[tokio::test]
    async fn rejects_corrupt_archives_and_existing_target_versions() {
        let corrupt = install_bytes("corrupt", b"not a zip", ArchiveInstallPolicy::default())
            .await
            .expect_err("corrupt zip");
        assert!(matches!(corrupt, ArchiveInstallError::InvalidArchive));

        for (label, bytes) in [("empty", b"".as_slice()), ("short", b"x".as_slice())] {
            let error = install_bytes(label, bytes, ArchiveInstallPolicy::default())
                .await
                .expect_err("short ZIP cannot contain EOCD");
            assert!(matches!(error, ArchiveInstallError::InvalidArchive));
        }

        let root = test_root("existing");
        fs::create_dir_all(&root).expect("root");
        let runtime = runtime();
        let runtime_dir = layout(&root).runtime_dir(&runtime);
        fs::create_dir_all(&runtime_dir).expect("existing runtime");
        fs::write(runtime_dir.join("sentinel"), b"old").expect("sentinel");
        let archive_path = root.join("artifact.verified");
        fs::write(&archive_path, archive_bytes(&valid_payload())).expect("archive");

        let error = RuntimeArchiveInstaller::new(ArchiveInstallPolicy::default())
            .expect("policy")
            .install(request(&root, archive_path, "trace_existing"))
            .await
            .expect_err("immutable target");
        assert!(matches!(error, ArchiveInstallError::TargetAlreadyExists));
        assert_eq!(
            fs::read(runtime_dir.join("sentinel")).expect("old runtime"),
            b"old"
        );
    }

    #[tokio::test]
    async fn validates_required_files_package_version_and_inventory_closure() {
        let missing_node = valid_payload()
            .into_iter()
            .filter(|(path, _)| !path.ends_with("node.exe"))
            .collect::<Vec<_>>();
        let error = install_bytes(
            "missing-node",
            &archive_bytes(&missing_node),
            ArchiveInstallPolicy::default(),
        )
        .await
        .expect_err("node required");
        assert!(matches!(
            error,
            ArchiveInstallError::RequiredPayloadMissing { component: "node" }
        ));

        let mut wrong_version = valid_payload();
        wrong_version[1].1 = br#"{"name":"@deepseek-ai/dsh","version":"9.9.9"}"#.to_vec();
        let error = install_bytes(
            "wrong-version",
            &archive_bytes(&wrong_version),
            ArchiveInstallPolicy::default(),
        )
        .await
        .expect_err("version mismatch");
        assert!(matches!(error, ArchiveInstallError::PackageMismatch));

        let payload = valid_payload();
        let mut inventory: Vec<serde_json::Value> = payload
            .iter()
            .map(|(path, bytes)| {
                serde_json::json!({
                    "path": path,
                    "size": bytes.len(),
                    "sha256": sha256_hex(bytes),
                })
            })
            .collect();
        inventory[0]["sha256"] = serde_json::Value::String("0".repeat(64));
        let bytes = archive_with_inventory(&payload, &inventory);
        let error = install_bytes(
            "inventory-mismatch",
            &bytes,
            ArchiveInstallPolicy::default(),
        )
        .await
        .expect_err("tampered archive");
        assert!(matches!(error, ArchiveInstallError::InventoryMismatch));
    }

    #[test]
    fn rejects_invalid_policy() {
        assert!(matches!(
            RuntimeArchiveInstaller::new(ArchiveInstallPolicy {
                max_files: 0,
                max_unpacked_bytes: 1
            }),
            Err(ArchiveInstallError::InvalidPolicy { field: "max_files" })
        ));
        assert!(matches!(
            RuntimeArchiveInstaller::new(ArchiveInstallPolicy {
                max_files: 1,
                max_unpacked_bytes: 0
            }),
            Err(ArchiveInstallError::InvalidPolicy {
                field: "max_unpacked_bytes"
            })
        ));
    }

    #[tokio::test]
    async fn rejects_trace_ids_that_could_alias_staging_paths() {
        for (index, trace_id) in ["", ".", "..", "a/b", r"a\b", "with space"]
            .iter()
            .enumerate()
        {
            let root = test_root(&format!("trace-{index}"));
            fs::create_dir_all(&root).expect("root");
            let archive_path = root.join("artifact.verified");
            fs::write(&archive_path, archive_bytes(&valid_payload())).expect("archive");

            let error = RuntimeArchiveInstaller::new(ArchiveInstallPolicy::default())
                .expect("policy")
                .install(request(&root, archive_path, trace_id))
                .await
                .expect_err("invalid trace");

            assert!(matches!(error, ArchiveInstallError::InvalidTraceId));
        }
    }

    #[test]
    fn canonical_child_boundary_rejects_reparse_targets_outside_staging() {
        let staging = Path::new(r"C:\Users\demo\AppData\Local\DSH\runtimes\dsh\.staging-t");

        assert!(
            super::validate_canonical_child(staging, &staging.join("app/node_modules")).is_ok()
        );
        assert!(matches!(
            super::validate_canonical_child(staging, Path::new(r"C:\outside\node_modules")),
            Err(ArchiveInstallError::UnsafeFilesystemBoundary)
        ));
    }
}
