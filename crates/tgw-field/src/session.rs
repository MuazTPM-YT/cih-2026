//! Persisted paired session: the derived key and the hospital's public address, written once
//! by `tgw-field pair` and read by the send path. Replaces the hand-typed key file for the
//! cross-LAN mode. The key is stored hex-encoded in a `0600` file, never logged.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tgw_core::Key;

/// A paired session: where the hospital is, and the key agreed via SPAKE2.
pub struct Session {
    /// Hospital public UDP address to dial.
    pub hospital_addr: String,
    /// The derived per-session key.
    pub key: Key,
}

#[derive(Serialize, Deserialize)]
struct OnDisk {
    hospital_addr: String,
    key: String,
}

/// Default session-file path; `TGW_SESSION_PATH` overrides (tests/multi-device).
#[must_use]
pub fn default_path() -> PathBuf {
    std::env::var("TGW_SESSION_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("field-session.json"))
}

/// Persist `session` to `path` as `0600` JSON.
pub fn save(path: &Path, session: &Session) -> Result<()> {
    let on_disk = OnDisk {
        hospital_addr: session.hospital_addr.clone(),
        key: session.key.to_hex(),
    };
    let json = serde_json::to_string(&on_disk).context("serialize session")?;
    std::fs::write(path, json).with_context(|| format!("write session {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

/// Load a session, or `Ok(None)` if the file does not exist.
pub fn load(path: &Path) -> Result<Option<Session>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read session {}", path.display())),
    };
    let on_disk: OnDisk = serde_json::from_str(&text).context("parse session")?;
    let key = Key::from_hex(&on_disk.key).context("session key")?;
    Ok(Some(Session {
        hospital_addr: on_disk.hospital_addr,
        key,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_absent_is_none() {
        let dir = std::env::temp_dir().join(format!("tgw-sess-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("s.json");
        assert!(load(&path).expect("load absent").is_none(), "absent ⇒ None");
        let key = tgw_core::Key::generate();
        save(
            &path,
            &Session {
                hospital_addr: "203.0.113.5:47000".into(),
                key: key.clone(),
            },
        )
        .expect("save");
        let got = load(&path).expect("load").expect("some");
        assert_eq!(got.hospital_addr, "203.0.113.5:47000");
        assert_eq!(got.key.to_hex(), key.to_hex());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
