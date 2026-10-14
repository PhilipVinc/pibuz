//! `settings.toml`: the one file the daemon's settings live in.
//!
//! One TOML table per settings domain — `[audio]`, `[playback]`, `[qconnect]`,
//! `[hooks]` — each owned by the crate that defines its struct. This module
//! knows nothing about what is in them; it only makes the file safe to share.
//!
//! It is shared by more than one process. The daemon writes it, and so does
//! `pibuz settings set`, which moOde runs once per key on every renderer start
//! while the daemon may well be up. Three rules keep that safe:
//!
//! - **Every write locks.** `flock` on a sibling `settings.toml.lock`, then
//!   re-read the file, change one section, write it back, unlock. Two writers
//!   changing different keys therefore cannot lose each other's change, which
//!   a plain read-modify-write would. The lock is a separate file because the
//!   settings file itself is replaced on every write, and a lock on a replaced
//!   inode protects nothing.
//! - **Every write replaces the file whole.** Write a temporary sibling, fsync
//!   it, rename it over the old one, fsync the directory. A reader takes no
//!   lock: it sees the old file or the new one, never half of either, and a
//!   power cut leaves one of the two.
//! - **A file that does not parse is never overwritten.** It was hand-edited,
//!   and the settings in it are the user's. Reads fall back to defaults and
//!   say so in the log; writes fail with the file name and the parse error, so
//!   `settings set` tells the user what to fix instead of silently resetting
//!   everything else in the file.
//!
//! A section struct must carry `#[serde(default)]` at the container level, so
//! a key missing from the file — new in this version, or deleted by hand —
//! takes its default instead of failing the whole section.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The file's name inside the daemon's data root.
pub const FILE_NAME: &str = "settings.toml";
const LOCK_NAME: &str = "settings.toml.lock";
const TMP_NAME: &str = "settings.toml.tmp";

/// Written at the top of every save. Comments are not preserved across a
/// rewrite, so this is the only one the file carries.
const HEADER: &str = "\
# pibuz settings. Prefer `pibuz settings set <key> <value>`: it validates the
# value and tells a running daemon to reload. Hand edits work too, but a file
# that does not parse is ignored until it is fixed.

";

/// Where the file lives under `root`.
pub fn path(root: &Path) -> PathBuf {
    root.join(FILE_NAME)
}

/// One section, or its defaults when the file or the section is absent.
///
/// A file or section that does not parse also yields the defaults, with an
/// error in the log: a daemon that cannot read its settings still starts.
pub fn read_section<T: DeserializeOwned + Default>(root: &Path, name: &str) -> T {
    try_read_section(root, name).unwrap_or_else(|e| {
        log::error!("[settings] {e}; using defaults for [{name}]");
        T::default()
    })
}

/// One section, or its defaults when the file or the section is absent; an
/// error when either is there but does not parse. For a caller that should
/// report a broken file rather than paper over it, such as `settings show`.
pub fn try_read_section<T: DeserializeOwned + Default>(
    root: &Path,
    name: &str,
) -> Result<T, String> {
    load(root)?.get(name)
}

/// Whether the file exists, parses, and has a `[name]` table.
pub fn has_section(root: &Path, name: &str) -> bool {
    load(root).map(|s| s.has(name)).unwrap_or(false)
}

/// Run `f` on one section under the lock and save the result.
///
/// The section is read fresh inside the lock, so `f` always sees the latest
/// value. Fails without writing if the file or the section does not parse.
pub fn update_section<T, R>(
    root: &Path,
    name: &str,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, String>
where
    T: Serialize + DeserializeOwned + Default,
{
    update(root, |sections| {
        let mut value: T = sections.get(name)?;
        let out = f(&mut value);
        sections.set(name, &value)?;
        Ok(out)
    })
}

/// Run `f` on the whole file under the lock, and save it if `f` changed
/// anything. For changes that span sections, or that must look before they
/// write (a migration that only fills a section nobody has written yet).
pub fn update<R>(
    root: &Path,
    f: impl FnOnce(&mut Sections) -> Result<R, String>,
) -> Result<R, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let _lock = Lock::acquire(root)?;
    let mut sections = load(root)?;
    let out = f(&mut sections)?;
    if sections.changed {
        write(root, &sections.table)?;
    }
    Ok(out)
}

/// The parsed file, as handed to [`update`]'s closure.
pub struct Sections {
    table: toml::Table,
    file: PathBuf,
    changed: bool,
}

