use super::{MAX_SKIN_BYTES, SkinError, SkinFormat, SkinImage, SkinStore};
use crate::paths::AppPaths;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::http::{HeaderValue, Response, StatusCode, header};
use tauri::{AppHandle, Manager};

const PROTOCOL_SCHEME: &str = "dsh-skin";
const PROTOCOL_HOST: &str = "localhost";
const WINDOWS_TRANSPORT_SCHEME: &str = "http";
const WINDOWS_TRANSPORT_HOST: &str = "dsh-skin.localhost";
const CACHE_CONTROL_VALUE: &str = "private, max-age=31536000, immutable";
const NOT_FOUND_BODY: &[u8] = b"not found";
const INTERNAL_ERROR_BODY: &[u8] = b"internal error";
const MANAGED_EXTENSIONS: [(&str, &str); 3] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("webp", "image/webp"),
];

/// 生成只读皮肤资源的规范 URL。
///
/// :param digest: 图片内容的规范小写 SHA-256 摘要。
/// :return: 摘要有效时返回 `dsh-skin://localhost/<digest>`，否则返回 `None`。
/// :raises: 此函数不访问文件系统，不产生错误。
pub fn skin_resource_url(digest: &str) -> Option<String> {
    is_canonical_digest(digest).then(|| format!("{PROTOCOL_SCHEME}://{PROTOCOL_HOST}/{digest}"))
}

/// 将当前设置快照映射为单个只读皮肤资源的协议处理器。
///
/// Windows WebView2 的 `http://dsh-skin.localhost/...` 是 Tauri 内部传输细节，
/// 不能成为库调用方可访问的公开协议入口：
///
/// ```compile_fail
/// use dsh_desktop_lib::skin::{SkinProtocol, SkinStore};
/// use std::path::PathBuf;
///
/// let protocol = SkinProtocol::new(
///     SkinStore::new(PathBuf::from("settings"), PathBuf::from("skins")),
///     PathBuf::from("skins"),
/// );
/// let _ = protocol.request_webview("http://dsh-skin.localhost/invalid");
/// ```
#[derive(Clone, Debug)]
pub struct SkinProtocol {
    store: SkinStore,
    skins_root: PathBuf,
    previews: SkinPreviewRegistry,
}

/// 自定义协议请求的类型化窗口受众。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkinProtocolAudience {
    Main,
    Appearance,
    Denied,
}

impl SkinProtocolAudience {
    /// 将 Tauri WebView label 收敛为封闭的协议受众。
    ///
    /// :param label: 发起自定义协议请求的 WebView label。
    /// :return: `main`、`appearance` 或默认拒绝受众。
    /// :raises: 未知 label 直接映射为拒绝，不产生错误。
    pub(crate) fn from_webview_label(label: &str) -> Self {
        match label {
            "main" => Self::Main,
            "appearance" => Self::Appearance,
            _ => Self::Denied,
        }
    }
}

/// 只保存设置窗口当前一次、尚未提交的导入图片授权。
type PreviewValidator = dyn Fn(&Path, &SkinImage) -> Result<(), SkinError> + Send + Sync;

#[derive(Clone)]
pub(crate) struct SkinPreviewRegistry {
    skins_root: PathBuf,
    authorization: Arc<PreviewAuthorization>,
    validator: Arc<PreviewValidator>,
}

impl std::fmt::Debug for SkinPreviewRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SkinPreviewRegistry")
            .field("skins_root", &self.skins_root)
            .field("authorization", &self.authorization)
            .field("validator", &"<bounded-preview-validator>")
            .finish()
    }
}

/// 预览登记的稳定完成状态；过期任务不是用户可见错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkinPreviewRegistration {
    Registered,
    Superseded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviewRegistrationTicket(u64);

#[derive(Debug, Default)]
struct PreviewAuthorizationState {
    epoch: u64,
    digest: Option<String>,
}

#[derive(Debug, Default)]
struct PreviewAuthorization {
    state: Mutex<PreviewAuthorizationState>,
}

