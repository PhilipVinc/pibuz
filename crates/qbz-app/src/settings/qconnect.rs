//! The `[qconnect]` section of `settings.toml`: this renderer's Connect
//! identity and options.
//!
//! The values are kept as the CLI spells them (`startup_mode = "on"`), and
//! `pibuz::qconnect::transport` turns them into types when it reads them, so a
//! value this version does not know reads as the default instead of failing
//! the whole section.

use qbz_models::settings_file;
use serde::{Deserialize, Serialize};
use std::path::Path;

const SECTION: &str = "qconnect";

/// Default port for the pairing listener — one above the control plane's 8182.
pub const DEFAULT_PAIRING_PORT: u16 = 8183;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QconnectSettings {
    /// How the cloud and every phone know this renderer. Minted on first use
    /// by [`device_uuid`] and never changed after: a new one makes the Pi a
    /// new device in every Qobuz app. Never exported in a settings bundle.
    pub device_uuid: Option<String>,
    /// The name shown in the Qobuz app; unset derives one from the hostname.
    pub device_name: Option<String>,
    /// `off` | `on`.
    pub startup_mode: String,
    /// `software` | `locked` | `external`; unset is `software`.
    pub volume_mode: Option<String>,
    /// The icon the Qobuz apps draw next to the name, as the protocol's
    /// DeviceType names it in lowercase (`streamer`, `speaker`, ...); unset is
    /// `streamer`.
    pub device_type: Option<String>,
    /// Percent to set when a session picks this device; `0` keeps the
    /// player's volume.
    pub initial_volume: u8,
    /// Whether to advertise the local pairing surface.
    pub pairing: bool,
    pub pairing_port: u16,
}

impl Default for QconnectSettings {
    fn default() -> Self {
        Self {
            device_uuid: None,
            device_name: None,
            startup_mode: "off".to_string(),
            volume_mode: None,
            device_type: None,
            initial_volume: 0,
            pairing: true,
            pairing_port: DEFAULT_PAIRING_PORT,
        }
    }
}

/// The current settings, or the defaults when none were ever written.
/// Folds in a `qconnect_settings.db` from before `settings.toml` first.
pub fn load(root: &Path) -> QconnectSettings {
    legacy::migrate(root);
    settings_file::read_section(root, SECTION)
}

/// Whether anything has ever written Connect settings here.
pub(crate) fn exists(root: &Path) -> bool {
    legacy::migrate(root);
    settings_file::has_section(root, SECTION)
}

/// Change the settings under the file lock.
pub fn update<R>(root: &Path, f: impl FnOnce(&mut QconnectSettings) -> R) -> Result<R, String> {
    legacy::migrate(root);
    settings_file::update_section(root, SECTION, f)
}

/// This renderer's device uuid, minted and saved the first time it is asked
/// for. Under the file lock, so two processes asking at once agree.
///
/// Falls back to a fresh, unsaved uuid only when the file cannot be written:
/// the renderer still connects, as a new device this once.
pub fn device_uuid(root: &Path) -> String {
    let minted = update(root, |s| match s.device_uuid.as_deref().map(str::trim) {
        Some(uuid) if !uuid.is_empty() => uuid.to_string(),
        _ => {
            let uuid = uuid::Uuid::new_v4().to_string();
            s.device_uuid = Some(uuid.clone());
            uuid
        }
    });
    minted.unwrap_or_else(|e| {
        log::error!("[qconnect] device uuid not saved, connecting as a new device: {e}");
        uuid::Uuid::new_v4().to_string()
    })
}

/// `qconnect_settings.db`, the key/value store before `settings.toml`. Read
/// once, then deleted.
#[cfg(feature = "sqlite-migrate")]
mod legacy {
    use super::*;

    const DB_NAME: &str = "qconnect_settings.db";

    pub(super) fn migrate(root: &Path) {
        settings_file::migrate_legacy(root, &root.join(DB_NAME), SECTION, |db, sections| {
            sections.set(SECTION, &read(db)?)
        });
    }

