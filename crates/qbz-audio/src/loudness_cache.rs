//! Loudness cache — persists EBU R128 measurements, one line per track.
//!
//! Three properties matter more than the caching itself, because this sits on
//! the audio thread's startup path:
//!
//! 1. **It cannot fail.** Construction used to return `Result` and the caller
//!    answered a disk error with `panic!` — on the audio thread, as its first
//!    act. A full or read-only card (an SD card on a Pi, say) meant no audio at
//!    all for the life of the process, rather than normalization being a little
//!    slower. Every path here degrades instead: disk, then memory.
//! 2. **It opens lazily.** `normalization_enabled` defaults to `false` to keep
//!    the pipeline bit-perfect, so on most installs no measurement is ever
//!    looked up. The file is read on the first lookup and created on the first
//!    measurement, so those installs never write to the card at all.
//! 3. **It lives where the caller says.** The directory is supplied, like
//!    `AudioSettingsStore::new_at`, so a daemon with its own profile root keeps
//!    its cache inside it instead of in the desktop app's data directory.
//!
//! # The file
//!
//! `loudness_cache.tsv`: a header line, then one line per measurement,
//!
//! ```text
//! <track_id> TAB <gain_db> TAB <peak> TAB <source> TAB <checksum>
//! ```
//!
//! read once into a map, and appended to as tracks are measured. A track
//! measured again gets a second line, and the later line wins. Lookups never
//! touch the disk.
//!
//! Appends are not fsynced: losing the last few measurements to a power cut
//! costs a re-measurement. What a power cut must not do is attach one track's
//! gain to another, and an append cut short can do exactly that, because the
//! next append continues the broken line: a torn `12` followed by `456 ...`
//! reads as a well-formed line for track 12456. So every line carries a
//! checksum of the rest, a line that fails it is dropped, and a file with any
//! dropped line is rewritten from the good ones before anything is appended
//! to it again.
//!
//! The file is also rewritten (to a temporary, fsynced, renamed into place)
//! when superseded lines outnumber live ones, and when it outgrows
//! [`MAX_ENTRIES`], in which case the oldest measurements go. At 20,000
//! tracks it is about a megabyte, and loading it is one pass over that.
//!
//! It replaced `loudness_cache.db` (SQLite). That is deleted on first use, not
//! migrated: a measurement is cheap to redo.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE_NAME: &str = "loudness_cache.tsv";
const TMP_NAME: &str = "loudness_cache.tsv.tmp";
const LEGACY_DB: &str = "loudness_cache.db";
const HEADER: &str = "# pibuz loudness cache v1";

/// The most tracks kept. Far past any one listener's rotation; past it the
/// oldest measurements are dropped when the file is compacted.
pub const MAX_ENTRIES: usize = 50_000;

/// Compact only once this many superseded lines have piled up, so a small
/// file is never rewritten for a handful of re-measurements.
const COMPACT_MIN_STALE: usize = 1_000;

#[derive(Debug, Clone)]
pub struct CachedLoudness {
    pub gain_db: f32,
    pub peak: f32,
    /// Source of the measurement: "ebur128" or "replaygain"
    pub source: String,
}

struct Entry {
    loudness: CachedLoudness,
    /// When it was stored, relative to the others: eviction drops the lowest.
    seq: u64,
}

struct Cache {
    entries: HashMap<u64, Entry>,
    next_seq: u64,
    /// Lines in the file, good or bad, header excluded. Minus the live
    /// entries, this is how much a rewrite would reclaim.
    lines_on_disk: usize,
    /// The file on disk starts with this version's header, so appending to it
    /// is safe. False until a load finds one or a rewrite writes one.
    file_ok: bool,
    /// The open file to append to, once something has been stored. `None`
    /// either before the first store or once the disk has failed.
    appender: Option<File>,
    /// The disk failed and this run is in memory from here on.
    disk_failed: bool,
}

enum State {
    /// Nothing has asked for a measurement yet, so nothing has been read.
    Unopened,
    Ready(Cache),
}

pub struct LoudnessCache {
    /// Where the file belongs, or `None` for a cache that must never touch the
    /// disk.
    dir: Option<PathBuf>,
    max_entries: usize,
    state: Mutex<State>,
}

impl LoudnessCache {
    /// A cache stored in `dir`. Nothing is read until the first lookup, and
    /// nothing written until the first measurement.
    pub fn open_at(dir: PathBuf) -> Self {
        Self::with_capacity(Some(dir), MAX_ENTRIES)
    }

