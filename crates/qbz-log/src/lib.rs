//! `qbz-log` — the logging core for pibuz.
//!
//! It owns a composite [`log::Log`] implementation ([`tee::TeeLogger`]) that wraps
//! `env_logger`'s built `Logger` and fans every record out to two sinks:
//!   1. **stderr** (redacted text, same line format as the file sink), and
//!   2. an **on-disk file** (`~/.local/share/qbz/logs/qbz.log`, prev-rotated at startup).
//!
//! Secret **redaction** ([`redact`]) is applied once at the single write choke point,
//! so both sinks get clean text.
//!
//! This crate is network-free: no `reqwest`, no `tokio`.

pub mod install;
mod line;
pub mod redact;
pub mod tee;

pub use install::install;
pub use redact::{redact, register_secret};
