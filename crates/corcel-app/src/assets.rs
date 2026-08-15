//! Embedded static assets, served to GPUI through [`AssetSource`] (registered
//! via `Application::with_assets` in `main`). The Nucleo Glass icon set (plus corcel-authored
//! matches for its gaps) that [`crate::theme::icon`] renders — GPUI rasterizes
//! each SVG and tints it with the element's text color, so every icon file
//! here must be a monochrome `currentColor` icon.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

/// Icon asset paths, kept as constants so a typo'd path is a compile error at
/// the call site instead of a silently blank icon at runtime.
/// Everything points at the Nucleo Glass set (shared free by Nucleo —
/// thanks!). The glyphs Glass doesn't ship (mic, plus, trash, …) are
/// corcel-authored in the same layered gradient+mask idiom, so the set
/// reads as one family; see `assets/icons/glass/`.
pub mod icons {
    pub const HASH: &str = "icons/glass/hash.svg";
    pub const HEADPHONE_OFF: &str = "icons/glass/headphones-off.svg";
    pub const HEADPHONES: &str = "icons/glass/headphones.svg";
    pub const LINK: &str = "icons/glass/link.svg";
    pub const LOG_OUT: &str = "icons/glass/circle-power-off.svg";
    pub const MIC: &str = "icons/glass/mic.svg";
    pub const MIC_OFF: &str = "icons/glass/mic-off.svg";
    pub const MONITOR_UP: &str = "icons/glass/monitor.svg";
    pub const PAUSE: &str = "icons/glass/pause.svg";
    pub const PENCIL: &str = "icons/glass/pen.svg";
    pub const PHONE: &str = "icons/glass/phone.svg";
    pub const PLAY: &str = "icons/glass/play.svg";
    pub const PLUS: &str = "icons/glass/plus.svg";
    pub const REPLY: &str = "icons/glass/reply.svg";
    pub const MESSAGE_SQUARE: &str = "icons/glass/msgs.svg";
    pub const SMILE_PLUS: &str = "icons/glass/face-grin.svg";
    pub const TRASH: &str = "icons/glass/trash.svg";
    pub const USER: &str = "icons/glass/user.svg";
    pub const USERS: &str = "icons/glass/users.svg";
    pub const VIDEO: &str = "icons/glass/video.svg";
    pub const VIDEO_OFF: &str = "icons/glass/video-off.svg";
    pub const VOLUME: &str = "icons/glass/volume.svg";
    pub const X: &str = "icons/glass/delete-x.svg";
}

const ASSETS: &[(&str, &[u8])] = &[
    (icons::HASH, include_bytes!("../assets/icons/glass/hash.svg")),
    (icons::HEADPHONE_OFF, include_bytes!("../assets/icons/glass/headphones-off.svg")),
    (icons::HEADPHONES, include_bytes!("../assets/icons/glass/headphones.svg")),
    (icons::LINK, include_bytes!("../assets/icons/glass/link.svg")),
    (icons::LOG_OUT, include_bytes!("../assets/icons/glass/circle-power-off.svg")),
    (icons::MIC, include_bytes!("../assets/icons/glass/mic.svg")),
    (icons::MIC_OFF, include_bytes!("../assets/icons/glass/mic-off.svg")),
    (icons::MONITOR_UP, include_bytes!("../assets/icons/glass/monitor.svg")),
    (icons::PAUSE, include_bytes!("../assets/icons/glass/pause.svg")),
    (icons::PENCIL, include_bytes!("../assets/icons/glass/pen.svg")),
    (icons::PHONE, include_bytes!("../assets/icons/glass/phone.svg")),
    (icons::PLAY, include_bytes!("../assets/icons/glass/play.svg")),
    (icons::PLUS, include_bytes!("../assets/icons/glass/plus.svg")),
    (icons::REPLY, include_bytes!("../assets/icons/glass/reply.svg")),
    (icons::MESSAGE_SQUARE, include_bytes!("../assets/icons/glass/msgs.svg")),
    (icons::SMILE_PLUS, include_bytes!("../assets/icons/glass/face-grin.svg")),
    (icons::TRASH, include_bytes!("../assets/icons/glass/trash.svg")),
    (icons::USER, include_bytes!("../assets/icons/glass/user.svg")),
    (icons::USERS, include_bytes!("../assets/icons/glass/users.svg")),
    (icons::VIDEO, include_bytes!("../assets/icons/glass/video.svg")),
    (icons::VIDEO_OFF, include_bytes!("../assets/icons/glass/video-off.svg")),
    (icons::VOLUME, include_bytes!("../assets/icons/glass/volume.svg")),
    (icons::X, include_bytes!("../assets/icons/glass/delete-x.svg")),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS.iter().find(|(name, _)| *name == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ASSETS.iter().filter(|(name, _)| name.starts_with(path)).map(|(name, _)| (*name).into()).collect())
    }
}