impl PreviewAuthorization {
    fn new() -> Self {
        Self::default()
    }

    fn begin(&self) -> Result<PreviewRegistrationTicket, SkinError> {
        let mut state = self.state.lock().map_err(|_| SkinError::FileSystem)?;
        state.epoch = state
            .epoch
            .checked_add(1)
            .ok_or(SkinError::RevisionExhausted)?;
        Ok(PreviewRegistrationTicket(state.epoch))
    }

    fn commit(
        &self,
        ticket: PreviewRegistrationTicket,
        digest: String,
    ) -> Result<SkinPreviewRegistration, SkinError> {
        let mut state = self.state.lock().map_err(|_| SkinError::FileSystem)?;
        if state.epoch != ticket.0 {
            return Ok(SkinPreviewRegistration::Superseded);
        }
        state.digest = Some(digest);
        Ok(SkinPreviewRegistration::Registered)
    }

    fn clear(&self) -> Result<(), SkinError> {
        let mut state = self.state.lock().map_err(|_| SkinError::FileSystem)?;
        state.epoch = state
            .epoch
            .checked_add(1)
            .ok_or(SkinError::RevisionExhausted)?;
        state.digest = None;
        Ok(())
    }

    fn digest(&self) -> Result<Option<String>, SkinError> {
        self.state
            .lock()
            .map(|state| state.digest.clone())
            .map_err(|_| SkinError::FileSystem)
    }
}

impl SkinPreviewRegistry {
    /// 创建绑定到固定托管图片目录的空预览登记表。
    ///
    /// :param skins_root: `SkinImporter` 写入不可变图片的固定目录。
    /// :return: 不授权任何预览摘要的进程内登记表。
    /// :raises: 此构造函数只保存路径，不访问文件系统。
    pub(crate) fn new(skins_root: PathBuf) -> Self {
        Self::with_validator(skins_root, Arc::new(validate_imported_preview))
    }

    fn with_validator(skins_root: PathBuf, validator: Arc<PreviewValidator>) -> Self {
        Self {
            skins_root,
            authorization: Arc::new(PreviewAuthorization::new()),
            validator,
        }
    }

    /// 在任何异步导入工作开始前领取选择顺序 ticket。
    ///
    /// :return: 严格晚于先前选择或清除操作的顺序 ticket。
    /// :raises SkinError: 登记锁不可用或顺序号耗尽时返回稳定错误。
    pub(crate) fn begin_registration(&self) -> Result<PreviewRegistrationTicket, SkinError> {
        self.authorization.begin()
    }

    /// 用预先领取的 ticket 登记已验证并复制到规范路径的预览图片。
    ///
    /// :param ticket: 在图片导入开始前领取的选择顺序 ticket。
    /// :param image: 导入器返回的类型化托管图片。
    /// :return: 最新任务登记成功返回 `Registered`；已被较新选择或清除取代返回 `Superseded`。
    /// :raises SkinError: 图片不在规范托管路径、内容已变化或登记锁不可用时失败关闭。
    pub(crate) fn commit_imported(
        &self,
        ticket: PreviewRegistrationTicket,
        image: &SkinImage,
    ) -> Result<SkinPreviewRegistration, SkinError> {
        // 即使 ticket 已过期也完成托管文件绑定验证，避免将不可信元数据带入状态机。
        (self.validator)(&self.skins_root, image)?;
        self.authorization.commit(ticket, image.digest.clone())
    }

    /// 撤销设置窗口尚未提交的预览授权，不删除托管图片。
    ///
    /// :return: 预览摘要清空后返回 `()`。
    /// :raises SkinError: 进程内登记锁中毒时返回固定文件系统错误。
    pub(crate) fn clear(&self) -> Result<(), SkinError> {
        self.authorization.clear()
    }

    fn digest(&self) -> Result<Option<String>, ProtocolError> {
        self.authorization
            .digest()
            .map_err(|_| ProtocolError::Internal)
    }
}

