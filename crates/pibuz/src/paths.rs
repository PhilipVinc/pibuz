// crates/pibuz/src/paths.rs — daemon profile roots: `~/.config/pibuz`,
// `~/.local/share/pibuz`, `~/.cache/pibuz`, or the overrides `resolve` takes.
//
// PRE-RENAME PROFILES. The daemon shipped as `qbzd` through 2.3.2 and wrote
// `~/.config/qbzd`, `~/.local/share/qbzd`, `~/.cache/qbzd`. That data root
// holds the persisted QConnect `device_uuid` and the audio settings, so
// starting fresh would re-appear in the Qobuz app as a different device and
// drop back to default (not bit-perfect) audio. Each root therefore resolves
// to `pibuz` unless that is absent and `qbzd` is there. Nothing is copied.

use std::path::{Path, PathBuf};

/// Directory name for the three XDG roots.
const APP_DIR: &str = "pibuz";
/// The pre-rename directory name, honoured when it is the one that exists.
const LEGACY_APP_DIR: &str = "qbzd";
/// Config file inside the config root.
pub(crate) const CONFIG_FILE: &str = "pibuz.toml";
/// The pre-rename config file name, honoured when it is the one that exists.
const LEGACY_CONFIG_FILE: &str = "qbzd.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileRoots {
    pub(crate) config: PathBuf,
    pub data: PathBuf,
    pub(crate) cache: PathBuf,
}

impl ProfileRoots {
    /// Resolve the three profile roots.
    ///
    /// - `config_override`: the `--config <path>` argument (a FILE path); its
    ///   parent directory becomes the config root. `None` falls back to
    ///   `dirs::config_dir()/pibuz`.
    /// - `data_root_override`: the already-parsed `pibuz.toml` `data_root`
    ///   value (a container override, e.g. for a Pi SD-card layout). `None`
    ///   falls back to `dirs::data_dir()/pibuz`.
    ///
    /// Cache is `dirs::cache_dir()/pibuz` UNLESS `data_root` was overridden,
    /// in which case cache = `<data_root>/cache` — never
    /// `<data_root>/../pibuz-cache`, which would walk outside the container.
    ///
    /// Nothing is created here. The config folder only ever holds a
    /// `pibuz.toml` someone wrote by hand; the data root is created by the
    /// instance lock and the cache root by the caches that write to it.
    pub(crate) fn resolve(
        config_override: Option<&Path>,
        data_root_override: Option<&Path>,
    ) -> Self {
        let config = match config_override {
            Some(config_file) => config_file
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            None => default_config_dir(),
        };
        let data = match data_root_override {
            Some(dir) => dir.to_path_buf(),
            None => default_data_dir(),
        };
        let cache = match data_root_override {
            Some(_) => data.join("cache"),
            None => default_cache_dir(),
        };

        Self {
            config,
            data,
            cache,
        }
    }

    /// This profile's config file — `pibuz.toml`, or the pre-rename
    /// `qbzd.toml` when that is the file the box actually has.
    pub(crate) fn config_file(&self) -> PathBuf {
        config_file_in(&self.config)
    }
}

/// `<base>/pibuz`, unless that does not exist and a pre-rename `<base>/qbzd`
/// does — then the existing profile, so an upgrade keeps its data.
fn app_dir(base: Option<PathBuf>) -> PathBuf {
    let base = base.unwrap_or_else(|| PathBuf::from("."));
    let current = base.join(APP_DIR);
    if current.is_dir() {
        return current;
    }
    let legacy = base.join(LEGACY_APP_DIR);
    if legacy.is_dir() {
        return legacy;
    }
    current
}

fn default_config_dir() -> PathBuf {
    app_dir(dirs::config_dir())
}

fn default_data_dir() -> PathBuf {
    app_dir(dirs::data_dir())
}

fn default_cache_dir() -> PathBuf {
    app_dir(dirs::cache_dir())
}

/// The config file inside `config_dir`: `pibuz.toml`, unless it is absent and
/// a pre-rename `qbzd.toml` is sitting there — then that one, so an upgrade
/// keeps reading (and writing) the settings the box already has rather than
/// starting a second file beside them.
pub(crate) fn config_file_in(config_dir: &Path) -> PathBuf {
    let current = config_dir.join(CONFIG_FILE);
    if current.is_file() {
        return current;
    }
    let legacy = config_dir.join(LEGACY_CONFIG_FILE);
    if legacy.is_file() {
        return legacy;
    }
    current
}

/// The daemon's log, rotated to `pibuz.log.prev` at each start.
pub(crate) fn log_file(roots: &ProfileRoots) -> PathBuf {
    roots.data.join("logs").join("pibuz.log")
}

