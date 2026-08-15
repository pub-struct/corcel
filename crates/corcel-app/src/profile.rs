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

/// Where replicated peer avatars live: one PNG per author, named
/// `<hex(author)>-<content hash>.png`. Hex keeps arbitrary display names
/// filesystem-safe; the content hash in the name means an updated avatar
/// gets a *new* path, so GPUI's by-path image cache can't pin the stale
/// one.
fn avatars_dir() -> PathBuf {
    config_dir().join("avatars")
}

/// Longest side of the avatar we put on the wire — plenty for the 20-80px
/// circles it renders in, and it keeps the base64 payload to a few tens
/// of KB instead of megabytes of camera photo.
const AVATAR_WIRE_SIZE: u32 = 256;

/// Decoded-size cap on incoming avatars, so a malicious/buggy peer can't
/// make us buffer an arbitrarily large blob.
const AVATAR_MAX_BYTES: usize = 1024 * 1024;

/// Loads this user's avatar file, downscales it to
/// [`AVATAR_WIRE_SIZE`], and returns it as base64 PNG — the payload of
/// our `ChatPayload::Profile`. `None` if the file is gone or unreadable
/// as an image.
pub fn encode_avatar(path: &std::path::Path) -> Option<String> {
    use base64::Engine;
    let image = image::open(path).ok()?;
    let thumb = image.thumbnail(AVATAR_WIRE_SIZE, AVATAR_WIRE_SIZE);
    let mut png = Vec::new();
    thumb
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(png))
}

/// Stores a peer's base64 avatar on disk and returns its path — or `None`
/// if it's oversized, not valid base64, or not decodable as an image
/// (re-encoded to PNG, so what lands on disk is something *we* rendered,
/// never a peer's raw bytes). Older files for the same author are swept
/// so the directory doesn't accumulate every avatar they ever had.
pub fn save_peer_avatar(author: &str, avatar_base64: &str) -> Option<PathBuf> {
    use base64::Engine;
    use std::hash::{Hash, Hasher};

    if avatar_base64.len() > AVATAR_MAX_BYTES * 4 / 3 + 4 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(avatar_base64).ok()?;
    if bytes.len() > AVATAR_MAX_BYTES {
        return None;
    }
    let image = image::load_from_memory(&bytes).ok()?;
    let image = image.thumbnail(AVATAR_WIRE_SIZE, AVATAR_WIRE_SIZE);
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;

    let author_hex = hex_encode(author.as_bytes());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    png.hash(&mut hasher);
    let path = avatars_dir().join(format!("{author_hex}-{:016x}.png", hasher.finish()));
    if path.exists() {
        return Some(path); // same author, same content — nothing to write
    }

    std::fs::create_dir_all(avatars_dir()).ok()?;
    std::fs::write(&path, &png).ok()?;
    // Sweep this author's previous avatars now that the new one is down.
    if let Ok(entries) = std::fs::read_dir(avatars_dir()) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path != path
                && entry_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.split_once('-'))
                    .is_some_and(|(hex, _)| hex == author_hex)
            {
                let _ = std::fs::remove_file(entry_path);
            }
        }
    }
    Some(path)
}

/// Deletes every cached avatar file for an author — how a peer's "I
/// removed my photo" propagates to this machine's disk cache.
pub fn remove_peer_avatar(author: &str) {
    let author_hex = hex_encode(author.as_bytes());
    let Ok(entries) = std::fs::read_dir(avatars_dir()) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.split_once('-'))
            .is_some_and(|(hex, _)| hex == author_hex)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Every peer avatar already on disk, keyed by author name — what seeds
/// the shell's avatar map at startup, so replicated avatars survive a
/// restart without waiting for the peers to come online again.
pub fn load_peer_avatars() -> std::collections::HashMap<String, PathBuf> {
    let mut avatars = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(avatars_dir()) else {
        return avatars;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(author) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.split_once('-'))
            .and_then(|(hex, _)| hex_decode(hex))
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            continue;
        };
        avatars.insert(author, path);
    }
    avatars
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// The full replication round trip on a temp config dir: encode a
    /// local avatar file, absorb it as a peer's, reload the cache from
    /// disk, and confirm an updated avatar replaces (not accumulates
    /// next to) the old one.
    #[test]
    fn avatar_round_trip() {
        let dir = std::env::temp_dir().join(format!("corcel-avatar-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // config_dir() honors XDG_CONFIG_HOME on every OS.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };

        let source = dir.join("face.png");
        image::DynamicImage::new_rgba8(512, 300).save(&source).unwrap();
        let encoded = encode_avatar(&source).expect("encodable");
        // The wire copy really was downscaled to the 256 box.
        let wire = base64::engine::general_purpose::STANDARD.decode(&encoded).unwrap();
        assert!(image::load_from_memory(&wire).unwrap().width() <= AVATAR_WIRE_SIZE);

        let path = save_peer_avatar("zé do caixão", &encoded).expect("savable");
        assert!(path.is_file());
        assert_eq!(load_peer_avatars().get("zé do caixão"), Some(&path));

        // A different avatar for the same author lands at a new path and
        // sweeps the old file.
        let source2 = dir.join("face2.png");
        image::DynamicImage::new_rgba8(64, 64).save(&source2).unwrap();
        let encoded2 = encode_avatar(&source2).unwrap();
        let path2 = save_peer_avatar("zé do caixão", &encoded2).expect("savable");
        assert_ne!(path, path2);
        assert!(!path.exists());
        assert_eq!(load_peer_avatars().get("zé do caixão"), Some(&path2));

        // Garbage and oversized payloads are rejected, not written.
        assert!(save_peer_avatar("x", "not base64!!!").is_none());
        let huge = base64::engine::general_purpose::STANDARD.encode(vec![0u8; AVATAR_MAX_BYTES + 1]);
        assert!(save_peer_avatar("x", &huge).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