fn validate_imported_preview(skins_root: &Path, image: &SkinImage) -> Result<(), SkinError> {
    if !is_canonical_digest(&image.digest)
        || image.byte_size > MAX_SKIN_BYTES
        || image.path != skins_root.join(format!("{}.{}", image.digest, image.format.extension()))
    {
        return Err(SkinError::InvalidSettings);
    }
    let resource =
        read_managed_resource(skins_root, &image.digest).map_err(|_| SkinError::FileSystem)?;
    if resource.mime != mime_for_format(image.format)
        || resource.bytes.len() as u64 != image.byte_size
    {
        return Err(SkinError::FileSystem);
    }
    Ok(())
}

impl SkinProtocol {
    /// 创建绑定到设置仓库与预定义托管图片目录的协议处理器。
    ///
    /// :param store: 用于读取当前登记摘要的设置仓库。
    /// :param skins_root: 导入器使用的固定托管图片目录。
    /// :return: 尚未读取设置或图片的协议处理器。
    /// :raises: 此构造函数只保存依赖，不产生错误。
    pub fn new(store: SkinStore, skins_root: PathBuf) -> Self {
        Self {
            store,
            previews: SkinPreviewRegistry::new(skins_root.clone()),
            skins_root,
        }
    }

    /// 创建共享设置窗口预览授权的内部协议处理器。
    ///
    /// :param store: 用于读取持久化 active 摘要的设置仓库。
    /// :param skins_root: 导入器使用的固定托管图片目录。
    /// :param previews: 由 Tauri state 托管、仅供 appearance 使用的进程内预览登记表。
    /// :return: 同时执行持久化 active 与窗口受众授权的协议处理器。
    /// :raises: 此构造函数只绑定已创建的依赖，不产生错误。
    pub(crate) fn with_preview_registry(
        store: SkinStore,
        skins_root: PathBuf,
        previews: SkinPreviewRegistry,
    ) -> Self {
        Self {
            store,
            skins_root,
            previews,
        }
    }

    /// 处理严格的公开皮肤资源 URL。
    ///
    /// :param uri: 必须精确匹配 `dsh-skin://localhost/<digest>` 的 URI。
    /// :return: 成功时返回图片及固定缓存头；越权 URI 返回固定 404，内部校验失败返回固定 500。
    /// :raises: 所有错误均收敛为脱敏 HTTP 响应，不向调用方抛出。
    pub fn request(&self, uri: &str) -> Response<Vec<u8>> {
        self.request_for_audience(uri, SkinProtocolAudience::Main)
    }

    pub(crate) fn request_for_audience(
        &self,
        uri: &str,
        audience: SkinProtocolAudience,
    ) -> Response<Vec<u8>> {
        let Some(requested_digest) = parse_canonical_uri(uri) else {
            return fixed_response(StatusCode::NOT_FOUND, NOT_FOUND_BODY);
        };
        match self.read_authorized(&requested_digest, audience) {
            Ok(resource) => success_response(resource),
            Err(ProtocolError::NotFound) => fixed_response(StatusCode::NOT_FOUND, NOT_FOUND_BODY),
            Err(ProtocolError::Internal) => {
                fixed_response(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY)
            }
        }
    }

    /// 处理 Tauri 在 Windows WebView2 中传入的自定义协议传输 URI。
    ///
    /// :param uri: 规范公开 URL，或 Tauri 固定转换出的 `http://dsh-skin.localhost/<digest>`。
    /// :return: 与 [`Self::request`] 相同的脱敏响应。
    /// :raises: 非精确传输形式直接返回固定 404，不向调用方抛出。
    fn request_webview(&self, uri: &str, audience: SkinProtocolAudience) -> Response<Vec<u8>> {
        if uri.starts_with("dsh-skin:") {
            return self.request_for_audience(uri, audience);
        }
        let Some(digest) = parse_windows_transport_uri(uri) else {
            return fixed_response(StatusCode::NOT_FOUND, NOT_FOUND_BODY);
        };
        // Windows 的 http 形式只是 WebView2 传输细节；文件映射仍统一经过规范协议解析。
        self.request_for_audience(
            &skin_resource_url(&digest)
                .expect("传输解析已保证摘要为规范小写 SHA-256，不可能构造失败"),
            audience,
        )
    }

