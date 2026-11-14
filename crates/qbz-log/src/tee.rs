//! The composite [`log::Log`] that fans every record to stderr and the file.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

use log::{Log, Metadata, Record};

use crate::line::LogLine;
use crate::redact;

/// Wraps `env_logger`'s built `Logger` and tees every record to stderr and
/// (optionally) the on-disk log file, with secret redaction applied once at this single
/// write choke point. Both sinks receive the redacted text.
pub(crate) struct TeeLogger {
    pub(crate) inner: env_logger::Logger,
    /// Unbuffered on purpose: each line is one `write_all`, on disk when `log`
    /// returns. Logging is a few lines a second, and the file is what is read
    /// after a crash.
    pub file: Option<Mutex<File>>,
}

fn now_epoch_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// Format a redacted log line the same way the file sink does (stable, greppable).
fn format_line(line: &LogLine) -> String {
    format!(
        "{} {:5} {} {}",
        line.format_ts(),
        line.level_str(),
        line.target,
        line.message
    )
}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        // Honor the inner logger's filter, for both sinks.
        if !self.inner.enabled(record.metadata()) {
            return;
        }

        // Redact ONCE; every downstream sink gets the cleaned text.
        let msg = redact::redact(&record.args().to_string());
        let line = LogLine {
            ts: now_epoch_ms(),
            level: record.level(),
            target: record.target().to_owned(),
            message: msg.clone(),
        };

        if let Some(file) = &self.file {
            if let Ok(mut writer) = file.lock() {
                let mut text = format_line(&line);
                text.push('\n');
                let _ = writer.write_all(text.as_bytes());
            }
        }

        // stderr: write the redacted line ourselves. Delegating to
        // `self.inner.log(record)` would reprint the *original* Record args
        // and bypass redaction (terminal transcripts, CI logs, support dumps).
        let _ = writeln!(std::io::stderr(), "{}", format_line(&line));
    }

    fn flush(&self) {
        self.inner.flush();
        if let Some(file) = &self.file {
            if let Ok(mut writer) = file.lock() {
                let _ = writer.flush();
            }
        }
        let _ = std::io::stderr().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    #[test]
    fn format_line_includes_redacted_message_not_raw() {
        let line = LogLine {
            ts: 0,
            level: Level::Info,
            target: "qbz".into(),
            message: "token=***REDACTED***".into(),
        };
        let s = format_line(&line);
        assert!(s.contains("***REDACTED***"));
        assert!(!s.contains("SEKRET"));
    }

    /// A line is on disk when `log` returns. The file used to go through an
    /// 8 KB buffer that nothing flushed, so on the Pi the log read 0 bytes for
    /// the first stretch of every run while stderr already had it all, and a
    /// crash lost whatever was still in the buffer: the lines that explain it.
    #[test]
    fn a_logged_line_is_in_the_file_at_once() {
        let path = std::env::temp_dir().join(format!(
            "qbz-log-tee-{}-{}.log",
            std::process::id(),
            now_epoch_ms()
        ));
        let file = File::create(&path).expect("create");
        let logger = TeeLogger {
            inner: env_logger::Builder::new()
                .filter_level(log::LevelFilter::Info)
                .build(),
            file: Some(Mutex::new(file)),
        };
        logger.log(
            &Record::builder()
                .level(Level::Info)
                .target("pibuz")
                .args(format_args!("the daemon started"))
                .build(),
        );
        let on_disk = std::fs::read_to_string(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        assert!(
            on_disk.contains("the daemon started") && on_disk.ends_with('\n'),
            "{on_disk:?}"
        );
    }
}
