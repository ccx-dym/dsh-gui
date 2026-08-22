use super::adapter::refresh_main_skin;
use super::protocol::{
    PreviewRegistrationTicket, SkinPreviewRegistration, SkinPreviewRegistry, skin_resource_url,
};
use super::{
    SkinDraft, SkinError, SkinErrorKind, SkinFormat, SkinImage, SkinImporter, SkinStateEnvelope,
    SkinStore,
};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;

pub const SKIN_STATE_EVENT: &str = "skin-state";

/// 设置窗口可安全序列化的托管图片视图。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinImageView {
    pub digest: String,
    pub format: SkinFormat,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub protocol_url: String,
}

impl TryFrom<SkinImage> for SkinImageView {
    type Error = SkinError;

    fn try_from(image: SkinImage) -> Result<Self, Self::Error> {
        // URL 只由已经验证的规范摘要生成；该视图刻意丢弃源路径和托管路径。
        let protocol_url = skin_resource_url(&image.digest).ok_or(SkinError::InvalidSettings)?;
        Ok(Self {
            digest: image.digest,
            format: image.format,
            width: image.width,
            height: image.height,
            bytes: image.byte_size,
            protocol_url,
        })
    }
}

/// 可跨 IPC 返回且不包含动态路径的固定皮肤命令错误。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinCommandError {
    pub code: &'static str,
    pub kind: SkinErrorKind,
    pub message: &'static str,
}

impl From<SkinError> for SkinCommandError {
    fn from(error: SkinError) -> Self {
        let kind = error.kind();
        let (code, message) = command_error_fields(kind);
        Self {
            code,
            kind,
            message,
        }
    }
}

fn command_error_fields(kind: SkinErrorKind) -> (&'static str, &'static str) {
    match kind {
        SkinErrorKind::InvalidSettings => ("invalid_settings", "皮肤设置超出允许范围"),
        SkinErrorKind::ImageNotRegistered => ("image_not_registered", "请先选择有效的皮肤图片"),
        SkinErrorKind::TooLarge => ("too_large", "图片文件不能超过 20 MiB"),
        SkinErrorKind::Dimensions => ("dimensions", "图片尺寸超出 8K 限制"),
        SkinErrorKind::Decode => ("decode", "图片已损坏或无法完整读取"),
        SkinErrorKind::UnsupportedFormat => ("unsupported_format", "仅支持 PNG、JPEG 和 WebP 图片"),
        SkinErrorKind::Worker => ("worker", "图片处理任务意外终止"),
        SkinErrorKind::RevisionConflict => (
            "revision_conflict",
            "皮肤设置已在其他窗口中更新，请重新加载",
        ),
        SkinErrorKind::RevisionExhausted => ("revision_exhausted", "皮肤设置版本号已耗尽"),
        SkinErrorKind::CorruptSettings => ("corrupt_settings", "皮肤设置文件无效"),
        SkinErrorKind::FileSystem => ("file_system", "无法访问皮肤设置"),
    }
}

/// 协调持久化、不可变导入与一次性预览授权的皮肤控制器。
#[derive(Clone, Debug)]
pub struct SkinController {
    store: SkinStore,
    importer: SkinImporter,
    previews: SkinPreviewRegistry,
    mutation: Arc<Mutex<()>>,
}

impl SkinController {
    /// 创建只绑定预定义设置和皮肤目录的控制器。
    ///
    /// :param store: 持有 revision 并执行原子保存的设置仓库。
    /// :param skins_root: 与 `AppPaths.skins` 相同的固定托管目录。
    /// :param previews: 与只读协议共享的设置窗口预览登记表。
    /// :return: 尚未执行文件系统操作的控制器。
    /// :raises: 构造过程只保存类型化依赖，不产生错误。
    pub(crate) fn new(
        store: SkinStore,
        skins_root: PathBuf,
        previews: SkinPreviewRegistry,
    ) -> Self {
        Self {
            store,
            importer: SkinImporter::new(skins_root),
            previews,
            mutation: Arc::new(Mutex::new(())),
        }
    }

    /// 读取当前已提交的皮肤快照。
    ///
    /// :return: 带单调 revision 的设置快照。
    /// :raises SkinError: 设置文件损坏或固定目录不可访问时返回稳定错误。
    pub fn load(&self) -> Result<SkinStateEnvelope, SkinError> {
        self.store.load()
    }