    fn read_authorized(
        &self,
        requested_digest: &str,
        audience: SkinProtocolAudience,
    ) -> Result<SkinResource, ProtocolError> {
        if audience == SkinProtocolAudience::Denied {
            return Err(ProtocolError::NotFound);
        }
        let snapshot = self.store.load().map_err(|_| ProtocolError::Internal)?;
        let active = snapshot.settings.image_digest.as_deref();
        let preview = if audience == SkinProtocolAudience::Appearance {
            self.previews.digest()?
        } else {
            None
        };
        if active != Some(requested_digest) && preview.as_deref() != Some(requested_digest) {
            return Err(ProtocolError::NotFound);
        }
        read_managed_resource(&self.skins_root, requested_digest)
    }
}

fn read_managed_resource(
    skins_root: &Path,
    registered_digest: &str,
) -> Result<SkinResource, ProtocolError> {
    let directory_guard = open_directory_guard(skins_root)?;
    let mut matched: Option<(&str, RegularFileGuard)> = None;
    for (extension, mime) in MANAGED_EXTENSIONS {
        // 请求文本永不参与路径拼接；这里只使用已与设置快照精确匹配的规范摘要和固定扩展名。
        let candidate = skins_root.join(format!("{registered_digest}.{extension}"));
        match open_regular_guard_io(&candidate) {
            Ok(guard) => {
                if matched.is_some() {
                    return Err(ProtocolError::Internal);
                }
                matched = Some((mime, guard));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ProtocolError::Internal),
        }
    }
    let (mime, mut file_guard) = matched.ok_or(ProtocolError::Internal)?;
    let candidate = skins_root.join(format!("{registered_digest}.{}", extension_for_mime(mime)));
    validate_directory_guard(&directory_guard, skins_root)?;
    validate_regular_guard(&file_guard, &candidate)?;
    if file_guard.size > MAX_SKIN_BYTES {
        return Err(ProtocolError::Internal);
    }
    file_guard
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|_| ProtocolError::Internal)?;
    let bytes = read_bounded(&mut file_guard.file, MAX_SKIN_BYTES)?;
    let (identity_after, size_after) = validated_handle_metadata(&file_guard.file, false)?;
    if identity_after != file_guard.identity
        || size_after != file_guard.size
        || bytes.len() as u64 != file_guard.size
        || format!("{:x}", Sha256::digest(&bytes)) != registered_digest
    {
        return Err(ProtocolError::Internal);
    }
    validate_directory_guard(&directory_guard, skins_root)?;
    validate_regular_guard(&file_guard, &candidate)?;
    Ok(SkinResource {
        bytes,
        mime,
        digest: registered_digest.to_owned(),
    })
}

/// 从应用固定目录处理 Tauri 自定义协议请求。
///
/// :param app: 当前 Tauri 应用句柄，用于解析预定义用户数据目录。
/// :param uri: WebView2 交给协议回调的完整 URI。
/// :return: 固定成功、404 或 500 响应，且响应体不包含本地路径。
/// :raises: 路径解析和文件系统错误均收敛为固定 500 响应。
pub(crate) fn handle_tauri_skin_request(
    app: &AppHandle,
    webview_label: &str,
    uri: &str,
) -> Response<Vec<u8>> {
    let audience = SkinProtocolAudience::from_webview_label(webview_label);
    if audience == SkinProtocolAudience::Denied {
        return fixed_response(StatusCode::NOT_FOUND, NOT_FOUND_BODY);
    }
    let Ok(paths) = AppPaths::resolve(app) else {
        return fixed_response(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY);
    };
    let Some(previews) = app.try_state::<SkinPreviewRegistry>() else {
        return fixed_response(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY);
    };
    let store = SkinStore::new(paths.settings.clone(), paths.skins.clone());
    SkinProtocol::with_preview_registry(store, paths.skins, previews.inner().clone())
        .request_webview(uri, audience)
}

