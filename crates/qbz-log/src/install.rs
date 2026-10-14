//! One-shot logger installation + the on-disk file sink (open / rotate).

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::tee::TeeLogger;

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install the [`TeeLogger`] as the global `log` logger.
///
/// Builds the inner `env_logger` logger from `RUST_LOG` (falling back to `default_level`),
/// opens/rotates the on-disk file, then sets the boxed logger + max level. Idempotent:
/// a second call is a guarded no-op (it neither rotates the file again nor panics).
pub fn install(default_level: &str) {
    // True one-shot guard: avoid re-rotating the log file or fighting an already-set logger.
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    // Demote `tracing` in the DEFAULT filter: its log bridge emits span
    // events at INFO. An explicit RUST_LOG still replaces the whole default.
    let inner = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(format!("{default_level},tracing=warn")),
    )
    .build();
    let level = inner.filter();
    let file = open_log_file();

    // Ignore the Err if a logger was somehow already set elsewhere.
    if log::set_boxed_logger(Box::new(TeeLogger { inner, file })).is_ok() {
        log::set_max_level(level);
    }
}

/// Path to the current-run log file (`~/.local/share/qbz/logs/qbz.log`), if a data dir exists.
pub(crate) fn log_file_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("qbz").join("logs").join("qbz.log"))
}

/// Open the log file for this run, rotating any previous one to `qbz.log.prev`.
/// Returns `None` (file sink disabled, gracefully) on any filesystem error.
fn open_log_file() -> Option<Mutex<BufWriter<File>>> {
    let path = log_file_path()?;
    let dir = path.parent()?;
    std::fs::create_dir_all(dir).ok()?;

    if path.exists() {
        let prev = dir.join("qbz.log.prev");
        // Best-effort rotation; a failure here must not disable logging.
        let _ = std::fs::rename(&path, &prev);
    }

    let file = File::create(&path).ok()?;
    Some(Mutex::new(BufWriter::new(file)))
}