    /// 导入并登记最近完成的设置窗口预览图片。
    ///
    /// :param source: Windows 原生选择器返回的本地路径。
    /// :return: 最新预览返回脱敏视图；被较新登记取代时返回 `None`。
    /// :raises SkinError: 图片验证、不可变复制或预览登记失败时返回稳定错误。
    pub async fn import(&self, source: PathBuf) -> Result<Option<SkinImageView>, SkinError> {
        // ticket 必须覆盖完整异步导入；否则较早的大图可能在较新的小图之后完成并反向覆盖。
        let ticket = self.begin_selection()?;
        let image = self.importer.import(source).await?;
        self.complete_selection(ticket, image)
    }

    fn begin_selection(&self) -> Result<PreviewRegistrationTicket, SkinError> {
        self.previews.begin_registration()
    }

    fn complete_selection(
        &self,
        ticket: PreviewRegistrationTicket,
        image: SkinImage,
    ) -> Result<Option<SkinImageView>, SkinError> {
        match self.previews.commit_imported(ticket, &image)? {
            SkinPreviewRegistration::Registered => Ok(Some(SkinImageView::try_from(image)?)),
            SkinPreviewRegistration::Superseded => Ok(None),
        }
    }

    /// 保存匹配 revision 的完整草稿，并撤销未提交预览授权。
    ///
    /// :param expected_revision: 调用方最后读取到的 revision。
    /// :param draft: 已类型化的完整皮肤草稿。
    /// :return: revision 严格加一后的已提交快照。
    /// :raises SkinError: revision 冲突、设置越界或持久化失败时保留当前预览并返回错误。
    pub fn save(
        &self,
        expected_revision: u64,
        draft: SkinDraft,
    ) -> Result<SkinStateEnvelope, SkinError> {
        self.save_with_publisher(expected_revision, draft, |_| {})
    }

    /// 串行执行保存与状态发布，保证并发命令不能倒序发送 revision。
    ///
    /// :param expected_revision: 调用方最后读取到的 revision。
    /// :param draft: 已类型化的完整皮肤草稿。
    /// :param publisher: 在串行锁释放前接收已提交快照的发布器。
    /// :return: revision 严格加一后的已提交快照。
    /// :raises SkinError: revision 冲突、持久化或预览撤销失败时返回稳定错误。
    pub(crate) fn save_with_publisher<F>(
        &self,
        expected_revision: u64,
        draft: SkinDraft,
        publisher: F,
    ) -> Result<SkinStateEnvelope, SkinError>
    where
        F: FnOnce(&SkinStateEnvelope),
    {
        let _guard = self.mutation.lock().map_err(|_| SkinError::FileSystem)?;
        let state = self.store.save(expected_revision, draft)?;
        self.previews.clear()?;
        publisher(&state);
        Ok(state)
    }

    /// 恢复默认设置并撤销未提交预览授权，不删除任何托管图片。
    ///
    /// :param expected_revision: 调用方最后读取到的 revision。
    /// :return: revision 严格加一后的默认设置快照。
    /// :raises SkinError: revision 冲突或持久化失败时保留当前预览并返回错误。
    pub fn reset(&self, expected_revision: u64) -> Result<SkinStateEnvelope, SkinError> {
        self.reset_with_publisher(expected_revision, |_| {})
    }

    /// 串行执行恢复默认与状态发布，保证事件 revision 单调递增。
    ///
    /// :param expected_revision: 调用方最后读取到的 revision。
    /// :param publisher: 在串行锁释放前接收默认设置快照的发布器。
    /// :return: revision 严格加一后的默认设置快照。
    /// :raises SkinError: revision 冲突、持久化或预览撤销失败时返回稳定错误。
    pub(crate) fn reset_with_publisher<F>(
        &self,
        expected_revision: u64,
        publisher: F,
    ) -> Result<SkinStateEnvelope, SkinError>
    where
        F: FnOnce(&SkinStateEnvelope),
    {
        let _guard = self.mutation.lock().map_err(|_| SkinError::FileSystem)?;
        let state = self.store.reset(expected_revision)?;
        self.previews.clear()?;
        publisher(&state);
        Ok(state)
    }
}

/// 返回当前已提交的皮肤设置。
///
/// :param controller: Tauri 托管的固定皮肤控制器。
/// :return: 带 revision 的设置快照。
/// :raises SkinCommandError: 设置损坏或固定目录不可访问时返回脱敏错误。
#[tauri::command]
pub fn get_skin_state(
    controller: State<'_, SkinController>,
) -> Result<SkinStateEnvelope, SkinCommandError> {
    controller.load().map_err(Into::into)
}