/// Where the logs, the disk cache and the Qobuz token cache lived before the
/// daemon kept them in its own roots: `~/.local/share/qbz` and `~/.cache/qbz`,
/// named after the crates rather than the program, unlisted by `config path`,
/// and out of `data_root`'s reach. Only `pibuz run` calls this.
///
/// It removes the sealed audio files, which are by far the bulk (tens of MB
/// on a Pi) and unreadable by anything (their key died with the run that
/// wrote them), and then each folder that is left empty. It deliberately
/// leaves the old log and token files: `qbz` is a name other software in
/// this family also uses, and neither file says whose it is.
pub(crate) fn clear_legacy_folders() {
    let Some(cache) = dirs::cache_dir().map(|dir| dir.join("qbz")) else {
        return;
    };
    let (files, bytes) = qbz_cache::remove_sealed_leftovers(&cache.join("playback"));
    if files > 0 {
        log::info!(
            "[paths] removed {files} sealed cache file(s) ({} MB) from {}, where the disk \
             cache lived before it moved into pibuz's own cache folder",
            bytes / (1024 * 1024),
            cache.join("playback").display()
        );
    }
    // Only succeeds once nothing else is in it.
    let _ = std::fs::remove_dir(&cache);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pibuz-paths-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// An upgrade from the `qbzd` releases must land on the profile the box
    /// already has — it holds the QConnect device_uuid and the audio settings.
    #[test]
    fn legacy_root_is_used_when_only_the_old_one_exists() {
        let base = scratch_dir("legacy-only");
        std::fs::create_dir_all(base.join("qbzd")).unwrap();

        assert_eq!(app_dir(Some(base.clone())), base.join("qbzd"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Once the new root exists it wins, so a box that has been migrated never
    /// silently falls back to a directory left behind beside it.
    #[test]
    fn new_root_wins_over_a_leftover_legacy_one() {
        let base = scratch_dir("both-roots");
        std::fs::create_dir_all(base.join("qbzd")).unwrap();
        std::fs::create_dir_all(base.join("pibuz")).unwrap();

        assert_eq!(app_dir(Some(base.clone())), base.join("pibuz"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A fresh install never sees the old name.
    #[test]
    fn fresh_install_resolves_to_the_new_root() {
        let base = scratch_dir("fresh");
        std::fs::create_dir_all(&base).unwrap();

        assert_eq!(app_dir(Some(base.clone())), base.join("pibuz"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The config file follows the same rule, and — because the resolved path
    /// is what `config path` reports and `run` reads — a pre-rename `qbzd.toml` keeps being
    /// the one file, instead of being shadowed by an empty `pibuz.toml`.
    #[test]
    fn config_file_prefers_the_new_name_then_the_legacy_one() {
        let dir = scratch_dir("config-file");
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing on disk: the new name, ready to be created.
        assert_eq!(config_file_in(&dir), dir.join("pibuz.toml"));

        // Only the pre-rename file: use it where it is.
        std::fs::write(dir.join("qbzd.toml"), "").unwrap();
        assert_eq!(config_file_in(&dir), dir.join("qbzd.toml"));

        // Both: the new one.
        std::fs::write(dir.join("pibuz.toml"), "").unwrap();
        assert_eq!(config_file_in(&dir), dir.join("pibuz.toml"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The config folder holds `pibuz.toml` and nothing else, and nothing in
    /// pibuz writes that file: someone who wants one creates it. Resolving the
    /// roots used to create the folder anyway, so every box carried an empty
    /// `~/.config/pibuz` that `config path` pointed at.
    #[test]
    fn config_override_uses_parent_dir_and_creates_nothing() {
        let dir = scratch_dir("config-override");
        let _ = std::fs::remove_dir_all(&dir);
        let config_file = dir.join("nested").join("pibuz.toml");

        let roots = ProfileRoots::resolve(Some(&config_file), None);

        assert_eq!(roots.config, dir.join("nested"));
        assert!(
            !dir.exists(),
            "resolving the roots created {}",
            dir.display()
        );
    }

    /// Everything the daemon writes is under its own roots, so `data_root`
    /// moves all of it: settings and logs in the data root, the disk cache and
    /// the token cache in the cache root under it.
    #[test]
    fn logs_live_in_the_data_root() {
        let data_dir = scratch_dir("logs");
        let roots = ProfileRoots::resolve(None, Some(&data_dir));
        assert_eq!(log_file(&roots), data_dir.join("logs").join("pibuz.log"));
        assert!(roots.cache.starts_with(&data_dir));
    }

    #[test]
    fn data_root_override_places_cache_under_it_not_beside_it() {
        let data_dir = scratch_dir("data-override");

        let roots = ProfileRoots::resolve(None, Some(&data_dir));

        assert_eq!(roots.data, data_dir);
        assert_eq!(roots.cache, data_dir.join("cache"));
        // Never derived as a sibling of data_root.
        assert_ne!(roots.cache, data_dir.parent().unwrap().join("pibuz-cache"));
    }

    /// Linux-only by nature: XDG_* is a freedesktop convention, and `dirs`
    /// deliberately ignores it on macOS in favour of
    /// `~/Library/Application Support`. Running this there asserts a rule the
    /// platform does not have, which is why it failed on every dev Mac.
    /// The daemon ships on Linux; that is where the contract must hold.
    #[cfg(target_os = "linux")]
    #[test]
    fn defaults_resolve_under_xdg_roots_without_touching_real_home() {
        // SAFETY: single-threaded within this test; original values restored
        // before returning so no other test observes the override. This test
        // must never touch the real developer $HOME/.config etc.
        let base = scratch_dir("xdg-defaults");
        let xdg_config = base.join("config");
        let xdg_data = base.join("data");
        let xdg_cache = base.join("cache");

        let saved = [
            ("XDG_CONFIG_HOME", std::env::var("XDG_CONFIG_HOME").ok()),
            ("XDG_DATA_HOME", std::env::var("XDG_DATA_HOME").ok()),
            ("XDG_CACHE_HOME", std::env::var("XDG_CACHE_HOME").ok()),
        ];
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
        std::env::set_var("XDG_DATA_HOME", &xdg_data);
        std::env::set_var("XDG_CACHE_HOME", &xdg_cache);

        let roots = ProfileRoots::resolve(None, None);

        for (key, prev) in saved {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }

        assert_eq!(roots.config, xdg_config.join("pibuz"));
        assert_eq!(roots.data, xdg_data.join("pibuz"));
        assert_eq!(roots.cache, xdg_cache.join("pibuz"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