    /// A cache that never touches the disk: measurements last for this run and
    /// are recomputed after a restart. For callers with no profile root of
    /// their own, which must not guess at one.
    pub fn in_memory() -> Self {
        Self::with_capacity(None, MAX_ENTRIES)
    }

    fn with_capacity(dir: Option<PathBuf>, max_entries: usize) -> Self {
        Self {
            dir,
            max_entries,
            state: Mutex::new(State::Unopened),
        }
    }

    /// Look up cached loudness for a track. Never touches the disk after the
    /// first call.
    pub fn get(&self, track_id: u64) -> Option<CachedLoudness> {
        self.with_cache(|cache, _| cache.entries.get(&track_id).map(|e| e.loudness.clone()))
            .flatten()
    }

    /// Store or update loudness data for a track.
    pub fn set(&self, track_id: u64, gain_db: f32, peak: f32, source: &str) {
        let loudness = CachedLoudness {
            gain_db,
            peak,
            // A tab or newline would break the line format. The sources are
            // fixed words, so this never fires; it keeps a bad one from
            // costing the rest of the file.
            source: source.replace(['\t', '\n', '\r'], " "),
        };
        let max_entries = self.max_entries;
        self.with_cache(|cache, dir| {
            let seq = cache.next_seq;
            cache.next_seq += 1;
            cache.entries.insert(
                track_id,
                Entry {
                    loudness: loudness.clone(),
                    seq,
                },
            );
            // Evict to 90 %, not to the cap, so a cache at its limit is not
            // rewritten for every new track.
            let over = cache.entries.len() > max_entries;
            if over {
                cache.evict_to(max_entries - max_entries / 10);
            }
            let Some(dir) = dir else { return };
            if cache.disk_failed {
                return;
            }
            let stale = cache.lines_on_disk.saturating_sub(cache.entries.len());
            let result = if over || (stale >= COMPACT_MIN_STALE && stale > cache.entries.len()) {
                cache.rewrite(dir)
            } else {
                cache.append(dir, track_id, &loudness)
            };
            if let Err(e) = result {
                cache.fail_disk(dir, &e);
            }
        });
    }

    /// Run `f` against the loaded cache, reading the file on the first call.
    ///
    /// `None` only for a poisoned lock, which every caller treats as "not
    /// cached" — always a safe answer.
    fn with_cache<T>(&self, f: impl FnOnce(&mut Cache, Option<&Path>) -> T) -> Option<T> {
        let mut state = self.state.lock().ok()?;
        if matches!(*state, State::Unopened) {
            *state = State::Ready(Cache::load(self.dir.as_deref()));
        }
        match &mut *state {
            State::Ready(cache) => Some(f(cache, self.dir.as_deref())),
            State::Unopened => None,
        }
    }
}

