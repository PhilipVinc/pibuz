//! Local pairing surface for Qobuz Connect: mDNS advertisement + the three
//! `/streamcore` HTTP endpoints the Qobuz app calls when a user picks this
//! device in the output selector.
//!
//! Protocol (verified against the qobuz-proxy and StreamCore32 receivers):
//!   - the device advertises `_qobuz-connect._tcp.local.` with TXT properties
//!     `path=/streamcore`, `type=SPEAKER`, `sdk_version`, `Name`, `device_uuid`;
//!   - the app then calls `GET /streamcore/get-display-info` (picker metadata),
//!     `GET /streamcore/get-connect-info` (`{current_session_id, app_id}`), and
//!     finally `POST /streamcore/connect-to-qconnect` with
//!     `{session_id, jwt_qconnect: {jwt, exp, endpoint}, jwt_api: {jwt, exp}}`.
//!
//! The handed-over `jwt_qconnect` is the SAME credential class
//! `/qws/createToken` returns (`WsTransportConfig.jwt_qws` + endpoint), but
//! minted for the CASTER's session — so any Qobuz account on the LAN can hand
//! this device a session, with last-write-wins takeover semantics. The tokens
//! land in the in-memory [`PairingStore`]; `DaemonQconnectService::connect`
//! prefers a live pairing token over `/qws/createToken` discovery.
//!
//! The handed-over `jwt_api` is the streaming credential:
//! `reconnect_for_pairing` installs it on the `QobuzClient` as its
//! `Authorization: Bearer`, and the refresh heartbeat below keeps both JWTs
//! alive past their expiry. When it cannot — the api credential expires or is
//! refused — `retire_dead_pairing` drops the session rather than leave a
//! connected-looking device that 401s every play.
//! Tokens are never persisted (a daemon restart just waits for the next
//! handoff POST) and never logged (registered with the qbz-log redactor on
//! receipt and on every refresh).
//!
//! This is a SEPARATE tiny_http listener from the `/api` control plane: the
//! Qobuz app is an unauthenticated LAN client, so it must not be subject to
//! the control plane's Origin shield / opt-in Bearer gate — and the pairing
//! surface must not widen the control plane's attack surface either.

use std::io::Cursor;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mdns_sd::{ServiceDaemon, ServiceInfo};
use qconnect_transport_ws::WsTransportConfig;
use serde_json::{json, Value};
use tiny_http::{Method, Request, Response, Server};

use super::transport::{
    default_qconnect_device_info, device_type_name, resolve_qconnect_device_type,
    resolve_qconnect_device_uuid,
};
use super::DaemonQconnectService;
// The response formatter is shared with the control plane deliberately: it is
// a pure serializer, so reusing it does not couple the two listeners.
use crate::api::json;

const MDNS_SERVICE_TYPE: &str = "_qobuz-connect._tcp.local.";
const SDK_VERSION: &str = concat!("qbz-", env!("CARGO_PKG_VERSION"));
/// Treat a token expiring within this window as already expired (clock skew +
/// time to finish the WS handshake).
const EXP_SLACK_SECS: u64 = 60;

/// Tokens handed over by the Qobuz app in `connect-to-qconnect`. `exp` values
/// are unix seconds as sent on the wire; `0` = not provided (treated as
/// non-expiring — nothing in this repo decodes JWTs to find out more).
/// `*_issued_ms` is when THIS process received the token (handoff or
/// refresh), in unix milliseconds: the wire carries no issue time, and the
/// refresh heartbeat needs one to find the token's half-life.
#[derive(Clone)]
pub(crate) struct PairingTokens {
    pub session_id: String,
    pub(crate) ws_jwt: String,
    pub(crate) ws_exp: u64,
    pub(crate) ws_issued_ms: u64,
    pub(crate) ws_endpoint: String,
    /// Streaming credential (`Authorization: Bearer`), installed on the client
    /// by `reconnect_for_pairing`.
    pub(crate) api_jwt: Option<String>,
    pub(crate) api_exp: u64,
    pub(crate) api_issued_ms: u64,
    /// When the client started SENDING this api credential (its Bearer
    /// install), on the process-monotonic [`mono_ms`] clock; `None` until
    /// then. The 401 check compares load start times against it, so it must
    /// be the install, not the receipt: between the two, loads still go out
    /// with the previous credential. And monotonic, because the Pi has no RTC
    /// and its wall clock can step at boot.
    pub(crate) api_in_use_since: Option<u64>,
    /// This api credential was renewed BECAUSE a load 401'd on the one before
    /// it. A 401 on a load that started after it was installed is then the
    /// second strike: renewal is not curing it, so the pairing is retired
    /// instead of renewed again (see `judge_suspicion`).
    pub(crate) api_renewed_on_suspicion: bool,
}

impl PairingTokens {
    fn ws_token_live(&self, now: u64) -> bool {
        self.ws_exp == 0 || self.ws_exp > now + EXP_SLACK_SECS
    }
}

/// The redactor covers registered values in LOG lines, but a `{:?}` in a panic
/// message or `dbg!` bypasses it — so Debug elides the raw JWTs entirely.
impl std::fmt::Debug for PairingTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingTokens")
            .field("session_id", &self.session_id)
            .field("ws_jwt", &"<redacted>")
            .field("ws_exp", &self.ws_exp)
            .field("ws_issued_ms", &self.ws_issued_ms)
            .field("ws_endpoint", &self.ws_endpoint)
            .field("api_jwt", &self.api_jwt.as_ref().map(|_| "<redacted>"))
            .field("api_exp", &self.api_exp)
            .field("api_issued_ms", &self.api_issued_ms)
            .field("api_in_use_since", &self.api_in_use_since)
            .field("api_renewed_on_suspicion", &self.api_renewed_on_suspicion)
            .finish()
    }
}

/// Last handed-over tokens, shared between the pairing listener (writer) and
/// the connect/reconnect paths (readers). In-memory only — see module doc.
pub(crate) type PairingStore = Arc<StdMutex<Option<PairingTokens>>>;

fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Milliseconds on a process-monotonic clock, for ORDERING events (a load's
/// start against a credential's install) — immune to the wall clock stepping.
/// Never 0, which callers use for "unknown".
pub(crate) fn mono_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
    u64::try_from(elapsed.as_millis())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A clone of the stored tokens if their WS credential is still usable.
/// Expired tokens are dropped from the store on the way out, so the connect
/// path falls back to `/qws/createToken` instead of retrying a dead JWT.
pub(crate) fn valid_ws_tokens(store: &PairingStore) -> Option<PairingTokens> {
    let mut guard = store.lock().ok()?;
    match guard.as_ref() {
        Some(tokens) if tokens.ws_token_live(now_secs()) => Some(tokens.clone()),
        Some(_) => {
            log::info!("[QConnect/Pairing] handed-over WS token expired; dropping it");
            *guard = None;
            None
        }
        None => None,
    }
}

/// Build the WS transport config from handed-over tokens. Field-for-field the
/// hardened defaults `resolve_transport_config` uses (require_jwt, 60s idle
/// retry, the default connectionId/backend/controllers channels).
pub(crate) fn transport_config_from(tokens: &PairingTokens) -> WsTransportConfig {
    WsTransportConfig {
        endpoint_url: tokens.ws_endpoint.clone(),
        jwt_qws: Some(tokens.ws_jwt.clone()),
        require_jwt: true,
        reconnect_idle_retry_ms: 60_000,
        subscribe_channels: vec![vec![0x01], vec![0x02], vec![0x03]],
        ..Default::default()
    }
}

/// Parse a `connect-to-qconnect` body. Only the WS connect token is mandatory:
/// without `{jwt, endpoint}` there is nothing to connect to.
fn parse_connect_request(body: &Value) -> Result<PairingTokens, String> {
    let session_id = body
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let ws = body
        .get("jwt_qconnect")
        .ok_or_else(|| "missing jwt_qconnect".to_string())?;
    let ws_jwt = ws
        .get("jwt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "missing jwt_qconnect.jwt".to_string())?
        .to_string();
    let ws_endpoint = ws
        .get("endpoint")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "missing jwt_qconnect.endpoint".to_string())?
        .to_string();
    let ws_exp = ws.get("exp").and_then(Value::as_u64).unwrap_or(0);

    let (api_jwt, api_exp) = match body.get("jwt_api") {
        Some(api) => (
            api.get("jwt")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(ToString::to_string),
            api.get("exp").and_then(Value::as_u64).unwrap_or(0),
        ),
        None => (None, 0),
    };

    let now_ms = now_ms();
    let tokens = PairingTokens {
        session_id,
        ws_jwt,
        ws_exp,
        ws_issued_ms: now_ms,
        ws_endpoint,
        api_jwt,
        api_exp,
        api_issued_ms: now_ms,
        api_in_use_since: None,
        api_renewed_on_suspicion: false,
    };
    if !tokens.ws_token_live(now_ms / 1000) {
        return Err("jwt_qconnect is already expired".to_string());
    }
    Ok(tokens)
}

/// mDNS instance names cannot carry spaces or exotic characters; keep
/// alphanumerics/`-`/`_`, collapse runs of `-`. The original display name still
/// travels in the TXT `Name` property.
fn sanitize_instance_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "Pibuz".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Owner handle for the pairing surface: the mDNS registration + the
/// `/streamcore` listener thread. Shut down BEFORE the qconnect disconnect so
/// no handoff can land mid-teardown, and joined so its
/// `Arc<DaemonQconnectService>` (→ `Arc<AppRuntime>`) clone drops ahead of
/// `drop(booted)` (the #521 clock-release ordering).
pub(crate) struct PairingHandle {
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
    mdns: Option<(ServiceDaemon, String)>,
}

impl PairingHandle {
    pub(crate) fn shutdown(&mut self) {
        if let Some((daemon, fullname)) = self.mdns.take() {
            let _ = daemon.unregister(&fullname);
            let _ = daemon.shutdown();
        }
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Bind the pairing listener, register the mDNS service, and start serving.
/// mDNS failure is non-fatal (the listener still serves for apps that probe by
/// IP, and boot must not die on a flaky avahi/Bonjour) — bind failure is an
/// error because the advertised surface would point at nothing.
pub(crate) fn spawn(
    port: u16,
    friendly_name: &str,
    service: Arc<DaemonQconnectService>,
    rt: tokio::runtime::Handle,
) -> Result<PairingHandle, String> {
    let server = Server::http(("0.0.0.0", port))
        .map_err(|err| format!("bind pairing listener on port {port}: {err}"))?;
    let server = Arc::new(server);

    let mdns = match register_mdns(port, friendly_name) {
        Ok(pair) => Some(pair),
        Err(err) => {
            log::warn!("[QConnect/Pairing] mDNS registration failed (serving anyway): {err}");
            None
        }
    };

    let srv = Arc::clone(&server);
    let thread = std::thread::Builder::new()
        .name("pibuz-pairing".into())
        .spawn(move || {
            for mut req in srv.incoming_requests() {
                let resp = handle(&service, &rt, &mut req);
                let _ = req.respond(resp);
            }
        })
        .map_err(|err| format!("spawn pairing thread: {err}"))?;

    log::info!(
        "[QConnect/Pairing] serving /streamcore on port {port} as \"{friendly_name}\"{}",
        if mdns.is_some() {
            " (mDNS registered)"
        } else {
            ""
        }
    );
    Ok(PairingHandle {
        server,
        thread: Some(thread),
        mdns,
    })
}

fn register_mdns(port: u16, friendly_name: &str) -> Result<(ServiceDaemon, String), String> {
    let daemon = ServiceDaemon::new().map_err(|err| format!("mdns daemon: {err}"))?;
    let instance = sanitize_instance_name(friendly_name);
    let device_uuid = resolve_qconnect_device_uuid();
    let properties = [
        ("path", "/streamcore"),
        ("type", pairing_device_type()),
        ("sdk_version", SDK_VERSION),
        ("Name", friendly_name),
        ("device_uuid", device_uuid.as_str()),
    ];
    // Host label derived from the instance (not the machine hostname) so we
    // never fight avahi/Bonjour over the machine's own `<hostname>.local.`
    // A-record; enable_addr_auto fills in the real interface addresses.
    let info = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &instance,
        &format!("{instance}.local."),
        "",
        port,
        &properties[..],
    )
    .map_err(|err| format!("mdns service info: {err}"))?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon
        .register(info)
        .map_err(|err| format!("mdns register: {err}"))?;
    Ok((daemon, fullname))
}

// ---------------------------------------------------------------------------
// Token refresh (StreamCore32 parity): `POST /qws/refreshToken` with the
// Bearer api credential renews jwt_api (body `jwt=jwt_api`) and jwt_qws (body
// `jwt=jwt_qws`), so a paired, account-less device can outlive its handed-over
// tokens' expiry without the app re-casting.
//
// The body names the KIND of token wanted, not the token: what authenticates
// the call is the api credential in the Bearer header. So the api credential
// is the whole pairing. While it lives, either token can be renewed; once it
// has expired, Qobuz answers 401 and nothing this device holds can get it
// back. Only a fresh handoff can.
//
// That is why renewal starts at HALF-LIFE, not just ahead of expiry. On
// 2026-11-07 refreshToken answered 500 for the whole five minutes ahead of a
// one-hour token's expiry; the old five-minute lead had no slack left, the
// credential expired, and every stream after it was a 401 behind a WS session
// that still looked connected. Half-life gives a 30-minute outage room to
// clear, at the cost of one extra refresh an hour.
// ---------------------------------------------------------------------------

const REFRESH_CHECK_SECS: u64 = 30;
/// Ceiling on one refreshToken round trip; a timeout is `Transient`.
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Where one token stands at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenPhase {
    /// Less than half its lifetime used (or no expiry on the wire).
    Fresh,
    /// Past half-life and not yet expired: renew it, and keep retrying every
    /// tick until that lands.
    Renew,
    /// Expired. For the api credential this ends the pairing.
    Expired,
}

/// `issued` is when this process received the token, `exp` its expiry (both
/// unix seconds). `exp == 0` (no expiry on the wire) is always fresh.
fn token_phase(issued: u64, exp: u64, now: u64) -> TokenPhase {
    if exp == 0 {
        TokenPhase::Fresh
    } else if now >= exp {
        TokenPhase::Expired
    } else if now >= issued + exp.saturating_sub(issued) / 2 {
        TokenPhase::Renew
    } else {
        TokenPhase::Fresh
    }
}

/// Daemon-lifetime refresh heartbeat: a no-op until a handoff populates the
/// store. Held by `QconnectHandle` and abort+joined at shutdown (it clones
/// the service, and through it `Arc<AppRuntime>` — #521 ordering, same
/// contract as the report scheduler).
///
/// It also answers [`AuthSuspect`]: a load the engine saw refused with a 401.
/// Both arms run in THIS task, one after the other, never concurrently: two
/// api renewals in flight would each install a credential, and if Qobuz
/// invalidates the older one on renewal, the loser could be left installed.
pub(crate) fn spawn_token_refresh(
    service: Arc<DaemonQconnectService>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(REFRESH_CHECK_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => refresh_if_needed(&service).await,
                () = service.auth_suspect.notified() => verify_suspicion(&service).await,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// A 401 on playback. The heartbeat above only knows a credential is dead by
// the clock or by refreshToken refusing it, so a credential Qobuz stops
// accepting early (logout, password change — never observed, but nothing
// rules it out) would leave the session up behind 401s until the next
// renewal: the 2026-11-07 symptom for up to half a token's life.
//
// So the engine reports a load refused with "Authentication failed", and the
// heartbeat checks it with ONE renewal ("one retry"). That renewal settles
// less than it looks: refreshToken may accept a credential the stream
// endpoint refuses. So if the renewal succeeds, the NEXT 401 — on a load that
// started after the renewed credential was installed — retires the pairing
// rather than renewing again. Every failure is stamped with when its load
// started, so a burst of failures from one gesture, or a leftover from a
// previous credential or handoff, can never count against a newer one.
// ---------------------------------------------------------------------------

/// Retire reasons, as they land in `last_errors.auth`.
const EXPIRED_REASON: &str = "the Qobuz Connect credential expired before it could be renewed";
const REFUSED_REASON: &str = "Qobuz refused the Qobuz Connect credential when renewing it";
const REFUSED_AFTER_RENEWAL_REASON: &str =
    "Qobuz refused to stream with the Qobuz Connect credential even after renewing it";

/// "A load was refused with a 401": from the renderer engine (writer) to the
/// refresh heartbeat (reader). Carries the LATEST start time of a refused
/// load, so the heartbeat can tell which credential the refusal was about.
#[derive(Default)]
pub(crate) struct AuthSuspect {
    notify: tokio::sync::Notify,
    latest_failed_load_ms: std::sync::atomic::AtomicU64,
}

impl AuthSuspect {
    /// A load that started at `load_started_ms` ([`mono_ms`]) was refused
    /// with a 401. Synchronous and lock-free: it is called from the engine's
    /// `on_load_failed`, on the session event loop.
    pub(crate) fn report(&self, load_started_ms: u64) {
        self.latest_failed_load_ms
            .fetch_max(load_started_ms, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_one();
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }

    fn latest_failed_load_ms(&self) -> u64 {
        self.latest_failed_load_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// What a reported 401 means for the credential in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suspicion {
    /// Not about this credential: its load started before the credential was
    /// put to use (a burst from one gesture, a previous credential, a
    /// previous handoff), or there is no api credential to judge.
    Ignore,
    /// Renew it once; a renewed credential that is refused again is retired.
    Renew,
    /// Nothing will cure it: retire the pairing, for this reason.
    Retire(&'static str),
}

/// How long after a suspicion renewal a 401 still counts as its second
/// strike. Past it, a 401 gets its own one retry: a credential that played
/// fine for a while and then hits one bad answer is not retired on sight.
const SECOND_STRIKE_WINDOW_MS: u64 = 10 * 60 * 1000;

/// Pure decision behind [`verify_suspicion`]. `failed_load_started` is the
/// latest start of a load refused with a 401, on the [`mono_ms`] clock
/// (0 = unknown, ignored); `now_secs` is the wall clock, for the expiry.
fn judge_suspicion(tokens: &PairingTokens, failed_load_started: u64, now_secs: u64) -> Suspicion {
    // Not in use yet (a handoff whose takeover has not installed it, a
    // renewal between store write and Bearer install), or in use only after
    // the refused load began: the refusal was of a previous credential.
    let Some(in_use_since) = tokens.api_in_use_since else {
        return Suspicion::Ignore;
    };
    if tokens.api_jwt.is_none() || failed_load_started <= in_use_since {
        Suspicion::Ignore
    } else if token_phase(tokens.api_issued_ms / 1000, tokens.api_exp, now_secs)
        == TokenPhase::Expired
    {
        Suspicion::Retire(EXPIRED_REASON)
    } else if tokens.api_renewed_on_suspicion
        && failed_load_started - in_use_since < SECOND_STRIKE_WINDOW_MS
    {
        Suspicion::Retire(REFUSED_AFTER_RENEWAL_REASON)
    } else {
        Suspicion::Renew
    }
}

async fn verify_suspicion(service: &DaemonQconnectService) {
    let Some(snapshot) = service
        .pairing_store()
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
    else {
        return;
    };
    let failed_load_started_ms = service.auth_suspect.latest_failed_load_ms();
    match judge_suspicion(&snapshot, failed_load_started_ms, now_secs()) {
        Suspicion::Ignore => {}
        Suspicion::Retire(reason) => service.retire_dead_pairing(&snapshot, reason).await,
        Suspicion::Renew => {
            log::info!(
                "[QConnect/Pairing] a load was refused (401); renewing the credential once \
                 to tell a stale credential from a dead one"
            );
            renew(service, &snapshot, true, false, true).await;
        }
    }
}

/// Extract `{jwt, exp[, endpoint]}` from a refreshToken token payload.
/// `None` when the jwt is missing/empty; a missing `exp` maps to 0.
fn parse_refresh_payload(payload: &Value) -> Option<(String, u64, Option<String>)> {
    let jwt = payload
        .get("jwt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let exp = payload.get("exp").and_then(Value::as_u64).unwrap_or(0);
    let endpoint = payload
        .get("endpoint")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    Some((jwt, exp, endpoint))
}

/// Identity of a handoff: session + the ws credential it arrived with (which
/// no refresh arm has touched yet within a tick).
fn same_handoff(a: &PairingTokens, b: &PairingTokens) -> bool {
    a.session_id == b.session_id && a.ws_jwt == b.ws_jwt
}

/// Write a refreshed token back ONLY if the store still holds the handoff the
/// refresh was computed from. Guards two races across the HTTP await: a new
/// `connect-to-qconnect` handoff replacing the store (a stale refresh must not
/// corrupt the new caster's tokens), and the operator disconnect clearing it
/// (a stale refresh must not resurrect anything). Returns whether it landed.
fn install_refreshed(
    store: &PairingStore,
    snapshot: &PairingTokens,
    update: impl FnOnce(&mut PairingTokens),
) -> bool {
    let Ok(mut guard) = store.lock() else {
        return false;
    };
    match guard.as_mut() {
        Some(tokens) if same_handoff(tokens, snapshot) => {
            update(tokens);
            true
        }
        _ => false,
    }
}

/// Stamp the store's api credential as in use from now (its Bearer was just
/// installed), if the store still holds `snapshot`'s handoff (see
/// `PairingTokens::api_in_use_since`).
pub(super) fn mark_api_in_use(store: &PairingStore, snapshot: &PairingTokens) {
    install_refreshed(store, snapshot, |tokens| {
        tokens.api_in_use_since = Some(mono_ms());
    });
}

/// Empty the store ONLY if it still holds the handoff `snapshot` came from —
/// the retire-a-dead-credential twin of [`install_refreshed`]. A handoff that
/// landed while the dead one was being discovered owns the store now and must
/// survive. Returns whether it cleared anything.
pub(super) fn clear_if_same_handoff(store: &PairingStore, snapshot: &PairingTokens) -> bool {
    let Ok(mut guard) = store.lock() else {
        return false;
    };
    match guard.as_ref() {
        Some(tokens) if same_handoff(tokens, snapshot) => {
            *guard = None;
            true
        }
        _ => false,
    }
}

async fn refresh_if_needed(service: &DaemonQconnectService) {
    let Some(snapshot) = service
        .pairing_store()
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
    else {
        return;
    };
    // The refresh endpoint itself authenticates with the Bearer api credential;
    // without one (degenerate handoff) there is nothing we can renew.
    if snapshot.api_jwt.is_none() {
        return;
    }
    let now = now_secs();
    let api_phase = token_phase(snapshot.api_issued_ms / 1000, snapshot.api_exp, now);
    if api_phase == TokenPhase::Expired {
        service.retire_dead_pairing(&snapshot, EXPIRED_REASON).await;
        return;
    }
    let api_due = api_phase == TokenPhase::Renew;
    // An expired ws token is still renewable: the call mints a fresh one from
    // the (live) api credential.
    let ws_due =
        token_phase(snapshot.ws_issued_ms / 1000, snapshot.ws_exp, now) != TokenPhase::Fresh;
    if api_due || ws_due {
        renew(service, &snapshot, api_due, ws_due, false).await;
    }
}

/// Renew the api and/or ws credential of the handoff `snapshot` was taken
/// from. `on_suspicion`: this api renewal answers a 401 on playback, so the
/// renewed credential carries the second-strike mark (see `judge_suspicion`);
/// a scheduled renewal clears it.
async fn renew(
    service: &DaemonQconnectService,
    snapshot: &PairingTokens,
    api_due: bool,
    ws_due: bool,
    on_suspicion: bool,
) {
    let store = service.pairing_store();
    let Some(mut api_jwt) = snapshot.api_jwt.clone() else {
        return;
    };
    let Some(client) = service.runtime.core().client().read().await.clone() else {
        return;
    };
    let left = snapshot.api_exp.saturating_sub(now_secs());

    if api_due {
        match refresh_jwt(&client, &api_jwt, "jwt_api").await {
            Ok(payload) => {
                if let Some((jwt, exp, _)) = parse_refresh_payload(&payload) {
                    qbz_log::register_secret(jwt.clone());
                    let landed = install_refreshed(&store, snapshot, |tokens| {
                        apply_api_renewal(tokens, &jwt, exp, now_ms(), on_suspicion);
                    });
                    if landed {
                        api_jwt = jwt;
                        // Client install only when the store write landed: the
                        // store changing under us means a newer handoff or an
                        // operator disconnect owns the Bearer slot now.
                        client.set_bearer_api_token(Some(api_jwt.clone())).await;
                        // Re-check after that await: a handoff landing in this
                        // exact window owns the Bearer slot — hand back its
                        // credential instead of leaving ours installed.
                        let hijacked = store
                            .lock()
                            .map(|guard| match guard.as_ref() {
                                Some(tokens) => !same_handoff(tokens, snapshot),
                                None => true,
                            })
                            .unwrap_or(true);
                        if hijacked {
                            let current = store.lock().ok().and_then(|guard| {
                                guard.as_ref().and_then(|tokens| tokens.api_jwt.clone())
                            });
                            client.set_bearer_api_token(current).await;
                            log::info!(
                                "[QConnect/Pairing] yielded the Bearer slot to a newer handoff"
                            );
                            return;
                        }
                        // In use from now: a load that started between the store
                        // write and the Bearer install went out with the OLD
                        // credential, and its 401 must not count against this one.
                        mark_api_in_use(&store, snapshot);
                        log::info!("[QConnect/Pairing] refreshed jwt_api (exp {exp})");
                    } else {
                        log::info!(
                            "[QConnect/Pairing] dropped stale jwt_api refresh (store changed)"
                        );
                        return;
                    }
                } else {
                    log::warn!("[QConnect/Pairing] jwt_api refresh answered without a jwt");
                }
            }
            Err(RefreshError::Rejected(err)) => {
                log::warn!("[QConnect/Pairing] jwt_api refresh rejected: {err}");
                service.retire_dead_pairing(snapshot, REFUSED_REASON).await;
                return;
            }
            Err(RefreshError::Transient(err)) => log::warn!(
                "[QConnect/Pairing] jwt_api refresh failed ({left} s before it expires; \
                 retrying every {REFRESH_CHECK_SECS} s): {err}"
            ),
        }
    }

    if ws_due {
        match refresh_jwt(&client, &api_jwt, "jwt_qws").await {
            Ok(payload) => {
                if let Some((jwt, exp, endpoint)) = parse_refresh_payload(&payload) {
                    qbz_log::register_secret(jwt.clone());
                    let landed = install_refreshed(&store, snapshot, |tokens| {
                        tokens.ws_jwt = jwt.clone();
                        tokens.ws_exp = exp;
                        tokens.ws_issued_ms = now_ms();
                        if let Some(endpoint) = endpoint {
                            tokens.ws_endpoint = endpoint;
                        }
                    });
                    if landed {
                        // The LIVE WS connection keeps its old token; the fresh
                        // one is what the reconnect credential re-resolve uses.
                        log::info!("[QConnect/Pairing] refreshed jwt_qws (exp {exp})");
                    } else {
                        log::info!(
                            "[QConnect/Pairing] dropped stale jwt_qws refresh (store changed)"
                        );
                    }
                } else {
                    log::warn!("[QConnect/Pairing] jwt_qws refresh answered without a jwt");
                }
            }
            // The ws arm authenticates with the same api credential, so a
            // rejection here is the api credential's death too. Unless the api
            // arm just renewed it, in which case the snapshot no longer names
            // the store's credential — the next tick re-reads and decides.
            Err(RefreshError::Rejected(err)) if !api_due => {
                log::warn!("[QConnect/Pairing] jwt_qws refresh rejected: {err}");
                service.retire_dead_pairing(snapshot, REFUSED_REASON).await;
            }
            Err(RefreshError::Rejected(err) | RefreshError::Transient(err)) => log::warn!(
                "[QConnect/Pairing] jwt_qws refresh failed ({left} s before the api \
                 credential expires; retrying every {REFRESH_CHECK_SECS} s): {err}"
            ),
        }
    }
}

/// A renewed api credential replaces the old one, received at `now_ms`.
/// Answering a 401 (`on_suspicion`) marks it for the second strike; a
/// scheduled renewal clears the mark, so one stale suspicion cannot hang over
/// every credential after it.
fn apply_api_renewal(
    tokens: &mut PairingTokens,
    jwt: &str,
    exp: u64,
    now_ms: u64,
    on_suspicion: bool,
) {
    tokens.api_jwt = Some(jwt.to_string());
    tokens.api_exp = exp;
    tokens.api_issued_ms = now_ms;
    // In use only once the client sends it: `mark_api_in_use` after the
    // Bearer install.
    tokens.api_in_use_since = None;
    tokens.api_renewed_on_suspicion = on_suspicion;
}

/// Why a refreshToken round-trip failed.
#[derive(Debug)]
enum RefreshError {
    /// 401: Qobuz refuses the Bearer credential. Retrying cannot help.
    Rejected(String),
    /// Anything else (5xx, network, a malformed body): worth retrying while
    /// the credential is still alive.
    Transient(String),
}

/// Only a 401 says the credential itself is refused. A 403 does NOT: Qobuz
/// answers 403 in storms that have nothing to do with the credential (the
/// client keeps a circuit breaker for them, issue #637), and treating one as
/// fatal would drop a working session on a bad minute. A credential that
/// really is dead still ends at its expiry.
fn refresh_status_rejects_the_credential(status: u16) -> bool {
    status == 401
}

/// One `POST /qws/refreshToken` round-trip; returns the renewed token payload
/// (`{jwt, exp[, endpoint]}`). Status-before-decode like the createToken path.
async fn refresh_jwt(
    client: &qbz_qobuz::QobuzClient,
    bearer: &str,
    kind: &str,
) -> Result<Value, RefreshError> {
    let app_id = client.app_id().await.map_err(|err| {
        RefreshError::Transient(format!(
            "refreshToken requires initialized API client: {err}"
        ))
    })?;
    let url = qbz_qobuz::endpoints::build_url("/qws/refreshToken");
    let response = client
        .get_http()
        .post(&url)
        .header("X-App-Id", app_id)
        .header("Authorization", format!("Bearer {bearer}"))
        .form(&[("jwt", kind)])
        // The whole round trip, body included: the client only bounds the TCP
        // connect, and one stalled answer would otherwise freeze the heartbeat
        // — and with it expiry handling and the 401 check — for good.
        .timeout(REFRESH_TIMEOUT)
        .send()
        .await
        .map_err(|err| {
            RefreshError::Transient(format!("refreshToken HTTP request failed: {err}"))
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|err| {
        RefreshError::Transient(format!("refreshToken response read failed: {err}"))
    })?;
    if !status.is_success() {
        let preview = body.trim().chars().take(300).collect::<String>();
        let message = format!("refreshToken status {status}: {preview}");
        return Err(if refresh_status_rejects_the_credential(status.as_u16()) {
            RefreshError::Rejected(message)
        } else {
            RefreshError::Transient(message)
        });
    }
    let payload: Value = serde_json::from_str(&body).map_err(|err| {
        RefreshError::Transient(format!("refreshToken response decode failed: {err}"))
    })?;
    payload.get(kind).cloned().ok_or_else(|| {
        RefreshError::Transient(format!("refreshToken response missing {kind} payload"))
    })
}

// ---------------------------------------------------------------------------
// Request handling (runs on the pibuz-pairing thread; async work reaches the
// tokio runtime through the captured Handle, same pattern as ApiState.rt).
// ---------------------------------------------------------------------------

fn handle(
    service: &Arc<DaemonQconnectService>,
    rt: &tokio::runtime::Handle,
    req: &mut Request,
) -> Response<Cursor<Vec<u8>>> {
    let path = req.url().split('?').next().unwrap_or_default().to_owned();
    match (req.method(), path.as_str()) {
        (Method::Get, "/streamcore/get-display-info") => json(200, display_info()),
        (Method::Get, "/streamcore/get-connect-info") => {
            let session_id = valid_ws_tokens(&service.pairing_store())
                .map(|t| t.session_id)
                .unwrap_or_default();
            let app_id = rt.block_on(service.current_app_id()).unwrap_or_default();
            json(
                200,
                json!({ "current_session_id": session_id, "app_id": app_id }),
            )
        }
        (Method::Post, "/streamcore/connect-to-qconnect") => {
            // Unauthenticated LAN endpoint: cap the body read (a real handoff
            // is well under 8 KB) so a hostile client can't stream gigabytes
            // into daemon memory.
            use std::io::Read as _;
            let mut body = String::new();
            let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
            let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            match parse_connect_request(&body) {
                Ok(tokens) => {
                    // Never let a handed-over credential reach a log line.
                    qbz_log::register_secret(tokens.ws_jwt.clone());
                    if let Some(api_jwt) = &tokens.api_jwt {
                        qbz_log::register_secret(api_jwt.clone());
                    }
                    log::info!(
                        "[QConnect/Pairing] handoff received (session {}…); taking over",
                        tokens.session_id.chars().take(8).collect::<String>()
                    );
                    if let Ok(mut guard) = service.pairing_store().lock() {
                        *guard = Some(tokens);
                    }
                    // Ack the app immediately (it watches the cloud session for
                    // the device joining, not this response) and run the
                    // disconnect+connect takeover off the serving thread. The
                    // task is tracked on the service: a newer handoff aborts
                    // it, and shutdown aborts it before the final disconnect.
                    let takeover = Arc::clone(service);
                    let task = rt.spawn(async move {
                        if let Err(err) = takeover.reconnect_for_pairing().await {
                            log::warn!("[QConnect/Pairing] takeover connect failed: {err}");
                        }
                    });
                    service.replace_takeover_task(task);
                    json(200, json!({}))
                }
                Err(err) => json(400, json!({ "error": err })),
            }
        }
        _ => json(404, json!({ "error": "not found" })),
    }
}

/// The LAN surface names the same DeviceType the WS join announces, so the
/// picker and the cloud's device list agree on what this box is.
fn pairing_device_type() -> &'static str {
    device_type_name(resolve_qconnect_device_type()).unwrap_or("STREAMER")
}

/// `get-display-info` — picker metadata, sourced from the same canonical device
/// identity the WS join advertises. The display quality vocabulary is
/// MP3 | LOSSLESS | HIRES_L1 (24/96) | HIRES_L3 (24/192); our wire capability
/// is hires_l2 = 192 kHz decode, i.e. HIRES_L3 in display terms.
fn display_info() -> Value {
    let info = default_qconnect_device_info();
    json!({
        "type": pairing_device_type(),
        "friendly_name": info.friendly_name.unwrap_or_default(),
        "model_display_name": info.model.unwrap_or_default(),
        "brand_display_name": info.brand.unwrap_or_default(),
        "serial_number": info.device_uuid.unwrap_or_default(),
        "max_audio_quality": "HIRES_L3",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_body(jwt: &str, exp: u64, endpoint: &str) -> Value {
        json!({
            "session_id": "sess-1234",
            "jwt_qconnect": { "jwt": jwt, "exp": exp, "endpoint": endpoint },
            "jwt_api": { "jwt": "api-jwt", "exp": exp }
        })
    }

    #[test]
    fn parse_accepts_a_full_handoff() {
        let far_future = now_secs() + 3600;
        let tokens =
            parse_connect_request(&ws_body("ws-jwt", far_future, "wss://qws.example")).unwrap();
        assert_eq!(tokens.session_id, "sess-1234");
        assert_eq!(tokens.ws_jwt, "ws-jwt");
        assert_eq!(tokens.ws_endpoint, "wss://qws.example");
        assert_eq!(tokens.api_jwt.as_deref(), Some("api-jwt"));
    }

    #[test]
    fn parse_rejects_missing_or_expired_ws_token() {
        assert!(parse_connect_request(&json!({})).is_err());
        assert!(parse_connect_request(&json!({
            "jwt_qconnect": { "jwt": "x", "exp": 0 } // no endpoint
        }))
        .is_err());
        // Already expired (exp in the past) must be refused at the door.
        assert!(parse_connect_request(&ws_body("ws-jwt", 1000, "wss://qws.example")).is_err());
    }

    #[test]
    fn parse_treats_missing_exp_as_non_expiring() {
        let body = json!({
            "session_id": "s",
            "jwt_qconnect": { "jwt": "ws-jwt", "endpoint": "wss://e" }
        });
        let tokens = parse_connect_request(&body).unwrap();
        assert_eq!(tokens.ws_exp, 0);
        assert!(tokens.ws_token_live(now_secs()));
        assert!(tokens.api_jwt.is_none());
    }

    #[test]
    fn ws_token_live_boundary_honors_the_slack() {
        let tokens = |exp| PairingTokens {
            session_id: "s".into(),
            ws_jwt: "jwt".into(),
            ws_exp: exp,
            ws_issued_ms: 0,
            ws_endpoint: "wss://e".into(),
            api_jwt: None,
            api_exp: 0,
            api_issued_ms: 0,
            api_in_use_since: None,
            api_renewed_on_suspicion: false,
        };
        let now = 1_000_000;
        // Strictly greater than now + slack is required: exactly at the slack
        // edge counts as expired.
        assert!(!tokens(now + EXP_SLACK_SECS).ws_token_live(now));
        assert!(tokens(now + EXP_SLACK_SECS + 1).ws_token_live(now));
    }

    #[test]
    fn debug_never_prints_the_jwts() {
        let rendered = format!(
            "{:?}",
            PairingTokens {
                session_id: "s".into(),
                ws_jwt: "SECRET-WS".into(),
                ws_exp: 0,
                ws_issued_ms: 0,
                ws_endpoint: "wss://e".into(),
                api_jwt: Some("SECRET-API".into()),
                api_exp: 0,
                api_issued_ms: 0,
                api_in_use_since: None,
                api_renewed_on_suspicion: false,
            }
        );
        assert!(!rendered.contains("SECRET-WS"));
        assert!(!rendered.contains("SECRET-API"));
    }

    #[test]
    fn store_drops_expired_tokens_on_read() {
        let store: PairingStore = Arc::new(StdMutex::new(Some(PairingTokens {
            session_id: "s".into(),
            ws_jwt: "jwt".into(),
            ws_exp: 1, // long expired
            ws_issued_ms: 0,
            ws_endpoint: "wss://e".into(),
            api_jwt: None,
            api_exp: 0,
            api_issued_ms: 0,
            api_in_use_since: None,
            api_renewed_on_suspicion: false,
        })));
        assert!(valid_ws_tokens(&store).is_none());
        assert!(
            store.lock().unwrap().is_none(),
            "expired entry must be cleared"
        );
    }

    #[test]
    fn transport_config_mirrors_the_hardened_defaults() {
        let tokens = PairingTokens {
            session_id: "s".into(),
            ws_jwt: "jwt".into(),
            ws_exp: 0,
            ws_issued_ms: 0,
            ws_endpoint: "wss://e".into(),
            api_jwt: None,
            api_exp: 0,
            api_issued_ms: 0,
            api_in_use_since: None,
            api_renewed_on_suspicion: false,
        };
        let config = transport_config_from(&tokens);
        assert_eq!(config.endpoint_url, "wss://e");
        assert_eq!(config.jwt_qws.as_deref(), Some("jwt"));
        assert!(config.require_jwt);
        assert_eq!(config.reconnect_idle_retry_ms, 60_000);
        assert_eq!(
            config.subscribe_channels,
            vec![vec![0x01], vec![0x02], vec![0x03]]
        );
    }

    #[test]
    fn token_phase_renews_from_half_life_until_expiry() {
        let issued = 1_000_000;
        let exp = issued + 3600;
        assert_eq!(
            token_phase(issued, 0, issued),
            TokenPhase::Fresh,
            "no expiry"
        );
        assert_eq!(token_phase(issued, exp, issued), TokenPhase::Fresh);
        assert_eq!(token_phase(issued, exp, issued + 1799), TokenPhase::Fresh);
        assert_eq!(token_phase(issued, exp, issued + 1800), TokenPhase::Renew);
        assert_eq!(token_phase(issued, exp, exp - 1), TokenPhase::Renew);
        assert_eq!(token_phase(issued, exp, exp), TokenPhase::Expired);
        // Handed over already past its own expiry (clock skew): dead at once,
        // not "renew forever".
        assert_eq!(token_phase(exp + 10, exp, exp + 10), TokenPhase::Expired);
    }

    /// Replays the heartbeat over three hours of 30 s ticks against a fake
    /// refreshToken that answers 500 for `outage` (seconds after the first
    /// handoff) and otherwise issues a fresh one-hour token. Returns when the
    /// api credential was found expired, if it was.
    fn credential_death(outage: std::ops::Range<u64>) -> Option<u64> {
        const LIFETIME: u64 = 3600;
        let (mut issued, mut exp) = (0, LIFETIME);
        for now in (0..3 * LIFETIME).step_by(REFRESH_CHECK_SECS as usize) {
            match token_phase(issued, exp, now) {
                TokenPhase::Fresh => {}
                TokenPhase::Renew if outage.contains(&now) => {}
                TokenPhase::Renew => (issued, exp) = (now, now + LIFETIME),
                TokenPhase::Expired => return Some(now),
            }
        }
        None
    }

    #[test]
    fn the_2026_11_07_outage_no_longer_kills_the_pairing() {
        // refreshToken answered 500 from five minutes before the expiry of a
        // one-hour token until past it. The old five-minute lead lost the
        // credential to exactly this; half-life renews it long before.
        assert_eq!(credential_death(3300..3900), None);
        // Nor does an outage that begins right at half-life, as long as it
        // clears before the expiry.
        assert_eq!(credential_death(1800..3500), None);
    }

    #[test]
    fn an_outage_through_the_whole_second_half_is_found_at_expiry() {
        // No renewal can land: the credential is lost, and the heartbeat must
        // say so at the expiry itself, so the session is dropped right away
        // instead of left up behind a dead credential.
        assert_eq!(credential_death(1800..3600), Some(3600));
    }

    // ---- a 401 on playback ----

    /// A paired credential received and put to use at `issued_ms`, expiring
    /// an hour later. (The tests use one timeline for both clocks: only the
    /// ordering of stamps, and the expiry against `now`, matter here.)
    fn paired(issued_ms: u64) -> PairingTokens {
        PairingTokens {
            session_id: "s1".into(),
            ws_jwt: "ws1".into(),
            ws_exp: issued_ms / 1000 + 3600,
            ws_issued_ms: issued_ms,
            ws_endpoint: "wss://e".into(),
            api_jwt: Some("api1".into()),
            api_exp: issued_ms / 1000 + 3600,
            api_issued_ms: issued_ms,
            api_in_use_since: Some(issued_ms),
            api_renewed_on_suspicion: false,
        }
    }

    const T: u64 = 1_800_000_000_000; // unix ms

    #[test]
    fn judge_suspicion_counts_only_refusals_of_the_current_credential() {
        let tokens = paired(T);
        let now = (T + 100_000) / 1000;
        assert_eq!(
            judge_suspicion(&tokens, 0, now),
            Suspicion::Ignore,
            "no stamp"
        );
        // A load that started before the credential was in use went out with
        // the previous one.
        assert_eq!(judge_suspicion(&tokens, T - 1, now), Suspicion::Ignore);
        assert_eq!(judge_suspicion(&tokens, T, now), Suspicion::Ignore);
        assert_eq!(judge_suspicion(&tokens, T + 1, now), Suspicion::Renew);

        let mut no_api = tokens.clone();
        no_api.api_jwt = None;
        assert_eq!(judge_suspicion(&no_api, T + 1, now), Suspicion::Ignore);

        let mut second_strike = tokens.clone();
        second_strike.api_renewed_on_suspicion = true;
        assert_eq!(
            judge_suspicion(&second_strike, T + 1, now),
            Suspicion::Retire(REFUSED_AFTER_RENEWAL_REASON)
        );

        // Already past its expiry: no renewal can work, retire at once.
        assert_eq!(
            judge_suspicion(&tokens, T + 1, (T + 3_600_000) / 1000),
            Suspicion::Retire(EXPIRED_REASON)
        );

        // Received but not yet installed: a takeover still on its way, or a
        // renewal between store write and Bearer install. Whatever 401'd went
        // out with the previous credential.
        let mut not_in_use = tokens.clone();
        not_in_use.api_in_use_since = None;
        assert_eq!(
            judge_suspicion(&not_in_use, T + 50_000, now),
            Suspicion::Ignore
        );
    }

    /// The heartbeat's answer to 401s, over the production decision
    /// (`judge_suspicion`) and the production renewal (`apply_api_renewal`),
    /// with refreshToken scripted.
    struct Heartbeat {
        tokens: PairingTokens,
        renewals: u32,
        retired: Option<&'static str>,
    }

    impl Heartbeat {
        fn paired_at(issued_ms: u64) -> Self {
            Self {
                tokens: paired(issued_ms),
                renewals: 0,
                retired: None,
            }
        }

        /// Woken at `now_ms` by a 401 whose load started at `load_started_ms`;
        /// refreshToken, if asked, answers `renewal_ok` (else a 401).
        fn wake(&mut self, load_started_ms: u64, now_ms: u64, renewal_ok: bool) {
            if self.retired.is_some() {
                return;
            }
            match judge_suspicion(&self.tokens, load_started_ms, now_ms / 1000) {
                Suspicion::Ignore => {}
                Suspicion::Retire(reason) => self.retired = Some(reason),
                Suspicion::Renew => {
                    self.renewals += 1;
                    if renewal_ok {
                        let exp = now_ms / 1000 + 3600;
                        apply_api_renewal(&mut self.tokens, "renewed", exp, now_ms, true);
                        // ... and the Bearer install that puts it in use.
                        self.tokens.api_in_use_since = Some(now_ms);
                    } else {
                        self.retired = Some(REFUSED_REASON);
                    }
                }
            }
        }
    }

    #[test]
    fn a_burst_of_refusals_from_one_gesture_renews_once() {
        // 2026-11-08 10:05:22: four queue items refused within 0.7 s. The
        // heartbeat answers the first; the other three loads had all started
        // before its renewal landed, so they say nothing about the renewed
        // credential.
        let mut heartbeat = Heartbeat::paired_at(T);
        heartbeat.wake(T + 1_000, T + 1_100, true);
        for started in [T + 1_020, T + 1_050, T + 1_090] {
            heartbeat.wake(started, T + 1_700, true);
        }
        assert_eq!(heartbeat.renewals, 1);
        assert_eq!(heartbeat.retired, None, "a cured credential stays paired");
    }

    #[test]
    fn a_refusal_after_the_renewal_retires_instead_of_renewing_again() {
        // refreshToken may accept a credential the stream endpoint refuses.
        // Renewing on every 401 would then loop forever behind a session that
        // looks connected — the 2026-11-07 symptom. The second strike ends it.
        let mut heartbeat = Heartbeat::paired_at(T);
        heartbeat.wake(T + 1_000, T + 1_100, true);
        heartbeat.wake(T + 60_000, T + 60_100, true);
        assert_eq!(heartbeat.renewals, 1);
        assert_eq!(heartbeat.retired, Some(REFUSED_AFTER_RENEWAL_REASON));
    }

    #[test]
    fn a_refusal_long_after_the_renewal_gets_its_own_retry() {
        // The renewal cured it; ten minutes of good playback later a single
        // 401 is a new suspicion, not the second strike of the old one.
        let mut heartbeat = Heartbeat::paired_at(T);
        heartbeat.wake(T + 1_000, T + 1_100, true);
        heartbeat.wake(
            T + 1_100 + SECOND_STRIKE_WINDOW_MS + 1,
            T + 1_100 + SECOND_STRIKE_WINDOW_MS + 100,
            true,
        );
        assert_eq!(heartbeat.renewals, 2);
        assert_eq!(heartbeat.retired, None);
    }

    #[test]
    fn a_renewal_refused_with_401_retires_at_once() {
        let mut heartbeat = Heartbeat::paired_at(T);
        heartbeat.wake(T + 1_000, T + 1_100, false);
        assert_eq!(heartbeat.retired, Some(REFUSED_REASON));
    }

    #[test]
    fn a_refusal_left_over_from_the_previous_handoff_is_ignored() {
        // The old session's loads 401'd (which is why the user re-picked);
        // the suspicion is answered only after the new handoff is in use.
        let mut heartbeat = Heartbeat::paired_at(T);
        heartbeat.tokens = paired(T + 30_000); // the new handoff
        heartbeat.wake(T + 20_000, T + 31_000, true);
        assert_eq!(heartbeat.renewals, 0);
        assert_eq!(heartbeat.retired, None);
    }

    #[test]
    fn a_scheduled_renewal_clears_the_second_strike() {
        let mut heartbeat = Heartbeat::paired_at(T);
        heartbeat.wake(T + 1_000, T + 1_100, true);
        assert!(heartbeat.tokens.api_renewed_on_suspicion);
        // A few minutes later the heartbeat renews on schedule ...
        apply_api_renewal(&mut heartbeat.tokens, "scheduled", 0, T + 300_000, false);
        heartbeat.tokens.api_in_use_since = Some(T + 300_000);
        // ... and a later 401, still inside the window of the first renewal,
        // gets its own one retry again.
        heartbeat.wake(T + 400_000, T + 400_100, true);
        assert_eq!(heartbeat.renewals, 2);
        assert_eq!(heartbeat.retired, None);
    }

    #[test]
    fn auth_suspect_keeps_the_latest_refused_load() {
        let suspect = AuthSuspect::default();
        assert_eq!(suspect.latest_failed_load_ms(), 0);
        suspect.report(T + 50);
        suspect.report(T + 10); // reported later, started earlier
        assert_eq!(suspect.latest_failed_load_ms(), T + 50);
    }

    #[test]
    fn only_a_401_from_refresh_token_rejects_the_credential() {
        assert!(refresh_status_rejects_the_credential(401));
        // 403 storms are not about the credential (issue #637).
        for status in [403, 429, 500, 502, 503] {
            assert!(!refresh_status_rejects_the_credential(status), "{status}");
        }
    }

    #[test]
    fn clear_if_same_handoff_spares_a_newer_handoff() {
        let original = PairingTokens {
            session_id: "s1".into(),
            ws_jwt: "jwt1".into(),
            ws_exp: 0,
            ws_issued_ms: 0,
            ws_endpoint: "wss://e".into(),
            api_jwt: Some("api1".into()),
            api_exp: 0,
            api_issued_ms: 0,
            api_in_use_since: None,
            api_renewed_on_suspicion: false,
        };
        // A newer handoff landed while the old credential was found dead: it
        // owns the store and must survive.
        let mut newer = original.clone();
        newer.ws_jwt = "jwt2".into();
        let store: PairingStore = Arc::new(StdMutex::new(Some(newer)));
        assert!(!clear_if_same_handoff(&store, &original));
        assert!(store.lock().unwrap().is_some());
        // A refresh renewing the api credential does not change the handoff's
        // identity, so the dead one's own entry is still found and cleared.
        let mut renewed = original.clone();
        renewed.api_jwt = Some("api2".into());
        *store.lock().unwrap() = Some(renewed);
        assert!(clear_if_same_handoff(&store, &original));
        assert!(store.lock().unwrap().is_none());
        // Already cleared (operator disconnect): nothing to do.
        assert!(!clear_if_same_handoff(&store, &original));
    }

    #[test]
    fn parse_refresh_payload_requires_a_jwt() {
        assert!(parse_refresh_payload(&json!({})).is_none());
        assert!(parse_refresh_payload(&json!({ "jwt": "", "exp": 5 })).is_none());
        assert_eq!(
            parse_refresh_payload(&json!({ "jwt": "j" })),
            Some(("j".to_string(), 0, None))
        );
        assert_eq!(
            parse_refresh_payload(&json!({ "jwt": "j", "exp": 7, "endpoint": "wss://e" })),
            Some(("j".to_string(), 7, Some("wss://e".to_string())))
        );
    }

    #[test]
    fn install_refreshed_rejects_a_changed_store() {
        let original = PairingTokens {
            session_id: "s1".into(),
            ws_jwt: "jwt1".into(),
            ws_exp: 0,
            ws_issued_ms: 0,
            ws_endpoint: "wss://e".into(),
            api_jwt: Some("api1".into()),
            api_exp: 0,
            api_issued_ms: 0,
            api_in_use_since: None,
            api_renewed_on_suspicion: false,
        };
        // Same handoff still in the store -> the write lands.
        let store: PairingStore = Arc::new(StdMutex::new(Some(original.clone())));
        assert!(install_refreshed(&store, &original, |t| t.api_exp = 42));
        assert_eq!(store.lock().unwrap().as_ref().unwrap().api_exp, 42);
        // A newer handoff (different ws_jwt) -> the stale write is dropped.
        store.lock().unwrap().as_mut().unwrap().ws_jwt = "jwt2".into();
        assert!(!install_refreshed(&store, &original, |t| t.api_exp = 99));
        assert_eq!(store.lock().unwrap().as_ref().unwrap().api_exp, 42);
        // Operator disconnect cleared the store -> nothing is resurrected.
        *store.lock().unwrap() = None;
        assert!(!install_refreshed(&store, &original, |t| t.api_exp = 99));
        assert!(store.lock().unwrap().is_none());
    }

    #[test]
    fn instance_name_is_mdns_safe() {
        assert_eq!(
            sanitize_instance_name("Pibuz (studio-pi)"),
            "Pibuz-studio-pi"
        );
        assert_eq!(sanitize_instance_name("Living Room"), "Living-Room");
        assert_eq!(sanitize_instance_name("  ***  "), "Pibuz");
    }
}
