//! A single captured log record.

use log::Level;

/// One captured log line. `message` is **already redacted** by the time it is
/// written to stderr or the file (redaction happens at the single write choke point in
/// [`crate::tee::TeeLogger`]).
#[derive(Clone)]
pub(crate) struct LogLine {
    /// Capture timestamp, epoch milliseconds (local-clock derived).
    pub(crate) ts: i64,
    /// Severity.
    pub(crate) level: Level,
    /// `log` target (usually the module path).
    pub(crate) target: String,
    /// Redacted message text.
    pub message: String,
}

impl LogLine {
    /// Fixed-width uppercase level label (`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`).
    pub(crate) fn level_str(&self) -> &'static str {
        match self.level {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// Human-readable local timestamp (`YYYY-MM-DD HH:MM:SS.mmm`) from the epoch ms.
    /// Falls back to the raw epoch value if the timestamp is somehow out of range.
    pub(crate) fn format_ts(&self) -> String {
        use chrono::{Local, TimeZone};
        match Local.timestamp_millis_opt(self.ts).single() {
            Some(dt) => dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            None => self.ts.to_string(),
        }
    }
}