#[derive(Debug)]
struct SkinResource {
    bytes: Vec<u8>,
    mime: &'static str,
    digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtocolError {
    NotFound,
    Internal,
}

fn parse_canonical_uri(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    let digest = url.path().strip_prefix('/')?;
    if url.scheme() != PROTOCOL_SCHEME
        || url.host_str() != Some(PROTOCOL_HOST)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !is_canonical_digest(digest)
        || skin_resource_url(digest).as_deref() != Some(value)
    {
        return None;
    }
    Some(digest.to_owned())
}

fn parse_windows_transport_uri(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    let digest = url.path().strip_prefix('/')?;
    let expected = format!("{WINDOWS_TRANSPORT_SCHEME}://{WINDOWS_TRANSPORT_HOST}/{digest}");
    if url.scheme() != WINDOWS_TRANSPORT_SCHEME
        || url.host_str() != Some(WINDOWS_TRANSPORT_HOST)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !is_canonical_digest(digest)
        || value != expected
    {
        return None;
    }
    Some(digest.to_owned())
}

fn is_canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn success_response(resource: SkinResource) -> Response<Vec<u8>> {
    let mut response = Response::new(resource.bytes);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(resource.mime),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_VALUE),
    );
    let etag = HeaderValue::from_str(&format!("\"{}\"", resource.digest))
        .expect("规范十六进制摘要始终是合法 HTTP 头值");
    response.headers_mut().insert(header::ETAG, etag);
    response
}

fn fixed_response(status: StatusCode, body: &[u8]) -> Response<Vec<u8>> {
    let mut response = Response::new(body.to_vec());
    *response.status_mut() = status;
    response
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => unreachable!("MIME 仅来自封闭的 MANAGED_EXTENSIONS"),
    }
}

fn mime_for_format(format: SkinFormat) -> &'static str {
    match format {
        SkinFormat::Png => "image/png",
        SkinFormat::Jpeg => "image/jpeg",
        SkinFormat::Webp => "image/webp",
    }
}

fn read_bounded(file: &mut File, limit: u64) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProtocolError::Internal)?;
    if bytes.len() as u64 > limit {
        return Err(ProtocolError::Internal);
    }
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

fn open_directory_guard(path: &Path) -> Result<DirectoryGuard, ProtocolError> {
    let file = open_directory_file(path).map_err(|_| ProtocolError::Internal)?;
    let (identity, _) = validated_handle_metadata(&file, true)?;
    Ok(DirectoryGuard { file, identity })
}