/// 通过 Windows 原生选择器导入一张皮肤图片。
///
/// :param app: 当前 Tauri 应用句柄，用于打开受限的原生文件选择器。
/// :param controller: Tauri 托管的固定皮肤控制器。
/// :return: 用户取消或选择被更新操作取代时返回 `None`，否则返回脱敏图片视图。
/// :raises SkinCommandError: 图片验证、导入或预览登记失败时返回稳定错误。
#[tauri::command]
pub async fn choose_skin_image(
    app: AppHandle,
    controller: State<'_, SkinController>,
) -> Result<Option<SkinImageView>, SkinCommandError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("图片", &["png", "jpg", "jpeg", "webp"])
        .pick_file(move |selected| {
            // 设置窗口关闭也可能让接收端先结束；此时丢弃结果即可，不能 panic。
            let _ = sender.send(selected);
        });
    let selected = receiver
        .await
        .map_err(|_| SkinCommandError::from(SkinError::Worker))?;
    let Some(path) = selected.and_then(|value| value.into_path().ok()) else {
        return Ok(None);
    };
    controller.import(path).await.map_err(Into::into)
}

/// 保存设置窗口提交的完整皮肤草稿。
///
/// :param app: 当前 Tauri 应用句柄，用于发布最新状态事件。
/// :param controller: Tauri 托管的固定皮肤控制器。
/// :param expected_revision: 调用方最后读取到的 revision。
/// :param draft: 已类型化的完整皮肤草稿。
/// :return: revision 严格加一后的已提交快照。
/// :raises SkinCommandError: revision 冲突、设置越界或持久化失败时返回稳定错误。
#[tauri::command]
pub fn save_skin_settings(
    app: AppHandle,
    controller: State<'_, SkinController>,
    expected_revision: u64,
    draft: SkinDraft,
) -> Result<SkinStateEnvelope, SkinCommandError> {
    let state = controller
        .save_with_publisher(expected_revision, draft, |state| {
            // 事件仅是同一窗口内的同步提示；发送失败不能反向改写已持久化设置。
            let _ = app.emit_to("appearance", SKIN_STATE_EVENT, state);
        })
        .map_err(SkinCommandError::from)?;
    if refresh_main_skin(&app, &state.settings).is_err() {
        crate::record_skin_apply_diagnostic(&app);
    }
    Ok(state)
}

