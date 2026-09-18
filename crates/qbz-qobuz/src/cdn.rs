//! One shared HTTP client for every Qobuz CDN fetch.
//!
//! The stream probe, the streaming feeder, the CMAF segment fetchers and the
//! whole-track fallbacks used to build a `reqwest::Client` each, per track — six
//! construction sites, several of them more than once per track. That cost more
//! than it looks:
//!
//! * Every `Client::builder().build()` decodes the whole webpki root store.
//! * Every client starts with an EMPTY connection pool, so nothing is reused —
//!   the probe and the download in `remote_stream.rs` hit the SAME url and still
//!   could not share a connection.
//! * rustls caches TLS sessions PER CLIENT, so a fresh client per track means a
//!   full handshake every time, never a resumption. On a Pi the handshake is CPU
//!   as well as latency.
//!
//! One process-lifetime client fixes all three: warm pool, warm session cache,
//! roots parsed once.
//!
//! # Timeouts (read this before changing them)
//!
//! `read_timeout`, NOT `timeout`. reqwest's `timeout` is a TOTAL deadline that
//! covers the streamed body, and the feeder's buffer window rate-matches the
//! download to playback — so a five-minute track legitimately takes five
//! minutes and a total deadline severs every one of them. What we actually want
//! to catch is a STALL, which is what a read timeout measures. (This is the
//! reasoning `remote_stream.rs`'s download client was already built on; it now
//! lives here, where it governs every CDN fetch.)
//!
//! A caller that genuinely wants a total deadline — a small, bounded fetch like
//! the format probe — adds `.timeout(..)` to its own `RequestBuilder`, which
//! overrides the client's for that request only. Never add one here.

use std::sync::OnceLock;
use std::time::Duration;

/// Browser UA. The CDN edge treats an unrecognized agent differently, and every
/// call site set this by hand before; it is a client default now so no fetch can
/// forget it.
const CDN_USER_AGENT: &str = "Mozilla/5.0";

/// Bound the TCP connect phase so a dead route (a stale CDN address) cannot hang
/// a fetch indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Stall detector: no bytes for this long on an open body is a broken transfer,
/// not a slow one. Generous on purpose — a rate-matched feeder is idle by
/// design whenever the buffer window is full.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

static CDN_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// The shared CDN client.
///
/// Fallible so every call site keeps the error handling it already had; in
/// practice `build()` only fails if the TLS backend cannot start. Two threads
/// racing the first call may each build one and discard the loser — harmless,
/// and cheaper than holding a lock across the build.
pub fn client() -> Result<&'static reqwest::Client, String> {
    if let Some(client) = CDN_CLIENT.get() {
        return Ok(client);
    }
    let built = reqwest::Client::builder()
        .user_agent(CDN_USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| format!("CDN client error: {e}"))?;
    Ok(CDN_CLIENT.get_or_init(|| built))
}

#[cfg(test)]
mod tests {
    /// The client is built once and handed back by reference, so repeated calls
    /// are the same client — which is the entire point of the module (a fresh
    /// client per call would mean a cold pool and no TLS resumption).
    #[test]
    fn the_client_is_built_once_and_shared() {
        // The workspace pins reqwest's `rustls-tls-webpki-roots-no-provider`,
        // so the process-level CryptoProvider must exist before any client is
        // built or `build()` panics with "No provider set". `pibuz` installs it
        // in `main`; a test binary has no `main` (same dance as `redact.rs`).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let first = super::client().expect("first build");
        let second = super::client().expect("second call");
        assert!(
            std::ptr::eq(first, second),
            "cdn::client() handed back a different client on the second call"
        );
    }
}
