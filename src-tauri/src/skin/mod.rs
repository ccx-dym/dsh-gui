//! 沉浸式皮肤的类型化设置与持久化边界。

mod model;
mod store;

pub use model::{
    MaskTone, SkinDraft, SkinFit, SkinFormat, SkinImage, SkinPosition, SkinSettings,
    SkinStateEnvelope,
};
pub use store::{SkinError, SkinErrorKind, SkinStore};
