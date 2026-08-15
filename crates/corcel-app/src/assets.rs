//! Embedded static assets, served to GPUI through [`AssetSource`] (registered
//! via `Application::with_assets` in `main`). Currently just the Lucide icon
//! set (ISC license) that [`crate::theme::icon`] renders — GPUI rasterizes
//! each SVG and tints it with the element's text color, so every icon file
//! here must be a monochrome `currentColor` icon.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

/// Icon asset paths, kept as constants so a typo'd path is a compile error at
/// the call site instead of a silently blank icon at runtime.
pub mod icons {
    pub const HASH: &str = "icons/hash.svg";
    pub const LINK: &str = "icons/link.svg";
    pub const LOG_OUT: &str = "icons/log-out.svg";
    pub const MIC: &str = "icons/mic.svg";
    pub const MIC_OFF: &str = "icons/mic-off.svg";
    pub const MONITOR_UP: &str = "icons/monitor-up.svg";
    pub const PENCIL: &str = "icons/pencil.svg";
    pub const PHONE: &str = "icons/phone.svg";
    pub const PLUS: &str = "icons/plus.svg";
    pub const REPLY: &str = "icons/reply.svg";
    pub const SMILE_PLUS: &str = "icons/smile-plus.svg";
    pub const TRASH: &str = "icons/trash-2.svg";
    pub const USER: &str = "icons/user.svg";
    pub const USERS: &str = "icons/users.svg";
    pub const VIDEO: &str = "icons/video.svg";
    pub const VIDEO_OFF: &str = "icons/video-off.svg";
    pub const VOLUME: &str = "icons/volume-2.svg";
    pub const X: &str = "icons/x.svg";
}

const ASSETS: &[(&str, &[u8])] = &[
    (icons::HASH, include_bytes!("../assets/icons/hash.svg")),
    (icons::LINK, include_bytes!("../assets/icons/link.svg")),
    (icons::LOG_OUT, include_bytes!("../assets/icons/log-out.svg")),
    (icons::MIC, include_bytes!("../assets/icons/mic.svg")),
    (icons::MIC_OFF, include_bytes!("../assets/icons/mic-off.svg")),
    (icons::MONITOR_UP, include_bytes!("../assets/icons/monitor-up.svg")),
    (icons::PENCIL, include_bytes!("../assets/icons/pencil.svg")),
    (icons::PHONE, include_bytes!("../assets/icons/phone.svg")),
    (icons::PLUS, include_bytes!("../assets/icons/plus.svg")),
    (icons::REPLY, include_bytes!("../assets/icons/reply.svg")),
    (icons::SMILE_PLUS, include_bytes!("../assets/icons/smile-plus.svg")),
    (icons::TRASH, include_bytes!("../assets/icons/trash-2.svg")),
    (icons::USER, include_bytes!("../assets/icons/user.svg")),
    (icons::USERS, include_bytes!("../assets/icons/users.svg")),
    (icons::VIDEO, include_bytes!("../assets/icons/video.svg")),
    (icons::VIDEO_OFF, include_bytes!("../assets/icons/video-off.svg")),
    (icons::VOLUME, include_bytes!("../assets/icons/volume-2.svg")),
    (icons::X, include_bytes!("../assets/icons/x.svg")),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS.iter().find(|(name, _)| *name == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ASSETS.iter().filter(|(name, _)| name.starts_with(path)).map(|(name, _)| (*name).into()).collect())
    }
}
