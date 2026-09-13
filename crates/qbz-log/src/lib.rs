//! `qbz-log` — frontend-agnostic logging core for the qbz desktop client.
//!
//! It owns a composite [`log::Log`] implementation ([`tee::TeeLogger`]) that wraps
//! `env_logger`'s built `Logger` and fans every record out to two sinks:
//!   1. **stderr** (redacted text, same line format as the file sink), and
//!   2. an **on-disk file** (`~/.local/share/qbz/logs/qbz.log`, prev-rotated at startup).
//!
//! There was a third: a 5000-line in-memory ring, for the desktop log viewer and
//! its "copy diagnostics bundle" button. Both went with the GUI, and the ring
//! went on being FILLED on every log line with nothing left to read it.
//!
//! Secret **redaction** ([`redact`]) is applied once at the single write choke point,
//! so every downstream consumer (stderr, ring, file, clipboard, paste upload) gets clean text.
//!
//! This crate is network-free and UI-free: no `reqwest`, no `tokio`, no `slint`.

pub mod install;
mod line;
pub mod redact;
pub mod tee;

pub use install::install;
pub use redact::{redact, register_secret};
