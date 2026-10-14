// crates/qbz-app/src/settings/daemon_prefs.rs
//! The daemon's player prefs (01-architecture.md §10.3). Lives in qbz-app so
//! pibuz AND settings::bundle share one struct (D2).
//!
//! Stored as two sections of `settings.toml`, named after the CLI keys that
//! set them: `[playback]` holds `quality` and `mpris`, `[hooks]` holds
//! `script`. They used to be `daemon_prefs.json`, which is folded in and
//! deleted on first use.
use qbz_models::settings_file::{self, Sections};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonPrefs {
    /// "mp3" | "cd" | "hires" | "hires_plus".
    pub streaming_quality: String,
    /// Whether the daemon publishes MPRIS system media controls at boot
    /// (CONSOLE ext). Default ON; the `QBZD_MPRIS` env var overrides it when
    /// set. Toggling this needs a daemon restart (the service spawns at boot).
    pub mpris_enabled: bool,
    /// Absolute path of an executable the daemon runs on playback/session
    /// events (CONSOLE ext), with the event in `QBZ_*` environment variables.
    /// Empty = hooks off (the default). The `QBZD_HOOK` env var overrides it
    /// when set. Changing it needs a daemon restart (the dispatcher spawns at
    /// boot). Machine-local by design — NEVER part of the settings bundle.
    pub hook_script: String,
}
impl Default for DaemonPrefs {
    fn default() -> Self {
        Self {
            streaming_quality: "hires_plus".into(),
            mpris_enabled: true,
            hook_script: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct PlaybackSection {
    quality: String,
    mpris: bool,
}

impl Default for PlaybackSection {
    fn default() -> Self {
        let d = DaemonPrefs::default();
        Self {
            quality: d.streaming_quality,
            mpris: d.mpris_enabled,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct HooksSection {
    script: String,
}

const PLAYBACK: &str = "playback";
const HOOKS: &str = "hooks";
const LEGACY_JSON: &str = "daemon_prefs.json";

/// The prefs, or their defaults when none were ever written.
pub fn load_at(data_root: &Path) -> DaemonPrefs {
    migrate(data_root);
    match settings_file::load(data_root).and_then(|s| from_sections(&s)) {
        Ok(prefs) => prefs,
        Err(e) => {
            log::error!("[settings] {e}; using default player prefs");
            DaemonPrefs::default()
        }
    }
}

/// Change the prefs under the file lock, so a `settings set` and the daemon
/// writing at once cannot drop each other's change.
pub fn update_at(data_root: &Path, f: impl FnOnce(&mut DaemonPrefs)) -> Result<(), String> {
    migrate(data_root);
    settings_file::update(data_root, |sections| {
        let mut prefs = from_sections(sections)?;
        f(&mut prefs);
        to_sections(sections, &prefs)
    })
}

fn from_sections(sections: &Sections) -> Result<DaemonPrefs, String> {
    let playback: PlaybackSection = sections.get(PLAYBACK)?;
    let hooks: HooksSection = sections.get(HOOKS)?;
    Ok(DaemonPrefs {
        streaming_quality: playback.quality,
        mpris_enabled: playback.mpris,
        hook_script: hooks.script,
    })
}

fn to_sections(sections: &mut Sections, prefs: &DaemonPrefs) -> Result<(), String> {
    sections.set(
        PLAYBACK,
        &PlaybackSection {
            quality: prefs.streaming_quality.clone(),
            mpris: prefs.mpris_enabled,
        },
    )?;
    sections.set(
        HOOKS,
        &HooksSection {
            script: prefs.hook_script.clone(),
        },
    )
}

/// Fold in the files these prefs used to live in. Plain JSON, so this needs
/// no feature flag. `playback_preferences.db` held the desktop's playback
/// preferences, which nothing read; it is only deleted.
fn migrate(data_root: &Path) {
    settings_file::migrate_legacy(
        data_root,
        &data_root.join(LEGACY_JSON),
        PLAYBACK,
        |json, sections| {
            let text = std::fs::read_to_string(json).map_err(|e| e.to_string())?;
            // Unknown fields are ignored: `volume` was written by older
            // versions and never read.
            let prefs: DaemonPrefs = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            to_sections(sections, &prefs)
        },
    );
    let playback_db = data_root.join("playback_preferences.db");
    if playback_db.exists() {
        settings_file::remove_legacy(&playback_db);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "qbz-app-daemon-prefs-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = unique_test_dir("missing");
        let prefs = load_at(&dir);
        assert_eq!(prefs.streaming_quality, "hires_plus");
        assert!(prefs.mpris_enabled);
        assert!(!dir.exists(), "a read must not create the data root");
    }

    #[test]
    fn roundtrip_preserves_values_at_tempdir() {
        let dir = unique_test_dir("roundtrip");
        update_at(&dir, |p| {
            p.streaming_quality = "cd".into();
            // Non-default (default is true) so the roundtrip actually
            // exercises the field, not just its presence.
            p.mpris_enabled = false;
            // Non-default (default is empty) for the same reason.
            p.hook_script = "/usr/local/bin/qbz-hook.sh".into();
        })
        .expect("save daemon prefs");
        let loaded = load_at(&dir);

        assert_eq!(loaded.streaming_quality, "cd");
        assert!(!loaded.mpris_enabled);
        assert_eq!(loaded.hook_script, "/usr/local/bin/qbz-hook.sh");
        let text = std::fs::read_to_string(settings_file::path(&dir)).unwrap();
        assert!(
            text.contains("[playback]") && text.contains("[hooks]"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_json_file_is_migrated_and_deleted() {
        let dir = unique_test_dir("legacy-json");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join(LEGACY_JSON),
            r#"{"streaming_quality":"mp3","volume":0.3,"mpris_enabled":false,
                "hook_script":"/opt/hook.sh","bitrate_kbps":320}"#,
        )
        .expect("write raw prefs file");
        std::fs::write(dir.join("playback_preferences.db"), b"sqlite").unwrap();

        let loaded = load_at(&dir);

        assert_eq!(loaded.streaming_quality, "mp3");
        assert!(!loaded.mpris_enabled);
        assert_eq!(loaded.hook_script, "/opt/hook.sh");
        assert!(!dir.join(LEGACY_JSON).exists());
        assert!(!dir.join("playback_preferences.db").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
