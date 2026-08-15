//! The local user's profile: a display name (required) plus an optional
//! avatar, banner, and bio, set up once on first launch (see
//! [`crate::Shell`]'s onboarding gate) and persisted to disk so it never
//! runs again on the same machine.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub avatar_path: Option<PathBuf>,
    pub banner_path: Option<PathBuf>,
    pub bio: Option<String>,
}

impl Profile {
    /// The single uppercase letter shown wherever there's no avatar image to
    /// render instead.
    pub fn initial(&self) -> String {
        self.name
            .trim()
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string())
    }

    pub fn load() -> Option<Self> {
        let bytes = std::fs::read(config_path()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// `$XDG_CONFIG_HOME/freecord/profile.json`, falling back to
/// `$HOME/.config/freecord/profile.json`, then just `./freecord/profile.json`
/// if neither is set.
fn config_path() -> PathBuf {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    config_home.join("freecord").join("profile.json")
}
