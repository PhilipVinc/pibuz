//! Nudging a running daemon to reload its settings.
//!
//! These lived in `login.rs` until the account path was removed, which made
//! plain what they always were: not authentication at all, but the local IPC
//! every `qbzd settings set` uses to tell a running daemon that something on
//! disk changed. They outlived login because they were never part of it.

use crate::paths::ProfileRoots;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Three-state outcome of the ping-then-reload nudge. 04-settings-portability.md
/// §5.3 step 7 needs "daemon simply not running" (not an error) distinguished
/// from "daemon up but the reload was refused/500" (exit 1 with the restart
/// hint) — a single bool conflates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeOutcome {
    /// Ping answered and the reload returned 2xx.
    Reloaded,
    /// Ping did not answer — no daemon to nudge (never an error).
    DaemonDown,
    /// Ping answered but the reload did not return 2xx.
    ReloadRefused,
}

/// Best-effort `GET /api/ping` → `POST /api/settings/reload` against a local
/// daemon. `token` carries the opt-in `[server] token` as
/// `Authorization: Bearer` when present; T5 callers pass `None`.
pub fn nudge_reload_outcome(host: &str, token: Option<&str>) -> NudgeOutcome {
    if !http_request_2xx(host, "GET", "/api/ping", token) {
        return NudgeOutcome::DaemonDown;
    }
    if http_request_2xx(host, "POST", "/api/settings/reload", token) {
        NudgeOutcome::Reloaded
    } else {
        NudgeOutcome::ReloadRefused
    }
}

/// Boolean skin over [`nudge_reload_outcome`] for the callers that only need
/// "did a running daemon acknowledge?" (login/logout/`settings set` — they are
/// specified to work daemon-down, 02 §2.2, so any non-reload is just "the
/// daemon picks it up on next start").
pub fn nudge_reload(host: &str, token: Option<&str>) -> bool {
    nudge_reload_outcome(host, token) == NudgeOutcome::Reloaded
}

/// The local daemon's reload address. Credentials are written to the LOCAL
/// config root, so the daemon to nudge is always local; its port comes from the
/// same `qbzd.toml` the daemon reads (default 8182).
pub(crate) fn nudge_host(roots: &ProfileRoots) -> String {
    let port = crate::config::QbzdConfig::load(&roots.config.join("qbzd.toml"))
        .map(|(c, _)| c.server.port)
        .unwrap_or(8182);
    format!("127.0.0.1:{port}")
}

fn http_request_2xx(host: &str, method: &str, path: &str, token: Option<&str>) -> bool {
    let addr = match host.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(600)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let _ = stream.flush();
    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).unwrap_or(0);
    let status = String::from_utf8_lossy(&buf[..n]);
    matches!(
        status.lines().next().and_then(|l| l.split_whitespace().nth(1)),
        Some(code) if code.starts_with('2')
    )
}
