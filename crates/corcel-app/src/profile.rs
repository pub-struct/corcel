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

/// `$XDG_CONFIG_HOME/corcel`, falling back to `$HOME/.config/corcel`
/// (`%APPDATA%\corcel` on Windows, which has no `$HOME`), then just
/// `./corcel` if nothing is set. Everything the app persists —
/// `profile.json` and the database — lives in this one directory so the
/// user can find, back up, or delete it all in one place. The XDG
/// override is honored everywhere, so tests can redirect it on any OS.
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let fallback = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(target_os = "windows"))]
    let fallback = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"));
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or(fallback)
        .unwrap_or_else(|| PathBuf::from("."));
    config_home.join("corcel")
}

/// One-time adoption of a pre-rename config directory: the app used to be
/// called "freecord", and machines that ran it have `…/freecord/` with a
/// profile and database inside. Renames it (and the database file within)
/// so nobody loses their servers or history to the rename. Runs before
/// anything opens files; all failures are ignored — worst case the user
/// starts fresh, same as any new install.
pub fn migrate_legacy_config_dir() {
    let new_dir = config_dir();
    let Some(old_dir) = new_dir.parent().map(|parent| parent.join("freecord")) else { return };
    if !new_dir.exists() && old_dir.is_dir() {
        let _ = std::fs::rename(&old_dir, &new_dir);
    }
    let old_db = new_dir.join("freecord.db");
    let new_db = new_dir.join("corcel.db");
    if !new_db.exists() && old_db.is_file() {
        let _ = std::fs::rename(&old_db, &new_db);
    }
}

fn config_path() -> PathBuf {
    config_dir().join("profile.json")
}