    fn read(db: &Path) -> Result<QconnectSettings, String> {
        // Read-write: a read-only open cannot replay a WAL a crashed writer
        // left behind.
        let conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT key, value FROM settings")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        let mut s = QconnectSettings::default();
        for (key, value) in rows {
            let value = value.trim().to_string();
            let text = || Some(value.clone()).filter(|v| !v.is_empty());
            match key.as_str() {
                "device_uuid" => s.device_uuid = text(),
                "device_name" => s.device_name = text(),
                "startup_mode" => s.startup_mode = value.clone(),
                "volume_mode" => s.volume_mode = text(),
                "initial_volume" => s.initial_volume = value.parse().unwrap_or(0),
                "pairing" => s.pairing = value != "off",
                "pairing_port" => {
                    s.pairing_port = value
                        .parse()
                        .ok()
                        .filter(|p| *p != 0)
                        .unwrap_or(DEFAULT_PAIRING_PORT)
                }
                _ => {}
            }
        }
        Ok(s)
    }
}

/// Without `sqlite-migrate` an old `qconnect_settings.db` cannot be read. Say
/// so, and leave it for a build that can.
#[cfg(not(feature = "sqlite-migrate"))]
mod legacy {
    use super::*;

    pub(super) fn migrate(root: &Path) {
        let db = root.join("qconnect_settings.db");
        if db.exists() && !settings_file::has_section(root, SECTION) {
            log::warn!(
                "[settings] {} is from an older pibuz and this build cannot read it \
                 (built without sqlite-migrate); this renderer will appear as a new device",
                db.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "qbz-app-qconnect-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn device_uuid_is_minted_once_and_then_stable() {
        let root = scratch("uuid");
        let first = device_uuid(&root);
        assert!(!first.trim().is_empty());
        assert_eq!(device_uuid(&root), first, "read back, not re-minted");
        assert_eq!(load(&root).device_uuid.as_deref(), Some(first.as_str()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The daemon and a `settings set` can both be the first to ask. Two
    /// different uuids would make one of them a device the other never saved.
    #[test]
    fn concurrent_first_uses_agree_on_one_uuid() {
        let root = scratch("uuid-race");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || device_uuid(&root))
            })
            .collect();
        let uuids: std::collections::HashSet<String> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(uuids.len(), 1, "{uuids:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_startup_mode_does_not_fail_the_section() {
        let root = scratch("unknown-mode");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            settings_file::path(&root),
            "[qconnect]\nstartup_mode = \"remember_last\"\ndevice_name = \"Den\"\n",
        )
        .unwrap();
        let s = load(&root);
        assert_eq!(s.startup_mode, "remember_last");
        assert_eq!(s.device_name.as_deref(), Some("Den"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "sqlite-migrate")]
    #[test]
    fn a_legacy_database_is_migrated_with_its_identity() {
        let root = scratch("legacy");
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("qconnect_settings.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO settings VALUES
                    ('device_uuid', '6f1c1e9a-0000-4000-8000-000000000001'),
                    ('device_name', 'Kitchen'),
                    ('startup_mode', 'on'),
                    ('volume_mode', 'locked'),
                    ('initial_volume', '30'),
                    ('pairing', 'off'),
                    ('pairing_port', '9000'),
                    ('last_known_state', 'connected');",
            )
            .unwrap();
        }

        let s = load(&root);
        assert_eq!(
            s.device_uuid.as_deref(),
            Some("6f1c1e9a-0000-4000-8000-000000000001"),
            "the identity is the one thing that must survive"
        );
        assert_eq!(device_uuid(&root), "6f1c1e9a-0000-4000-8000-000000000001");
        assert_eq!(s.device_name.as_deref(), Some("Kitchen"));
        assert_eq!(s.startup_mode, "on");
        assert_eq!(s.volume_mode.as_deref(), Some("locked"));
        assert_eq!(s.initial_volume, 30);
        assert!(!s.pairing);
        assert_eq!(s.pairing_port, 9000);
        assert!(!db.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