impl Sections {
    /// The file these sections came from, for error messages.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Whether the file has a `[name]` table.
    pub fn has(&self, name: &str) -> bool {
        self.table.get(name).is_some_and(toml::Value::is_table)
    }

    /// The `[name]` table, or `T::default()` when there is none. An error
    /// when the table is there but does not fit `T`.
    pub fn get<T: DeserializeOwned + Default>(&self, name: &str) -> Result<T, String> {
        match self.table.get(name) {
            None => Ok(T::default()),
            Some(value) => value
                .clone()
                .try_into()
                .map_err(|e| format!("{} [{name}]: {e}", self.file.display())),
        }
    }

    /// Replace the `[name]` table with `value`.
    pub fn set<T: Serialize>(&mut self, name: &str, value: &T) -> Result<(), String> {
        let value = toml::Value::try_from(value)
            .map_err(|e| format!("{} [{name}]: {e}", self.file.display()))?;
        if self.table.get(name) != Some(&value) {
            self.table.insert(name.to_string(), value);
            self.changed = true;
        }
        Ok(())
    }
}

/// The whole file, parsed. Absent is empty; unparseable is an error.
pub fn load(root: &Path) -> Result<Sections, String> {
    let file = path(root);
    let table = match std::fs::read_to_string(&file) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map_err(|e| format!("{} does not parse: {e}", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    Ok(Sections {
        table,
        file,
        changed: false,
    })
}

fn write(root: &Path, table: &toml::Table) -> Result<(), String> {
    let file = path(root);
    let tmp = root.join(TMP_NAME);
    let fail = |e: std::io::Error| format!("{}: {e}", file.display());
    let body = toml::to_string_pretty(table).map_err(|e| format!("{}: {e}", file.display()))?;
    {
        // A fixed temporary name is safe: only the lock holder writes it.
        let mut out = File::create(&tmp).map_err(fail)?;
        out.write_all(HEADER.as_bytes()).map_err(fail)?;
        out.write_all(body.as_bytes()).map_err(fail)?;
        out.sync_all().map_err(fail)?;
    }
    std::fs::rename(&tmp, &file).map_err(fail)?;
    // The rename is durable only once the directory entry is.
    File::open(root)
        .and_then(|dir| dir.sync_all())
        .map_err(fail)
}

/// Fold a store from before `settings.toml` into it, once, then delete it.
///
/// Runs only when `legacy` exists. If `section` is already in the file, the
/// store was migrated before and only the deletion is left to do (a crash
/// between the two). Otherwise `import` reads `legacy` and writes what it
/// holds through [`Sections::set`]; it should set nothing until it has read
/// everything.
///
/// Every failure keeps the settings, never the error:
/// - `settings.toml` itself does not parse: nothing happens, and the next
///   open tries again once it is fixed.
/// - `import` fails: the old store is renamed to `<name>.unreadable` for a
///   human to look at, so it is not retried, and logged, on every open.
pub fn migrate_legacy(
    root: &Path,
    legacy: &Path,
    section: &str,
    import: impl FnOnce(&Path, &mut Sections) -> Result<(), String>,
) {
    if !legacy.exists() {
        return;
    }
    let mut unreadable = None;
    let migrated = update(root, |sections| {
        if sections.has(section) {
            return Ok(false);
        }
        if let Err(e) = import(legacy, sections) {
            unreadable = Some(e);
            return Ok(false);
        }
        Ok(true)
    });
    match (migrated, unreadable) {
        (Err(e), _) => log::error!(
            "[settings] {} not migrated yet, settings.toml cannot be written: {e}",
            legacy.display()
        ),
        (Ok(_), Some(e)) => {
            log::error!(
                "[settings] could not read {}: {e}; moved aside, its settings are lost",
                legacy.display()
            );
            set_aside_legacy(legacy);
        }
        (Ok(migrated), None) => {
            if migrated {
                log::info!("[settings] migrated {} into {FILE_NAME}", legacy.display());
            }
            remove_legacy(legacy);
        }
    }
}

/// Delete a store from before `settings.toml`, with the `-wal` and `-shm`
/// files SQLite keeps beside one. Missing files are fine.
pub fn remove_legacy(legacy: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = legacy.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

fn set_aside_legacy(legacy: &Path) {
    let mut name = legacy.as_os_str().to_owned();
    name.push(".unreadable");
    if let Err(e) = std::fs::rename(legacy, PathBuf::from(name)) {
        log::error!("[settings] could not move {} aside: {e}", legacy.display());
    }
    for suffix in ["-wal", "-shm"] {
        let mut name = legacy.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

/// An exclusive `flock` on the lock file, released when dropped (the
/// descriptor closes).
struct Lock {
    _file: File,
}

impl Lock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let lock_path = root.join(LOCK_NAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("{}: {e}", lock_path.display()))?;
        // Blocking: a holder keeps it for one small read and write.
        loop {
            // SAFETY: flock on a descriptor this function owns.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                return Ok(Self { _file: file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("{}: {err}", lock_path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Demo {
        name: String,
        count: u32,
        enabled: bool,
    }

    impl Default for Demo {
        fn default() -> Self {
            Self {
                name: "default".into(),
                count: 0,
                enabled: true,
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "qbz-settings-file-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn a_missing_file_reads_as_defaults_and_creates_nothing() {
        let root = scratch("missing");
        assert_eq!(read_section::<Demo>(&root, "demo"), Demo::default());
        assert!(!has_section(&root, "demo"));
        assert!(!root.exists(), "a read must not create the data root");
    }

    #[test]
    fn a_section_round_trips_and_leaves_the_others_alone() {
        let root = scratch("roundtrip");
        update_section(&root, "other", |o: &mut Demo| o.count = 7).unwrap();
        update_section(&root, "demo", |d: &mut Demo| {
            d.name = "kitchen".into();
            d.enabled = false;
        })
        .unwrap();

        let demo: Demo = read_section(&root, "demo");
        assert_eq!(demo.name, "kitchen");
        assert!(!demo.enabled);
        assert_eq!(read_section::<Demo>(&root, "other").count, 7);
        assert!(has_section(&root, "demo") && has_section(&root, "other"));
        let text = std::fs::read_to_string(path(&root)).unwrap();
        assert!(text.starts_with("# pibuz settings."), "{text}");
        assert!(
            !root.join(TMP_NAME).exists(),
            "the temporary must be renamed away"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_key_missing_from_the_file_takes_its_default() {
        let root = scratch("partial");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(path(&root), "[demo]\ncount = 3\n").unwrap();
        let demo: Demo = read_section(&root, "demo");
        assert_eq!(demo.count, 3);
        assert_eq!(demo.name, "default");
        assert!(demo.enabled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_does_not_parse_is_read_as_defaults_and_never_overwritten() {
        let root = scratch("broken");
        std::fs::create_dir_all(&root).unwrap();
        let broken = "[demo]\nname = \"unterminated\n[other]\ncount = 5\n";
        std::fs::write(path(&root), broken).unwrap();

        assert_eq!(read_section::<Demo>(&root, "demo"), Demo::default());
        let err = update_section(&root, "demo", |d: &mut Demo| d.count = 1).unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
        assert!(
            err.contains(FILE_NAME),
            "the error must name the file: {err}"
        );
        assert_eq!(std::fs::read_to_string(path(&root)).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_section_of_the_wrong_shape_is_never_overwritten() {
        let root = scratch("wrong-shape");
        std::fs::create_dir_all(&root).unwrap();
        let text = "[demo]\ncount = \"three\"\n";
        std::fs::write(path(&root), text).unwrap();

        assert_eq!(read_section::<Demo>(&root, "demo"), Demo::default());
        let err = update_section(&root, "demo", |d: &mut Demo| d.count = 1).unwrap_err();
        assert!(
            err.contains("[demo]"),
            "the error must name the section: {err}"
        );
        assert_eq!(std::fs::read_to_string(path(&root)).unwrap(), text);
        // A different, healthy section can still be written.
        update_section(&root, "other", |o: &mut Demo| o.count = 2).unwrap();
        assert_eq!(read_section::<Demo>(&root, "other").count, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_update_that_changes_nothing_does_not_rewrite_the_file() {
        let root = scratch("no-op");
        update_section(&root, "demo", |d: &mut Demo| d.count = 1).unwrap();
        let before = std::fs::metadata(path(&root)).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        update_section(&root, "demo", |d: &mut Demo| d.count = 1).unwrap();
        let after = std::fs::metadata(path(&root)).unwrap().modified().unwrap();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The lost-update race the lock exists for: many writers each doing a
    /// read-modify-write of the SAME key. Without the lock most increments are
    /// lost; with it the total is exact. `flock` conflicts between two opens
    /// in one process exactly as between two processes, so threads are a
    /// faithful stand-in for `pibuz settings set` racing the daemon.
    #[test]
    fn concurrent_writers_never_lose_an_update() {
        let root = scratch("concurrent");
        let writers = 8;
        let each = 10;
        let handles: Vec<_> = (0..writers)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for _ in 0..each {
                        update_section(&root, "demo", |d: &mut Demo| d.count += 1).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(read_section::<Demo>(&root, "demo").count, writers * each);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Readers take no lock, so they must never see a half-written file.
    #[test]
    fn a_reader_racing_a_writer_always_sees_a_whole_file() {
        let root = scratch("torn");
        update_section(&root, "demo", |d: &mut Demo| d.name = "x".repeat(4096)).unwrap();
        let writer = {
            let root = root.clone();
            std::thread::spawn(move || {
                for i in 0..100 {
                    update_section(&root, "demo", |d: &mut Demo| d.count = i + 1).unwrap();
                }
            })
        };
        while !writer.is_finished() {
            let sections = load(&root).expect("a reader saw a file that does not parse");
            let demo: Demo = sections.get("demo").unwrap();
            assert_eq!(demo.name.len(), 4096, "a reader saw a truncated section");
        }
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
    fn plant_legacy(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let legacy = root.join("old_store.db");
        std::fs::write(&legacy, b"old").unwrap();
        std::fs::write(root.join("old_store.db-wal"), b"wal").unwrap();
        std::fs::write(root.join("old_store.db-shm"), b"shm").unwrap();
        legacy
    }

    fn import_demo(count: u32) -> impl FnOnce(&Path, &mut Sections) -> Result<(), String> {
        move |legacy, sections| {
            assert_eq!(std::fs::read(legacy).unwrap(), b"old");
            sections.set(
                "demo",
                &Demo {
                    count,
                    ..Demo::default()
                },
            )
        }
    }

    #[test]
    fn a_legacy_store_is_migrated_once_and_then_deleted_with_its_siblings() {
        let root = scratch("migrate");
        let legacy = plant_legacy(&root);
        migrate_legacy(&root, &legacy, "demo", import_demo(42));

        assert_eq!(read_section::<Demo>(&root, "demo").count, 42);
        for name in ["old_store.db", "old_store.db-wal", "old_store.db-shm"] {
            assert!(!root.join(name).exists(), "{name} must be deleted");
        }
        // Nothing left to migrate: the importer must not run again.
        migrate_legacy(&root, &legacy, "demo", |_, _| panic!("migrated twice"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_section_already_in_the_file_wins_over_a_leftover_legacy_store() {
        // A crash between writing settings.toml and deleting the old store.
        let root = scratch("migrate-leftover");
        update_section(&root, "demo", |d: &mut Demo| d.count = 7).unwrap();
        let legacy = plant_legacy(&root);
        migrate_legacy(&root, &legacy, "demo", |_, _| panic!("must not import"));
        assert_eq!(read_section::<Demo>(&root, "demo").count, 7);
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_legacy_store_is_set_aside_not_retried() {
        let root = scratch("migrate-unreadable");
        let legacy = plant_legacy(&root);
        migrate_legacy(&root, &legacy, "demo", |_, _| Err("not a database".into()));
        assert!(!legacy.exists());
        assert!(root.join("old_store.db.unreadable").exists());
        assert!(!has_section(&root, "demo"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_legacy_store_waits_while_settings_toml_does_not_parse() {
        let root = scratch("migrate-blocked");
        let legacy = plant_legacy(&root);
        std::fs::write(path(&root), "[demo\n").unwrap();
        migrate_legacy(&root, &legacy, "demo", |_, _| panic!("must not import"));
        assert!(legacy.exists(), "kept for when the file is fixed");

        std::fs::write(path(&root), "").unwrap();
        migrate_legacy(&root, &legacy, "demo", import_demo(3));
        assert_eq!(read_section::<Demo>(&root, "demo").count, 3);
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_legacy_store_means_no_file_is_created() {
        let root = scratch("migrate-none");
        migrate_legacy(&root, &root.join("old_store.db"), "demo", |_, _| {
            panic!("nothing to import")
        });
        assert!(!root.exists());
    }
}
