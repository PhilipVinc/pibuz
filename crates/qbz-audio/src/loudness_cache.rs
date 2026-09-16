//! Loudness cache — persists EBU R128 measurements in SQLite.
//!
//! Three properties matter more than the caching itself, because this sits on
//! the audio thread's startup path:
//!
//! 1. **It cannot fail.** Construction used to return `Result` and the caller
//!    answered a disk error with `panic!` — on the audio thread, as its first
//!    act. A full or read-only card (an SD card on a Pi, say) meant no audio at
//!    all for the life of the process, rather than normalization being a little
//!    slower. Every path here degrades instead: disk, then memory, then a cache
//!    that politely answers nothing.
//! 2. **It opens lazily.** `normalization_enabled` defaults to `false` to keep
//!    the pipeline bit-perfect, so on most installs no measurement is ever
//!    looked up. Opening on first use means those installs create no database,
//!    no WAL, and no SD-card writes at all.
//! 3. **It lives where the caller says.** The directory is supplied, like
//!    `AudioSettingsStore::new_at`, so a daemon with its own profile root keeps
//!    its cache inside it instead of in the desktop app's data directory.

use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DB_NAME: &str = "loudness_cache.db";

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS track_loudness (
    track_id INTEGER PRIMARY KEY,
    gain_db REAL NOT NULL,
    peak REAL NOT NULL DEFAULT 0.0,
    source TEXT NOT NULL DEFAULT 'ebur128',
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
)";

#[derive(Debug, Clone)]
pub struct CachedLoudness {
    pub gain_db: f32,
    pub peak: f32,
    /// Source of the measurement: "ebur128" or "replaygain"
    pub source: String,
}

/// What the cache is backed by right now.
enum State {
    /// Nothing has asked for a measurement yet, so nothing has been created.
    Unopened,
    Ready(Connection),
    /// Even an in-memory database could not be opened. Reported once; every
    /// later call is a silent no-op rather than a repeated warning.
    Disabled,
}

pub struct LoudnessCache {
    /// Where the database belongs, or `None` for a cache that must never touch
    /// the disk.
    dir: Option<PathBuf>,
    state: Mutex<State>,
}

impl LoudnessCache {
    /// A cache stored in `dir`, created on first use.
    ///
    /// The directory is not created and the file is not opened here — a caller
    /// that never normalizes never causes a write.
    pub fn open_at(dir: PathBuf) -> Self {
        Self {
            dir: Some(dir),
            state: Mutex::new(State::Unopened),
        }
    }

    /// A cache that never touches the disk: measurements last for this run and
    /// are recomputed after a restart. For callers with no profile root of
    /// their own, which must not guess at one.
    pub fn in_memory() -> Self {
        Self {
            dir: None,
            state: Mutex::new(State::Unopened),
        }
    }