impl Cache {
    fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            next_seq: 0,
            lines_on_disk: 0,
            file_ok: false,
            appender: None,
            disk_failed: false,
        }
    }

    /// Read the file, if there is one. A missing file is an empty cache; a
    /// file with any bad line is rewritten from its good ones right away, so
    /// nothing is ever appended to a broken line.
    fn load(dir: Option<&Path>) -> Self {
        let mut cache = Self::empty();
        let Some(dir) = dir else { return cache };

        let legacy = dir.join(LEGACY_DB);
        if legacy.exists() {
            qbz_models::settings_file::remove_legacy(&legacy);
            log::info!(
                "[LoudnessCache] Removed {} (measurements are redone)",
                legacy.display()
            );
        }

        let path = dir.join(FILE_NAME);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return cache,
            Err(e) => {
                cache.fail_disk(dir, &format!("{}: {e}", path.display()));
                return cache;
            }
        };

        let mut lines = bytes.split(|b| *b == b'\n');
        let header_ok = lines.next() == Some(HEADER.as_bytes());
        let mut bad = 0usize;
        // A file that ends without a newline was cut short mid-append: its last
        // "line" is a fragment even if it happens to parse.
        let complete = bytes.ends_with(b"\n");
        let mut body: Vec<&[u8]> = lines.collect();
        if complete {
            body.pop(); // the empty slice after the final newline
        } else if body.pop().is_some_and(|l| !l.is_empty()) {
            bad += 1;
        }
        if header_ok {
            cache.file_ok = true;
            for line in body {
                cache.lines_on_disk += 1;
                match parse_line(line) {
                    Some((id, loudness)) => {
                        let seq = cache.next_seq;
                        cache.next_seq += 1;
                        cache.entries.insert(id, Entry { loudness, seq });
                    }
                    None => bad += 1,
                }
            }
        } else if !bytes.is_empty() {
            log::warn!(
                "[LoudnessCache] {} is not a loudness cache this version wrote; starting over",
                path.display()
            );
            bad += 1;
        }

        if bad > 0 {
            log::warn!(
                "[LoudnessCache] Dropped {bad} damaged line(s) from {}; those tracks will be \
                 measured again",
                path.display()
            );
            if let Err(e) = cache.rewrite(dir) {
                cache.fail_disk(dir, &e);
            }
        }
        cache
    }

    fn append(
        &mut self,
        dir: &Path,
        track_id: u64,
        loudness: &CachedLoudness,
    ) -> Result<(), String> {
        let path = dir.join(FILE_NAME);
        let fail = |e: std::io::Error| format!("{}: {e}", path.display());
        if self.appender.is_none() {
            if !self.file_ok {
                // First measurement ever (or an empty file): write the header.
                return self.rewrite(dir);
            }
            self.appender = Some(OpenOptions::new().append(true).open(&path).map_err(fail)?);
        }
        let line = format_line(track_id, loudness);
        // One write call per line, so a crash tears at most this line.
        self.appender
            .as_mut()
            .expect("opened above")
            .write_all(line.as_bytes())
            .map_err(fail)?;
        self.lines_on_disk += 1;
        Ok(())
    }

    /// Drop the oldest measurements until at most `max` are left.
    fn evict_to(&mut self, max: usize) {
        if self.entries.len() <= max {
            return;
        }
        let mut seqs: Vec<u64> = self.entries.values().map(|e| e.seq).collect();
        seqs.sort_unstable();
        let cutoff = seqs[self.entries.len() - max];
        self.entries.retain(|_, e| e.seq >= cutoff);
    }

    /// Write every live entry to a fresh file and swap it in.
    fn rewrite(&mut self, dir: &Path) -> Result<(), String> {
        let path = dir.join(FILE_NAME);
        let tmp = dir.join(TMP_NAME);
        let fail = |e: std::io::Error| format!("{}: {e}", path.display());
        std::fs::create_dir_all(dir).map_err(fail)?;

        let mut ordered: Vec<(&u64, &Entry)> = self.entries.iter().collect();
        ordered.sort_unstable_by_key(|(_, e)| e.seq);
        let mut text = String::with_capacity(48 * ordered.len() + HEADER.len() + 1);
        text.push_str(HEADER);
        text.push('\n');
        for (id, entry) in &ordered {
            text.push_str(&format_line(**id, &entry.loudness));
        }
        {
            let mut out = File::create(&tmp).map_err(fail)?;
            out.write_all(text.as_bytes()).map_err(fail)?;
            out.sync_all().map_err(fail)?;
        }
        // The appender points at the file about to be replaced.
        self.appender = None;
        std::fs::rename(&tmp, &path).map_err(fail)?;
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
        self.lines_on_disk = ordered.len();
        self.file_ok = true;
        Ok(())
    }

    /// Carry on in memory for the rest of the run. Said once, not per track.
    fn fail_disk(&mut self, dir: &Path, error: &str) {
        log::warn!(
            "[LoudnessCache] {} unavailable ({error}) — keeping measurements in memory \
             for this run",
            dir.join(FILE_NAME).display()
        );
        self.appender = None;
        self.disk_failed = true;
    }
}

fn format_line(track_id: u64, l: &CachedLoudness) -> String {
    let body = format!("{track_id}\t{}\t{}\t{}", l.gain_db, l.peak, l.source);
    format!("{body}\t{:08x}\n", fnv1a(body.as_bytes()))
}

/// One line back into a measurement, or `None` if anything about it is off.
fn parse_line(line: &[u8]) -> Option<(u64, CachedLoudness)> {
    let line = std::str::from_utf8(line).ok()?;
    let (body, checksum) = line.rsplit_once('\t')?;
    if u32::from_str_radix(checksum, 16).ok()? != fnv1a(body.as_bytes()) || checksum.len() != 8 {
        return None;
    }
    let mut fields = body.split('\t');
    let id = fields.next()?.parse().ok()?;
    let gain_db: f32 = fields.next()?.parse().ok()?;
    let peak: f32 = fields.next()?.parse().ok()?;
    let source = fields.next()?.to_string();
    if fields.next().is_some() || !gain_db.is_finite() || !peak.is_finite() {
        return None;
    }
    Some((
        id,
        CachedLoudness {
            gain_db,
            peak,
            source,
        },
    ))
}

