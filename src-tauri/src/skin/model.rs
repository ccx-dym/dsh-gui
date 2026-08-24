use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 背景图片填充方式。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinFit {
    #[default]
    Cover,
    Contain,
    Stretch,
    Center,
}

/// 背景图片在九宫格中的锚点。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinPosition {
    TopLeft,
    Top,
    TopRight,
    Left,
    #[default]
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// 用于保证正文可读性的遮罩色调。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskTone {
    #[default]
    Light,
    Dark,
}

/// 托管图片的真实编码格式。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinFormat {
    Png,
    Jpeg,
    Webp,
}

impl SkinFormat {
    /// 返回托管图片使用的规范扩展名。
    ///
    /// :return: 不含点号的小写扩展名。
    /// :raises: 枚举值已封闭，此函数不产生错误。
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }
}

/// 已复制到应用目录的不可变图片元数据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkinImage {
    pub digest: String,
    pub format: SkinFormat,
    pub width: u32,
    pub height: u32,
    pub byte_size: u64,
    pub path: PathBuf,
}

/// 设置窗口尚未提交的皮肤草稿。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkinDraft {
    pub immersive: bool,
    pub image_digest: Option<String>,
    pub fit: SkinFit,
    pub position: SkinPosition,
    pub blur_px: u8,
    pub mask_tone: MaskTone,
    pub mask_opacity_percent: u8,
    /// schema 1 兼容字段；当前语义为背景图片不透明度百分比。
    pub panel_opacity_percent: u8,
}

/// 已提交且可注入主窗口的皮肤设置。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkinSettings {
    pub immersive: bool,
    pub image_digest: Option<String>,
    pub fit: SkinFit,
    pub position: SkinPosition,
    pub blur_px: u8,
    pub mask_tone: MaskTone,
    pub mask_opacity_percent: u8,
    /// schema 1 兼容字段；当前语义为背景图片不透明度百分比。
    pub panel_opacity_percent: u8,
}

impl Default for SkinSettings {
    fn default() -> Self {
        Self {
            immersive: false,
            image_digest: None,
            fit: SkinFit::Cover,
            position: SkinPosition::Center,
            blur_px: 0,
            mask_tone: MaskTone::Light,
            mask_opacity_percent: 22,
            panel_opacity_percent: 88,
        }
    }
}

impl From<SkinDraft> for SkinSettings {
    fn from(value: SkinDraft) -> Self {
        Self {
            immersive: value.immersive,
            image_digest: value.image_digest,
            fit: value.fit,
            position: value.position,
            blur_px: value.blur_px,
            mask_tone: value.mask_tone,
            mask_opacity_percent: value.mask_opacity_percent,
            panel_opacity_percent: value.panel_opacity_percent,
        }
    }
}

/// 携带单调 revision 的设置快照。
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkinStateEnvelope {
    pub revision: u64,
    pub settings: SkinSettings,
}