fn validate_directory_guard(guard: &DirectoryGuard, path: &Path) -> Result<(), ProtocolError> {
    let (held_identity, _) = validated_handle_metadata(&guard.file, true)?;
    let current = open_directory_guard(path)?;
    if held_identity != guard.identity || current.identity != guard.identity {
        return Err(ProtocolError::Internal);
    }
    Ok(())
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

fn validate_regular_guard(guard: &RegularFileGuard, path: &Path) -> Result<(), ProtocolError> {
    let (held_identity, held_size) = validated_handle_metadata(&guard.file, false)?;
    let current = open_regular_guard_io(path).map_err(|_| ProtocolError::Internal)?;
    if held_identity != guard.identity
        || held_size != guard.size
        || current.identity != guard.identity
        || current.size != guard.size
    {
        return Err(ProtocolError::Internal);
    }
    Ok(())
}

fn validated_handle_metadata(
    file: &File,
    directory: bool,
) -> Result<(FileIdentity, u64), ProtocolError> {
    validated_handle_metadata_io(file, directory).map_err(|_| ProtocolError::Internal)
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
    // SAFETY: `file` 在调用期间保持打开，输出结构体已按 Win32 API 要求初始化。
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
            "皮肤协议文件安全属性无效",
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
            "皮肤协议文件安全属性无效",
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

#[cfg(test)]
mod tests {
    use super::{
        SkinPreviewRegistration, SkinPreviewRegistry, SkinProtocol, SkinProtocolAudience,
        parse_windows_transport_uri,
    };
    use crate::skin::{
        MaskTone, SkinDraft, SkinFit, SkinFormat, SkinImage, SkinPosition, SkinStore,
        skin_resource_url,
    };
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dsh-desktop-protocol-audience-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn seed_image(root: &std::path::Path, bytes: &[u8]) -> SkinImage {
        let digest = format!("{:x}", Sha256::digest(bytes));
        let path = root.join(format!("{digest}.png"));
        fs::write(&path, bytes).expect("seed managed image");
        SkinImage {
            digest,
            format: SkinFormat::Png,
            width: 1,
            height: 1,
            byte_size: bytes.len() as u64,
            path,
        }
    }

    fn active_draft(image: &SkinImage) -> SkinDraft {
        SkinDraft {
            immersive: true,
            image_digest: Some(image.digest.clone()),
            fit: SkinFit::Cover,
            position: SkinPosition::Center,
            blur_px: 0,
            glass_blur_px: 0,
            mask_tone: MaskTone::Light,
            mask_opacity_percent: 22,
            panel_opacity_percent: 88,
        }
    }

    #[test]
    fn windows_transport_accepts_only_the_exact_tauri_origin() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_windows_transport_uri(&format!("http://dsh-skin.localhost/{digest}")),
            Some(digest.clone())
        );
        for denied in [
            format!("https://dsh-skin.localhost/{digest}"),
            format!("http://dsh-skin.localhost:80/{digest}"),
            format!("http://user@dsh-skin.localhost/{digest}"),
            format!("http://dsh-skin.localhost/{digest}?x=1"),
        ] {
            assert_eq!(parse_windows_transport_uri(&denied), None);
        }
    }

    #[test]
    fn unsaved_preview_is_authorized_only_for_appearance_until_cleared() {
        let root = unique_root("preview");
        let settings_root = root.join("settings");
        let skins_root = root.join("skins");
        fs::create_dir_all(&settings_root).expect("settings root");
        fs::create_dir_all(&skins_root).expect("skins root");
        let active = seed_image(&skins_root, b"active-image");
        let preview = seed_image(&skins_root, b"unsaved-preview");
        let store = SkinStore::new(settings_root, skins_root.clone());
        store.save(0, active_draft(&active)).expect("save active");
        let previews = SkinPreviewRegistry::new(skins_root.clone());
        let preview_ticket = previews.begin_registration().expect("preview ticket");
        previews
            .commit_imported(preview_ticket, &preview)
            .expect("register imported preview");
        let protocol = SkinProtocol::with_preview_registry(store, skins_root, previews.clone());
        let preview_url = skin_resource_url(&preview.digest).expect("preview URL");

        assert_eq!(
            protocol
                .request_for_audience(&preview_url, SkinProtocolAudience::Appearance)
                .status(),
            200
        );
        assert_eq!(
            protocol
                .request_for_audience(&preview_url, SkinProtocolAudience::Main)
                .status(),
            404
        );
        assert_eq!(
            protocol
                .request_for_audience(&preview_url, SkinProtocolAudience::Denied)
                .status(),
            404
        );

        previews.clear().expect("clear preview");
        assert_eq!(
            protocol
                .request_for_audience(&preview_url, SkinProtocolAudience::Appearance)
                .status(),
            404
        );
    }

    #[test]
    fn audience_mapping_denies_every_unknown_window_label() {
        assert_eq!(
            SkinProtocolAudience::from_webview_label("main"),
            SkinProtocolAudience::Main
        );
        assert_eq!(
            SkinProtocolAudience::from_webview_label("appearance"),
            SkinProtocolAudience::Appearance
        );
        for label in ["", "updates", "Appearance", "main-child"] {
            assert_eq!(
                SkinProtocolAudience::from_webview_label(label),
                SkinProtocolAudience::Denied
            );
        }
    }

    #[test]
    fn preview_registry_rejects_an_image_not_bound_to_its_managed_path() {
        let root = unique_root("outside-preview");
        let skins_root = root.join("skins");
        fs::create_dir_all(&skins_root).expect("skins root");
        let outside_root = root.join("outside");
        fs::create_dir_all(&outside_root).expect("outside root");
        let outside = seed_image(&outside_root, b"outside-image");
        let previews = SkinPreviewRegistry::new(skins_root);

        let ticket = previews.begin_registration().expect("preview ticket");
        assert!(previews.commit_imported(ticket, &outside).is_err());
        assert_eq!(previews.digest().expect("read registry"), None);
    }

    #[test]
    fn clear_supersedes_an_older_inflight_registration() {
        let validation_started = Arc::new(Barrier::new(2));
        let clear_finished = Arc::new(Barrier::new(2));
        let validator_started = validation_started.clone();
        let validator_clear_finished = clear_finished.clone();
        let registry = Arc::new(SkinPreviewRegistry::with_validator(
            PathBuf::new(),
            Arc::new(move |_, _| {
                validator_started.wait();
                validator_clear_finished.wait();
                Ok(())
            }),
        ));
        let image = SkinImage {
            digest: "a".repeat(64),
            format: SkinFormat::Png,
            width: 1,
            height: 1,
            byte_size: 1,
            path: PathBuf::new(),
        };
        let ticket = registry.begin_registration().expect("old ticket");
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || {
            worker_registry
                .commit_imported(ticket, &image)
                .expect("stable superseded result")
        });

        validation_started.wait();
        registry.clear().expect("clear preview");
        clear_finished.wait();

        assert_eq!(
            worker.join().expect("worker"),
            SkinPreviewRegistration::Superseded
        );
        assert_eq!(registry.digest().expect("current digest"), None);
    }

    #[test]
    fn newer_registration_start_prevents_older_completion_from_overwriting_it() {
        let old_validation_started = Arc::new(Barrier::new(2));
        let new_committed = Arc::new(Barrier::new(2));
        let validator_old_started = old_validation_started.clone();
        let validator_new_committed = new_committed.clone();
        let old_digest = "a".repeat(64);
        let validator_old_digest = old_digest.clone();
        let registry = Arc::new(SkinPreviewRegistry::with_validator(
            PathBuf::new(),
            Arc::new(move |_, image| {
                if image.digest == validator_old_digest {
                    validator_old_started.wait();
                    validator_new_committed.wait();
                }
                Ok(())
            }),
        ));
        let old_image = SkinImage {
            digest: old_digest,
            format: SkinFormat::Png,
            width: 1,
            height: 1,
            byte_size: 1,
            path: PathBuf::new(),
        };
        let new_image = SkinImage {
            digest: "b".repeat(64),
            format: SkinFormat::Png,
            width: 1,
            height: 1,
            byte_size: 1,
            path: PathBuf::new(),
        };
        let old_ticket = registry.begin_registration().expect("old ticket");
        let old_registry = registry.clone();
        let old = std::thread::spawn(move || {
            old_registry
                .commit_imported(old_ticket, &old_image)
                .expect("old stable result")
        });

        old_validation_started.wait();
        let new_ticket = registry.begin_registration().expect("new ticket");
        assert_eq!(
            registry
                .commit_imported(new_ticket, &new_image)
                .expect("new commit"),
            SkinPreviewRegistration::Registered
        );
        new_committed.wait();

        assert_eq!(
            old.join().expect("old worker"),
            SkinPreviewRegistration::Superseded
        );
        assert_eq!(
            registry.digest().expect("current digest"),
            Some("b".repeat(64))
        );
    }
}