/// 32-bit FNV-1a: no dependency, and plenty to tell a torn or flipped line
/// from a whole one.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
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

    fn file(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(FILE_NAME)).unwrap_or_default()
    }

    #[test]
    fn a_measurement_round_trips_and_the_newest_one_wins() {
        let dir = scratch("roundtrip");
        let cache = LoudnessCache::open_at(dir.clone());

        assert!(cache.get(42).is_none(), "nothing stored yet");

        cache.set(42, -7.5, 0.98, "ebur128");
        let hit = cache.get(42).expect("stored measurement");
        assert_eq!(hit.gain_db, -7.5);
        assert_eq!(hit.peak, 0.98);
        assert_eq!(hit.source, "ebur128");

        cache.set(42, -3.0, 0.5, "replaygain");
        let hit = cache.get(42).expect("replaced measurement");
        assert_eq!(hit.gain_db, -3.0);
        assert_eq!(hit.source, "replaygain");

        // And from the file, in a new process's worth of cache.
        let reopened = LoudnessCache::open_at(dir.clone());
        let hit = reopened.get(42).expect("persisted");
        assert_eq!(hit.gain_db, -3.0, "the later line wins");
        assert_eq!(hit.source, "replaygain");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Property 2: with normalization off — the default — nothing is stored,
    /// so the daemon must leave nothing behind. A lookup only reads.
    #[test]
    fn nothing_reaches_the_disk_until_a_measurement_does() {
        let dir = scratch("lazy");
        let cache = LoudnessCache::open_at(dir.clone());
        assert!(!dir.exists(), "constructing the cache created {dir:?}");

        assert!(cache.get(1).is_none());
        assert!(!dir.exists(), "a lookup must not create anything");

        cache.set(1, -6.0, 0.9, "ebur128");
        assert!(file(&dir).starts_with(HEADER), "{}", file(&dir));
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
        // A path whose parent is a FILE: nothing can be created under it.
        let blocker = scratch("unwritable");
        std::fs::create_dir_all(blocker.parent().expect("temp dir has a parent")).ok();
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");

        let cache = LoudnessCache::open_at(blocker.join("cache"));

        // No panic, and the cache still does its job for this run.
        cache.set(99, -5.0, 0.7, "ebur128");
        cache.set(100, -4.0, 0.7, "ebur128");
        assert_eq!(cache.get(99).expect("memory fallback").gain_db, -5.0);
        assert_eq!(cache.get(100).expect("memory fallback").gain_db, -4.0);

        assert!(blocker.is_file(), "the blocker must not have been replaced");
        let _ = std::fs::remove_file(&blocker);
    }

    /// The failure the checksum exists for. An append cut short leaves `12`;
    /// the next append continues that line, and without a check it reads as a
    /// perfectly good line for track 12456 carrying track 456's gain.
    #[test]
    fn a_torn_append_never_lends_its_gain_to_another_track() {
        let dir = scratch("torn");
        let cache = LoudnessCache::open_at(dir.clone());
        cache.set(1, -1.0, 0.1, "ebur128");
        drop(cache);

        // The crash: half a line, no newline.
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join(FILE_NAME))
            .unwrap();
        f.write_all(b"12").unwrap();
        drop(f);

        // A naive appender would now write straight after the fragment.
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join(FILE_NAME))
            .unwrap();
        f.write_all(
            format_line(
                456,
                &CachedLoudness {
                    gain_db: -9.0,
                    peak: 0.5,
                    source: "ebur128".into(),
                },
            )
            .as_bytes(),
        )
        .unwrap();
        drop(f);

        let reopened = LoudnessCache::open_at(dir.clone());
        assert!(reopened.get(12456).is_none(), "a fragment became a track");
        assert!(
            reopened.get(456).is_none(),
            "the merged line must be dropped whole"
        );
        assert_eq!(
            reopened.get(1).expect("the good line survives").gain_db,
            -1.0
        );

        // The damaged line is gone from the file, so new appends start clean.
        reopened.set(2, -2.0, 0.2, "ebur128");
        let again = LoudnessCache::open_at(dir.clone());
        assert_eq!(
            again.get(2).expect("appended after the repair").gain_db,
            -2.0
        );
        assert!(!file(&dir).contains("12456"), "{}", file(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fragment_at_the_end_is_dropped_even_if_it_parses() {
        let dir = scratch("fragment");
        let cache = LoudnessCache::open_at(dir.clone());
        cache.set(5, -5.0, 0.5, "ebur128");
        drop(cache);
        // A whole, valid line — but the newline never made it to the card.
        let line = format_line(
            6,
            &CachedLoudness {
                gain_db: -6.0,
                peak: 0.6,
                source: "ebur128".into(),
            },
        );
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join(FILE_NAME))
            .unwrap();
        f.write_all(line.trim_end().as_bytes()).unwrap();
        drop(f);

        let reopened = LoudnessCache::open_at(dir.clone());
        assert!(reopened.get(6).is_none());
        assert!(
            file(&dir).ends_with('\n'),
            "repaired before the next append"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_line_costs_only_that_track() {
        let dir = scratch("bitflip");
        let cache = LoudnessCache::open_at(dir.clone());
        for id in 1..=3 {
            cache.set(id, -(id as f32), 0.5, "ebur128");
        }
        drop(cache);
        // Flip one digit inside track 2's gain.
        let text = file(&dir).replace("2\t-2\t", "2\t-7\t");
        std::fs::write(dir.join(FILE_NAME), text).unwrap();

        let reopened = LoudnessCache::open_at(dir.clone());
        assert!(
            reopened.get(2).is_none(),
            "a changed value must fail its checksum"
        );
        assert_eq!(reopened.get(1).unwrap().gain_db, -1.0);
        assert_eq!(reopened.get(3).unwrap().gain_db, -3.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_file_gets_its_header_before_the_first_line() {
        let dir = scratch("empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), "").unwrap();
        let cache = LoudnessCache::open_at(dir.clone());
        cache.set(3, -3.0, 0.3, "ebur128");
        let reopened = LoudnessCache::open_at(dir.clone());
        assert_eq!(reopened.get(3).expect("kept across a reload").gain_db, -3.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_file_is_replaced_not_misread() {
        let dir = scratch("foreign");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), "1\t-3\t0.5\tebur128\n").unwrap();
        let cache = LoudnessCache::open_at(dir.clone());
        assert!(cache.get(1).is_none());
        assert_eq!(file(&dir), format!("{HEADER}\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn re_measuring_compacts_the_file_instead_of_growing_it() {
        let dir = scratch("compact");
        let cache = LoudnessCache::open_at(dir.clone());
        for round in 0..5 {
            for id in 0..500u64 {
                cache.set(id, -(round as f32), 0.5, "ebur128");
            }
        }
        let lines = file(&dir).lines().count() - 1;
        assert!(
            lines <= 500 + COMPACT_MIN_STALE + 1,
            "2,500 writes of 500 tracks left {lines} lines"
        );
        let reopened = LoudnessCache::open_at(dir.clone());
        for id in 0..500u64 {
            assert_eq!(reopened.get(id).unwrap().gain_db, -4.0, "track {id}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn past_the_cap_the_oldest_measurements_go() {
        let dir = scratch("cap");
        let cache = LoudnessCache::with_capacity(Some(dir.clone()), 100);
        for id in 0..150u64 {
            cache.set(id, -1.0, 0.5, "ebur128");
        }
        let reopened = LoudnessCache::with_capacity(Some(dir.clone()), 100);
        let kept = (0..150u64).filter(|id| reopened.get(*id).is_some()).count();
        assert!((50..=100).contains(&kept), "kept {kept}");
        assert!(reopened.get(149).is_some(), "the newest must stay");
        assert!(reopened.get(0).is_none(), "the oldest must go");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The size the cache is built for: 20,000 tracks survive a restart.
    #[test]
    fn twenty_thousand_tracks_round_trip() {
        let dir = scratch("20k");
        let cache = LoudnessCache::open_at(dir.clone());
        for id in 0..20_000u64 {
            cache.set(1_000_000 + id, -((id % 20) as f32) - 0.25, 0.75, "ebur128");
        }
        drop(cache);
        let size = std::fs::metadata(dir.join(FILE_NAME)).unwrap().len();
        assert!(size < 1_500_000, "{size} bytes for 20k tracks");

        let reopened = LoudnessCache::open_at(dir.clone());
        for id in (0..20_000u64).step_by(997) {
            let hit = reopened.get(1_000_000 + id).expect("present after reload");
            assert_eq!(hit.gain_db, -((id % 20) as f32) - 0.25);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_sqlite_cache_is_deleted_not_migrated() {
        let dir = scratch("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [LEGACY_DB, "loudness_cache.db-wal", "loudness_cache.db-shm"] {
            std::fs::write(dir.join(name), b"old").unwrap();
        }
        let cache = LoudnessCache::open_at(dir.clone());
        assert!(cache.get(1).is_none());
        for name in [LEGACY_DB, "loudness_cache.db-wal", "loudness_cache.db-shm"] {
            assert!(!dir.join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