/// 恢复默认皮肤设置，不删除已导入图片。
///
/// :param app: 当前 Tauri 应用句柄，用于发布最新状态事件。
/// :param controller: Tauri 托管的固定皮肤控制器。
/// :param expected_revision: 调用方最后读取到的 revision。
/// :return: revision 严格加一后的默认设置快照。
/// :raises SkinCommandError: revision 冲突或持久化失败时返回稳定错误。
#[tauri::command]
pub fn reset_skin_settings(
    app: AppHandle,
    controller: State<'_, SkinController>,
    expected_revision: u64,
) -> Result<SkinStateEnvelope, SkinCommandError> {
    let state = controller
        .reset_with_publisher(expected_revision, |state| {
            let _ = app.emit_to("appearance", SKIN_STATE_EVENT, state);
        })
        .map_err(SkinCommandError::from)?;
    if refresh_main_skin(&app, &state.settings).is_err() {
        crate::record_skin_apply_diagnostic(&app);
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::{SkinController, SkinImageView};
    use crate::skin::protocol::{SkinPreviewRegistry, SkinProtocol, SkinProtocolAudience};
    use crate::skin::{MaskTone, SkinDraft, SkinErrorKind, SkinFit, SkinPosition, SkinStore};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture(name: &str) -> (SkinController, SkinProtocol, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dsh-desktop-skin-controller-{}-{name}-{nonce}",
            std::process::id()
        ));
        let settings = root.join("settings");
        let skins = root.join("skins");
        fs::create_dir_all(&settings).expect("settings fixture");
        fs::create_dir_all(&skins).expect("skins fixture");
        let store = SkinStore::new(settings, skins.clone());
        let previews = SkinPreviewRegistry::new(skins.clone());
        let controller = SkinController::new(store.clone(), skins.clone(), previews.clone());
        let protocol = SkinProtocol::with_preview_registry(store, skins, previews);
        (controller, protocol, root)
    }

    fn write_png(path: &std::path::Path, color: [u8; 4]) -> u64 {
        image::RgbaImage::from_pixel(2, 1, image::Rgba(color))
            .save(path)
            .expect("PNG fixture");
        fs::metadata(path).expect("PNG metadata").len()
    }

    async fn managed_image(
        root: &std::path::Path,
        name: &str,
        color: [u8; 4],
    ) -> crate::skin::SkinImage {
        let source = root.join(name);
        write_png(&source, color);
        crate::skin::SkinImporter::new(root.join("skins"))
            .import(source)
            .await
            .expect("managed fixture")
    }

    fn immersive_draft(digest: String) -> SkinDraft {
        SkinDraft {
            immersive: true,
            image_digest: Some(digest),
            fit: SkinFit::Cover,
            position: SkinPosition::Center,
            blur_px: 8,
            mask_tone: MaskTone::Light,
            mask_opacity_percent: 22,
            panel_opacity_percent: 88,
        }
    }

    fn default_draft() -> SkinDraft {
        let defaults = crate::skin::SkinSettings::default();
        SkinDraft {
            immersive: defaults.immersive,
            image_digest: defaults.image_digest,
            fit: defaults.fit,
            position: defaults.position,
            blur_px: defaults.blur_px,
            mask_tone: defaults.mask_tone,
            mask_opacity_percent: defaults.mask_opacity_percent,
            panel_opacity_percent: defaults.panel_opacity_percent,
        }
    }

    #[tokio::test]
    async fn imported_view_is_pathless_and_preview_is_appearance_only() {
        let (controller, protocol, root) = fixture("pathless-preview");
        let source = root.join("用户选择的背景.png");
        let byte_size = write_png(&source, [255, 0, 0, 255]);

        let view = controller
            .import(source)
            .await
            .expect("import")
            .expect("latest selection");
        let json = serde_json::to_value(&view).expect("serialize view");
        assert_eq!(json["width"], 2);
        assert_eq!(json["height"], 1);
        assert_eq!(json["bytes"], byte_size);
        assert_eq!(json["protocolUrl"], view.protocol_url);
        assert!(json.get("path").is_none());
        assert!(!json.to_string().contains("用户选择"));

        assert_eq!(
            protocol
                .request_for_audience(&view.protocol_url, SkinProtocolAudience::Appearance)
                .status(),
            200
        );
        assert_eq!(
            protocol
                .request_for_audience(&view.protocol_url, SkinProtocolAudience::Main)
                .status(),
            404
        );
    }

    #[tokio::test]
    async fn successful_save_clears_preview_and_returns_monotonic_state() {
        let (controller, protocol, root) = fixture("save-clears");
        let source = root.join("source.png");
        write_png(&source, [255, 0, 0, 255]);
        let image = controller
            .import(source)
            .await
            .expect("import")
            .expect("latest selection");

        let state = controller
            .save(0, immersive_draft(image.digest.clone()))
            .expect("save");
        assert_eq!(state.revision, 1);
        assert_eq!(controller.load().expect("load"), state);
        assert_eq!(
            protocol
                .request_for_audience(&image.protocol_url, SkinProtocolAudience::Main)
                .status(),
            200
        );

        let reset = controller.reset(1).expect("reset");
        assert_eq!(reset.revision, 2);
        assert!(!reset.settings.immersive);
        assert_eq!(
            protocol
                .request_for_audience(&image.protocol_url, SkinProtocolAudience::Appearance)
                .status(),
            404
        );
    }

    #[tokio::test]
    async fn stale_save_keeps_current_unsaved_preview_authorized() {
        let (controller, protocol, root) = fixture("conflict-keeps-preview");
        let source = root.join("source.png");
        write_png(&source, [255, 0, 0, 255]);
        let image = controller
            .import(source)
            .await
            .expect("import")
            .expect("latest selection");
        controller
            .save(0, immersive_draft(image.digest.clone()))
            .expect("first save");
        let second_source = root.join("second.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 128, 255, 255]))
            .save(&second_source)
            .expect("second source fixture");
        let second = controller
            .import(second_source)
            .await
            .expect("second import")
            .expect("latest selection");

        let error = controller
            .save(0, immersive_draft(second.digest.clone()))
            .expect_err("stale revision must fail");
        assert_eq!(error.kind(), SkinErrorKind::RevisionConflict);
        assert_eq!(
            protocol
                .request_for_audience(&second.protocol_url, SkinProtocolAudience::Appearance)
                .status(),
            200
        );
    }

    #[test]
    fn image_view_type_does_not_offer_a_filesystem_path_field() {
        let view = SkinImageView {
            digest: "a".repeat(64),
            format: crate::skin::SkinFormat::Png,
            width: 1,
            height: 1,
            bytes: 1,
            protocol_url: format!("dsh-skin://localhost/{}", "a".repeat(64)),
        };
        let keys = serde_json::to_value(view)
            .expect("serialize")
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            [
                "bytes",
                "digest",
                "format",
                "height",
                "protocolUrl",
                "width"
            ]
        );
    }

    #[test]
    fn invalid_import_metadata_cannot_panic_while_building_an_ipc_view() {
        let image = crate::skin::SkinImage {
            digest: "A".repeat(64),
            format: crate::skin::SkinFormat::Png,
            width: 1,
            height: 1,
            byte_size: 1,
            path: PathBuf::new(),
        };

        assert!(SkinImageView::try_from(image).is_err());
    }

    #[test]
    fn state_publishers_cannot_reorder_monotonic_revisions() {
        let (controller, _, _) = fixture("monotonic-events");
        let first_publisher_started = Arc::new(Barrier::new(2));
        let release_first_publisher = Arc::new(Barrier::new(2));
        let published = Arc::new(Mutex::new(Vec::new()));

        let first_controller = controller.clone();
        let first_started = first_publisher_started.clone();
        let first_release = release_first_publisher.clone();
        let first_published = published.clone();
        let first = std::thread::spawn(move || {
            first_controller
                .save_with_publisher(0, default_draft(), |state| {
                    first_started.wait();
                    first_release.wait();
                    first_published
                        .lock()
                        .expect("published lock")
                        .push(state.revision);
                })
                .expect("first save")
        });

        first_publisher_started.wait();
        let second_controller = controller.clone();
        let second_published = published.clone();
        let second = std::thread::spawn(move || {
            second_controller
                .save_with_publisher(1, default_draft(), |state| {
                    second_published
                        .lock()
                        .expect("published lock")
                        .push(state.revision);
                })
                .expect("second save")
        });
        release_first_publisher.wait();

        first.join().expect("first worker");
        second.join().expect("second worker");
        assert_eq!(*published.lock().expect("published lock"), [1, 2]);
    }

    #[tokio::test]
    async fn newer_fast_selection_supersedes_an_older_slow_selection() {
        let (controller, _, root) = fixture("selection-order");
        let old_image = managed_image(&root, "old.png", [255, 0, 0, 255]).await;
        let new_image = managed_image(&root, "new.png", [0, 128, 255, 255]).await;
        let old_ticket = controller.begin_selection().expect("old ticket");
        let old_waiting = Arc::new(Barrier::new(2));
        let release_old = Arc::new(Barrier::new(2));
        let worker_controller = controller.clone();
        let worker_waiting = old_waiting.clone();
        let worker_release = release_old.clone();
        let old = std::thread::spawn(move || {
            worker_waiting.wait();
            worker_release.wait();
            worker_controller
                .complete_selection(old_ticket, old_image)
                .expect("old completion")
        });

        old_waiting.wait();
        let new_ticket = controller.begin_selection().expect("new ticket");
        let new_view = controller
            .complete_selection(new_ticket, new_image)
            .expect("new completion");
        assert!(new_view.is_some());
        release_old.wait();
        assert!(old.join().expect("old worker").is_none());
    }

    #[tokio::test]
    async fn save_invalidates_a_selection_still_inside_importer() {
        let (controller, _, root) = fixture("save-vs-import");
        let image = managed_image(&root, "pending.png", [255, 0, 0, 255]).await;
        let ticket = controller.begin_selection().expect("selection ticket");
        let importer_waiting = Arc::new(Barrier::new(2));
        let release_importer = Arc::new(Barrier::new(2));
        let worker_controller = controller.clone();
        let worker_waiting = importer_waiting.clone();
        let worker_release = release_importer.clone();
        let worker = std::thread::spawn(move || {
            worker_waiting.wait();
            worker_release.wait();
            worker_controller
                .complete_selection(ticket, image)
                .expect("stable completion")
        });

        importer_waiting.wait();
        controller
            .save(0, default_draft())
            .expect("save clears pending selection");
        release_importer.wait();
        assert!(worker.join().expect("import worker").is_none());
    }

    #[tokio::test]
    async fn reset_invalidates_a_selection_still_inside_importer() {
        let (controller, _, root) = fixture("reset-vs-import");
        let image = managed_image(&root, "pending.png", [255, 0, 0, 255]).await;
        let ticket = controller.begin_selection().expect("selection ticket");
        let importer_waiting = Arc::new(Barrier::new(2));
        let release_importer = Arc::new(Barrier::new(2));
        let worker_controller = controller.clone();
        let worker_waiting = importer_waiting.clone();
        let worker_release = release_importer.clone();
        let worker = std::thread::spawn(move || {
            worker_waiting.wait();
            worker_release.wait();
            worker_controller
                .complete_selection(ticket, image)
                .expect("stable completion")
        });

        importer_waiting.wait();
        controller.reset(0).expect("reset clears pending selection");
        release_importer.wait();
        assert!(worker.join().expect("import worker").is_none());
    }
}