    /// Look up cached loudness for a track.
    pub fn get(&self, track_id: u64) -> Option<CachedLoudness> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT gain_db, peak, source FROM track_loudness WHERE track_id = ?1",
                params![track_id as i64],
                |row| {
                    Ok(CachedLoudness {
                        gain_db: row.get::<_, f64>(0)? as f32,
                        peak: row.get::<_, f64>(1)? as f32,
                        source: row.get(2)?,
                    })
                },
            )
            .ok()
        })
        .flatten()
    }

    /// Store or update loudness data for a track.
    pub fn set(&self, track_id: u64, gain_db: f32, peak: f32, source: &str) {
        self.with_connection(|conn| {
            let result = conn.execute(
                "INSERT OR REPLACE INTO track_loudness (track_id, gain_db, peak, source, created_at)
                 VALUES (?1, ?2, ?3, ?4, strftime('%s', 'now'))",
                params![track_id as i64, gain_db as f64, peak as f64, source],
            );
            if let Err(e) = result {
                log::warn!(
                    "[LoudnessCache] Failed to store loudness for track {}: {}",
                    track_id,
                    e
                );
            }
        });
    }

    /// Run `f` against the connection, opening it on the first call.
    ///
    /// `None` means there is no connection to run against — a disabled cache or
    /// a poisoned lock — and every caller treats that as "not cached", which is
    /// always a safe answer.
    fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> T) -> Option<T> {
        let mut state = self.state.lock().ok()?;

        if matches!(*state, State::Unopened) {
            *state = match self.open() {
                Ok(conn) => State::Ready(conn),
                Err(err) => {
                    // Not an error: normalization still works, it just measures
                    // every track afresh. Saying so once beats a warning per track.
                    log::warn!(
                        "[LoudnessCache] Unavailable ({err}) — loudness will be \
                         measured again for every track this run"
                    );
                    State::Disabled
                }
            };
        }

        match &*state {
            State::Ready(conn) => Some(f(conn)),
            State::Unopened | State::Disabled => None,
        }
    }

    /// Disk if we have a directory and it works, memory otherwise.
    fn open(&self) -> Result<Connection, String> {
        if let Some(dir) = self.dir.as_deref() {
            match Self::open_on_disk(dir) {
                Ok(conn) => {
                    log::info!("[LoudnessCache] Opened at {}", dir.join(DB_NAME).display());
                    return Ok(conn);
                }
                Err(err) => {
                    // The interesting case: a full or read-only card. Keep
                    // normalization working, just without persistence.
                    log::warn!(
                        "[LoudnessCache] Could not open {}: {err} — measuring in \
                         memory for this run",
                        dir.join(DB_NAME).display()
                    );
                }
            }
        }
        Self::open_in_memory()
    }

    fn open_on_disk(dir: &Path) -> Result<Connection, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("failed to create data directory: {}", e))?;

        let conn = Connection::open(dir.join(DB_NAME))
            .map_err(|e| format!("failed to open loudness cache database: {}", e))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("failed to enable WAL: {}", e))?;

        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("failed to create loudness table: {}", e))?;

        Ok(conn)
    }

    fn open_in_memory() -> Result<Connection, String> {
        // No WAL: there is no file to journal against.
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("failed to open in-memory loudness cache: {}", e))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("failed to create in-memory loudness table: {}", e))?;
        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qbz_loudness_{}_{}_{:?}",
            name,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_measurement_round_trips_and_the_newest_one_wins() {
        let dir = scratch("roundtrip");
        let cache = LoudnessCache::open_at(dir.clone());

        assert!(cache.get(42).is_none(), "nothing stored yet");

        cache.set(42, -7.5, 0.98, "ebur128");
        let hit = cache.get(42).expect("stored measurement");
        assert!((hit.gain_db - -7.5).abs() < 1e-6);
        assert!((hit.peak - 0.98).abs() < 1e-6);
        assert_eq!(hit.source, "ebur128");

        cache.set(42, -3.0, 0.5, "replaygain");
        let hit = cache.get(42).expect("replaced measurement");
        assert!((hit.gain_db - -3.0).abs() < 1e-6);
        assert_eq!(hit.source, "replaygain");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Property 2: with normalization off — the default — nothing is ever
    /// looked up, so the daemon must leave no database behind at all.
    #[test]
    fn nothing_reaches_the_disk_until_a_measurement_does() {
        let dir = scratch("lazy");
        let cache = LoudnessCache::open_at(dir.clone());

        assert!(!dir.exists(), "constructing the cache created {dir:?}");

        // A miss opens it (a lookup is a use), but a cache that is never
        // consulted never gets here.
        assert!(cache.get(1).is_none());
        assert!(dir.join(DB_NAME).exists(), "first use should create the db");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_memory_only_cache_creates_no_file_but_still_caches() {
        let cache = LoudnessCache::in_memory();
        cache.set(7, -9.0, 1.0, "ebur128");
        assert_eq!(cache.get(7).expect("in-memory hit").source, "ebur128");
    }

    /// Property 1, and the whole point of the change: this is the shape of a
    /// full or read-only card. It used to be `panic!` on the audio thread.
    #[test]
    fn an_unusable_directory_degrades_to_memory_instead_of_failing() {
        // A path whose parent is a FILE: `create_dir_all` cannot succeed.
        let blocker = scratch("unwritable");
        std::fs::create_dir_all(blocker.parent().expect("temp dir has a parent")).ok();
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");

        let cache = LoudnessCache::open_at(blocker.join("cache"));

        // No panic, and the cache still does its job for this run.
        cache.set(99, -5.0, 0.7, "ebur128");
        let hit = cache
            .get(99)
            .expect("the in-memory fallback must still cache");
        assert!((hit.gain_db - -5.0).abs() < 1e-6);

        assert!(blocker.is_file(), "the blocker must not have been replaced");
        let _ = std::fs::remove_file(&blocker);
    }
}
