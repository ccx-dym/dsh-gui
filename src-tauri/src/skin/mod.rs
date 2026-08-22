//! 沉浸式皮肤的类型化设置与持久化边界。

mod import;
mod model;
pub(crate) mod protocol;
mod store;

pub use import::{MAX_SKIN_BYTES, MAX_SKIN_EDGE, MAX_SKIN_PIXELS, SkinImporter};
pub use model::{
    MaskTone, SkinDraft, SkinFit, SkinFormat, SkinImage, SkinPosition, SkinSettings,
    SkinStateEnvelope,
};
pub use protocol::{SkinProtocol, skin_resource_url};
pub use store::{SkinError, SkinErrorKind, SkinStore};
