//! QConnect transport config + credential discovery + device identity for the
//! pibuz daemon.
//!
//! Ports the desktop `qconnect_transport.rs` config layer. The Qobuz client is
//! reached via `runtime.core().client()` — an `Arc<RwLock<Option<QobuzClient>>>`
//! (may be `None` before the API is initialized). Two daemon adaptations vs. the
//! desktop copy (§1.4, §4): (1) the device-uuid + settings-DB path resolve
//! against the DAEMON root's `qconnect_settings.db` — its OWN device_uuid, minted
//! fresh, NEVER the desktop's global KV; (2) the default device name is
//! "Pibuz (hostname)" (OD6). The KV load/persist helpers are path-parameterized
//! `*_at` variants so T11 (settings) and T13 (TUI) drive the same daemon-root DB.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use qbz_app::playback_driver;
use qbz_app::settings::daemon_prefs;
use qbz_app::shell::AppRuntime;
use qbz_models::Quality;
use qconnect_transport_ws::WsTransportConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::DaemonAdapter;
use crate::qconnect::engine::VolumeMode;

// What the Qobuz apps show next to this endpoint in their device list. Display
// strings only — the Connect identity is the persisted device_uuid — but they
// are the name a user picks the box out by, so they name THIS project. Each is
// overridable through its QBZ_QCONNECT_DEVICE_* env var.
const DEFAULT_QCONNECT_DEVICE_BRAND: &str = "Pibuz";
const DEFAULT_QCONNECT_DEVICE_MODEL: &str = "Pibuz";
// DeviceType as the web player's protobuf enum numbers it. It picks the icon
// the apps draw next to the name; the daemon is a network streamer, not the
// "computer" the desktop app it forked from announced.
const DEFAULT_QCONNECT_DEVICE_TYPE: i32 = DEVICE_TYPE_STREAMER;
const DEVICE_TYPE_STREAMER: i32 = 2;
/// Wire value -> the name the LAN pairing surface (mDNS TXT `type`,
/// `get-display-info`) uses for it. Index = wire value; the enum stops at 9.
const DEVICE_TYPE_NAMES: [&str; 10] = [
    "UNKNOWN",
    "SPEAKER",
    "STREAMER",
    "TV",
    "SOUNDBAR",
    "COMPUTER",
    "MOBILE",
    "CAST",
    "HEADPHONES",
    "TABLET",
];
const DEFAULT_QCONNECT_SOFTWARE_PREFIX: &str = "pibuz";
const QCONNECT_QWS_TOKEN_KIND: &str = "jwt_qws";
const QCONNECT_QWS_CREATE_TOKEN_PATH: &str = "/qws/createToken";

type Runtime = Arc<AppRuntime<DaemonAdapter>>;

/// The daemon-root `qconnect_settings.db` path, set once by `qconnect::start`
/// before any connect. Daemon adaptation: the desktop delegates the path to the
/// GLOBAL `qbz_app::qconnect_identity`; the daemon points every device-identity
/// and KV read at its OWN root (§4 — the daemon never touches the desktop KV).
static DAEMON_QCONNECT_SETTINGS_DB: OnceLock<PathBuf> = OnceLock::new();

/// Point the QConnect settings/device-identity DB at the daemon root. Idempotent
/// (a second call is ignored). Called from `qconnect::start(roots)` with
/// `<roots.data>/qconnect_settings.db`.
pub fn init_settings_db_path(path: PathBuf) {
    let _ = DAEMON_QCONNECT_SETTINGS_DB.set(path);
}

/// The daemon-root QConnect settings DB path, if `init_settings_db_path` ran.
fn qconnect_settings_db_path() -> Option<PathBuf> {
    DAEMON_QCONNECT_SETTINGS_DB.get().cloned()
}

/// The daemon's data root, set once by `qconnect::start` alongside the settings
/// DB. Separate from it because `daemon_prefs` lives in the root itself, not in
/// the KV — deriving it from the DB path's parent would couple the two layouts.
static DAEMON_DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Point the QConnect quality cap at the daemon's data root. Idempotent (a
/// second call is ignored). Called from `qconnect::start(roots)`.
pub fn init_data_root(path: PathBuf) {
    let _ = DAEMON_DATA_ROOT.set(path);
}

/// Persistent QConnect device identity for the DAEMON. Daemon adaptation vs. the
/// desktop (which delegates to the shared global identity module): the uuid is
/// minted + persisted in the daemon root's `qconnect_settings.db`, so the daemon
/// keeps its OWN identity, distinct from the desktop install. An explicit
/// `QBZ_QCONNECT_DEVICE_UUID` env value still wins. Fail-open: a fresh v4 when no
/// daemon settings path is available yet.
pub fn resolve_qconnect_device_uuid() -> String {
    if let Some(explicit) = std::env::var("QBZ_QCONNECT_DEVICE_UUID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return explicit;
    }
    match qconnect_settings_db_path() {
        Some(path) => qbz_app::qconnect_identity::device_uuid_from_db(&path),
        None => uuid::Uuid::new_v4().to_string(),
    }
}

/// `pibuz/<version>` software-version string for the device-info payload.
pub fn resolve_qconnect_software_version() -> String {
    std::env::var("QBZ_QCONNECT_SOFTWARE_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            // Build id when stamped, so the controller (and Qobuz-side logs)
            // name the exact build rather than a bare upstream version.
            format!("{DEFAULT_QCONNECT_SOFTWARE_PREFIX}/{}", crate::VERSION)
        })
}

pub fn resolve_qconnect_device_brand() -> String {
    std::env::var("QBZ_QCONNECT_DEVICE_BRAND")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_QCONNECT_DEVICE_BRAND.to_string())
}

pub fn resolve_qconnect_device_model() -> String {
    std::env::var("QBZ_QCONNECT_DEVICE_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_QCONNECT_DEVICE_MODEL.to_string())
}

/// `QBZ_QCONNECT_DEVICE_TYPE` overrides the default with a wire value; one the
/// enum does not define falls back to the default rather than announcing a
/// type no app knows how to draw.
pub fn resolve_qconnect_device_type() -> i32 {
    device_type_from_env(std::env::var("QBZ_QCONNECT_DEVICE_TYPE").ok().as_deref())
}

fn device_type_from_env(value: Option<&str>) -> i32 {
    value
        .and_then(|value| value.trim().parse::<i32>().ok())
        .filter(|kind| device_type_name(*kind).is_some())
        .unwrap_or(DEFAULT_QCONNECT_DEVICE_TYPE)
}

/// The pairing-surface name of a DeviceType wire value, `None` outside the enum.
pub fn device_type_name(kind: i32) -> Option<&'static str> {
    usize::try_from(kind)
        .ok()
        .and_then(|index| DEVICE_TYPE_NAMES.get(index))
        .copied()
}

pub fn resolve_system_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.trim().is_empty() {
            return h.trim().to_string();
        }
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = h.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    "Desktop".to_string()
}

/// Returns "Pibuz ({hostname})" as the default device name (OD6). Only a
/// default: a name set in the daemon settings wins, and so does
/// `QBZ_QCONNECT_DEVICE_NAME`. A box that never set one is renamed in the
/// Qobuz app by this, which is the point — it is the same endpoint, under the
/// name the project now has.
pub fn resolve_default_qconnect_device_name() -> String {
    format!("Pibuz ({})", resolve_system_hostname())
}

/// Effective friendly name: custom override -> env -> "Pibuz ({hostname})".
pub fn resolve_qconnect_friendly_name(custom_name: Option<&str>) -> String {
    custom_name
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("QBZ_QCONNECT_DEVICE_NAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(resolve_default_qconnect_device_name)
}

// ---------------------------------------------------------------------------
// Device-info + join payload shapes (piece a). Mirror the Tauri adapter
// `types.rs` `QconnectDeviceInfoPayload` / `QconnectDeviceCapabilitiesPayload` /
// `QconnectJoinSessionRequest` exactly so the wire frames the server sees are
// byte-identical between the frontends + the daemon. Built from the device-
// identity resolvers above; the uuid resolves against the daemon root so the
// daemon keeps ONE identity of its own.
// ---------------------------------------------------------------------------

// AudioQuality wire levels: 1=mp3, 2=cd, 3=hires_l1(96k), 4=hires_l2(192k) --
// the same four tiers `playback.quality` names. Capabilities advertise the
// device's min/max decode support; VolumeRemoteControl is NOT_ALLOWED(1) or
// ALLOWED(2), the web player's enum values.
pub const AUDIO_QUALITY_MP3: i32 = 1;
pub const AUDIO_QUALITY_CD: i32 = 2;
pub const AUDIO_QUALITY_HIRES_LEVEL1: i32 = 3;
pub const AUDIO_QUALITY_HIRES_LEVEL2: i32 = 4;
const VOLUME_REMOTE_CONTROL_NOT_ALLOWED: i32 = 1;
const VOLUME_REMOTE_CONTROL_ALLOWED: i32 = 2;

/// What to advertise for a volume mode. `Locked` ignores every SetVolume, so it
/// says so up front instead of offering the controller a slider that snaps
/// back; `External` applies nothing either, but it does take the level — an
/// integrator downstream acts on it — so a controller may set it.
fn volume_remote_control(mode: VolumeMode) -> i32 {
    match mode {
        VolumeMode::Locked => VOLUME_REMOTE_CONTROL_NOT_ALLOWED,
        VolumeMode::Software | VolumeMode::External => VOLUME_REMOTE_CONTROL_ALLOWED,
    }
}
/// Renderer buffer-state wire value for OK/ready (mirrors the Tauri adapter).
pub const BUFFER_STATE_OK: i32 = 2;
/// Renderer buffer-state wire value for "filling the buffer, not yet audible"
/// (`BufferState.BUFFER_STATE_BUFFERING`, field 2 of the protocol's
/// RendererState). Reported while a stream is loading so the controller shows a
/// loading state instead of advancing its progress bar against silence — the
/// progressive feeder can take seconds to reach a deep resume position, and
/// claiming OK the whole time is what made the app rubber-band.
pub const BUFFER_STATE_BUFFERING: i32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QconnectDeviceCapabilitiesPayload {
    pub min_audio_quality: Option<i32>,
    pub max_audio_quality: Option<i32>,
    pub volume_remote_control: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QconnectDeviceInfoPayload {
    pub device_uuid: Option<String>,
    pub friendly_name: Option<String>,
    pub brand: Option<String>,
    pub model: Option<String>,
    pub serial_number: Option<String>,
    pub device_type: Option<i32>,
    pub capabilities: Option<QconnectDeviceCapabilitiesPayload>,
    pub software_version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QconnectJoinSessionRequest {
    pub session_uuid: Option<String>,
    pub device_info: Option<QconnectDeviceInfoPayload>,
}

/// Load the persisted custom device name from the daemon-root settings DB.
/// Fail-open: None when `init_settings_db_path` never ran or on any read error.
fn load_persisted_device_name() -> Option<String> {
    qconnect_settings_db_path().and_then(|path| load_device_name_at(&path))
}

/// Read from the KV like the name, rather than threaded from `connect()`, so
/// every path that builds a device info agrees with the others. `connect()`
/// reads the same key once per connect, and a change applies on the next one,
/// so the capability and the mode the engine enforces move together.
fn load_persisted_volume_mode() -> VolumeMode {
    VolumeMode::from_kv(
        qconnect_settings_db_path()
            .and_then(|path| load_volume_mode_at(&path))
            .as_deref(),
    )
}

// ---------------------------------------------------------------------------
// The local quality cap on the cast path (upstream#693).
//
// `playback.quality` (daemon_prefs.streaming_quality) used to bind only the
// local playback path (`api/playback.rs::resolve_quality`). As a QConnect
// renderer the daemon took the controller's `max_audio_quality` as gospel AND
// advertised level 4 unconditionally, so a box configured for CD streamed
// 24/192 whenever a phone asked for it. Since pibuz is cast-only, that made the
// setting inert for every track that actually plays.
//
// Two halves, and both are needed: advertise the cap so a well-behaved
// controller never offers above it, and clamp at the load seam so one that does
// is held anyway (`qconnect_app::session::cap_quality`).
// ---------------------------------------------------------------------------

/// Where a `Quality` sits on the QConnect wire scale.
fn audio_quality_level(quality: Quality) -> i32 {
    match quality {
        Quality::Mp3 => AUDIO_QUALITY_MP3,
        Quality::Lossless => AUDIO_QUALITY_CD,
        Quality::HiRes => AUDIO_QUALITY_HIRES_LEVEL1,
        Quality::UltraHiRes => AUDIO_QUALITY_HIRES_LEVEL2,
    }
}

/// The configured ceiling, read fresh from `daemon_prefs` under `data_root`.
///
/// Goes through the SAME `quality_from_key` contract the local path uses, so
/// the two can never drift: one key table, one meaning of "cd".
pub fn local_max_quality_at(data_root: &Path) -> Quality {
    playback_driver::quality_from_key(&daemon_prefs::load_at(data_root).streaming_quality)
}

/// The configured ceiling, or `None` when `init_data_root` never ran (fail-open
/// = uncapped, matching every other resolver in this module).
pub fn local_max_quality() -> Option<Quality> {
    DAEMON_DATA_ROOT
        .get()
        .map(|root| local_max_quality_at(root))
}

/// The ceiling as a wire level, for the capability payload and the renderer
/// report. Uncapped advertises the top tier, which is what it always did.
pub fn local_max_audio_quality_level() -> i32 {
    audio_quality_level(local_max_quality().unwrap_or(Quality::UltraHiRes))
}

/// Build the device-info payload with the EFFECTIVE friendly name: the persisted
/// custom device name when one is set, else the default. The persisted name must
/// win here (not only at the controller-bootstrap call site that threads it
/// explicitly): with a custom name set, announcing the DEFAULT name from the
/// renderer join / local identity gives ONE device_uuid TWO friendly names, and
/// when the server's ADD_RENDERER omits the uuid the name-fingerprint self-match
/// fails -> `local_renderer_id = None` -> every renderer report is dropped by the
/// `is_local_renderer_active` gate (mute renderer, frozen controller seekbar).
pub fn default_qconnect_device_info() -> QconnectDeviceInfoPayload {
    default_qconnect_device_info_with_name(load_persisted_device_name().as_deref())
}

/// Build the device-info payload, overriding the friendly name when a custom
/// device name is set. Mirrors the Tauri `default_qconnect_device_info_with_name`.
pub fn default_qconnect_device_info_with_name(
    custom_name: Option<&str>,
) -> QconnectDeviceInfoPayload {
    QconnectDeviceInfoPayload {
        device_uuid: Some(resolve_qconnect_device_uuid()),
        friendly_name: Some(resolve_qconnect_friendly_name(custom_name)),
        brand: Some(resolve_qconnect_device_brand()),
        model: Some(resolve_qconnect_device_model()),
        serial_number: None,
        device_type: Some(resolve_qconnect_device_type()),
        capabilities: Some(QconnectDeviceCapabilitiesPayload {
            min_audio_quality: Some(AUDIO_QUALITY_MP3),
            max_audio_quality: Some(local_max_audio_quality_level()),
            volume_remote_control: Some(volume_remote_control(load_persisted_volume_mode())),
        }),
        software_version: Some(resolve_qconnect_software_version()),
    }
}

/// Resolve THIS device's identity for injection into the frontend-agnostic
/// session-apply logic in qconnect-app (`apply_session_management_event` takes a
/// `LocalIdentity` so the crate never depends on the persistence layer). Mirrors
/// the Tauri `resolve_local_identity`.
pub fn resolve_local_identity() -> qconnect_app::LocalIdentity {
    let info = default_qconnect_device_info();
    qconnect_app::LocalIdentity {
        device_uuid: info.device_uuid.unwrap_or_default(),
        friendly_name: info.friendly_name,
        brand: info.brand,
        model: info.model,
        device_type: info.device_type,
    }
}

// ---------------------------------------------------------------------------
// Daemon-root KV helpers (path-parameterized `*_at` copies of
// qconnect_transport.rs:281-411). The desktop variants read the GLOBAL settings
// DB; the daemon threads the DB path so T11 (`pibuz qconnect enable|disable`,
// settings show/set) and T13 (TUI QConnect screen) drive the SAME daemon-root DB
// the connect path reads. Every LOAD is fail-open (returns the default on any
// error); every SAVE returns the error, so `pibuz settings set qconnect.*` can
// exit 1 instead of reporting a write that never happened.
// ---------------------------------------------------------------------------

/// How long a connection waits on another one's lock before giving up. The
/// daemon and a `settings set` from moOde open this file concurrently, and
/// without a timeout SQLite fails the loser immediately with SQLITE_BUSY —
/// which used to drop the write silently. Every holder keeps the lock for
/// one statement, so this is never actually waited out.
const QCONNECT_SETTINGS_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Open the daemon-root QConnect settings DB (key/value table), creating it when
/// missing. Mirrors the Tauri `startup.rs::open_settings_conn`.
fn try_open_qconnect_settings_conn_at(path: &Path) -> Result<rusqlite::Connection, String> {
    let fail = |err: rusqlite::Error| format!("qconnect settings {}: {err}", path.display());
    // SQLite creates the file but not its directory. On a fresh profile the
    // data root does not exist until the daemon or another store makes it, and
    // a `settings set qconnect.*` that came first was dropped without a word.
    // The other stores create theirs.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("qconnect settings {}: {err}", dir.display()))?;
    }
    let conn = rusqlite::Connection::open(path).map_err(fail)?;
    // Before the PRAGMA: switching to WAL takes a lock too.
    conn.busy_timeout(QCONNECT_SETTINGS_BUSY_TIMEOUT)
        .map_err(fail)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .map_err(fail)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )
    .map_err(fail)?;
    Ok(conn)
}

/// The fail-open form the loaders use.
fn open_qconnect_settings_conn_at(path: &Path) -> Option<rusqlite::Connection> {
    try_open_qconnect_settings_conn_at(path).ok()
}

/// Store one `key = value` row.
fn put_qconnect_setting_at(path: &Path, key: &str, value: &str) -> Result<(), String> {
    try_open_qconnect_settings_conn_at(path)?
        .execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .map(drop)
        .map_err(|err| format!("qconnect settings {}: write {key}: {err}", path.display()))
}

/// Load the persisted custom device name (fail-open: None on any error).
pub fn load_device_name_at(path: &Path) -> Option<String> {
    let conn = open_qconnect_settings_conn_at(path)?;
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'device_name'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|v| !v.trim().is_empty())
}

/// Persist the custom device name (None clears it).
pub fn persist_device_name_at(path: &Path, name: Option<&str>) -> Result<(), String> {
    match name {
        Some(n) => put_qconnect_setting_at(path, "device_name", n),
        None => try_open_qconnect_settings_conn_at(path)?
            .execute("DELETE FROM settings WHERE key = 'device_name'", [])
            .map(drop)
            .map_err(|err| {
                format!(
                    "qconnect settings {}: clear device_name: {err}",
                    path.display()
                )
            }),
    }
}

/// Load the persisted QConnect startup mode (`startup_mode` = "off" | "on" |
/// "remember_last"). Fail-open: returns `Off` (the default) when missing/invalid.
pub fn load_startup_mode_at(path: &Path) -> qconnect_app::QconnectStartupMode {
    let Some(conn) = open_qconnect_settings_conn_at(path) else {
        return qconnect_app::QconnectStartupMode::default();
    };
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'startup_mode'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .as_deref()
    .and_then(qconnect_app::QconnectStartupMode::from_str)
    .unwrap_or_default()
}

/// Persist the QConnect startup mode.
pub fn save_startup_mode_at(
    path: &Path,
    mode: qconnect_app::QconnectStartupMode,
) -> Result<(), String> {
    put_qconnect_setting_at(path, "startup_mode", mode.as_str())
}

/// Load the daemon volume mode (`volume_mode` = "software" | "locked"; 01 §7.4).
/// NEW daemon key (the desktop never reads it). Fail-open: None when unset.
pub fn load_volume_mode_at(path: &Path) -> Option<String> {
    let conn = open_qconnect_settings_conn_at(path)?;
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'volume_mode'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|v| !v.trim().is_empty())
}

/// Persist the daemon volume mode ("software" | "locked").
pub fn save_volume_mode_at(path: &Path, mode: &str) -> Result<(), String> {
    put_qconnect_setting_at(path, "volume_mode", mode)
}

/// Load the pairing-surface switch (`pairing` = "on" | "off"; pairing.rs). NEW
/// daemon key (the desktop never reads it). Fail-open: ON when unset/invalid —
/// local pairing is the daemon's reason to exist on a shared-household box.
pub fn load_pairing_enabled_at(path: &Path) -> bool {
    let Some(conn) = open_qconnect_settings_conn_at(path) else {
        return true;
    };
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'pairing'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok();
    !matches!(value.as_deref(), Some("off"))
}

/// Persist the pairing-surface switch.
pub fn save_pairing_enabled_at(path: &Path, enabled: bool) -> Result<(), String> {
    put_qconnect_setting_at(path, "pairing", if enabled { "on" } else { "off" })
}

/// Default port for the pairing listener — one above the control plane's 8182.
pub const DEFAULT_PAIRING_PORT: u16 = 8183;

/// Load the pairing listener port (`pairing_port`). Fail-open to the default.
pub fn load_pairing_port_at(path: &Path) -> u16 {
    let Some(conn) = open_qconnect_settings_conn_at(path) else {
        return DEFAULT_PAIRING_PORT;
    };
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'pairing_port'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.trim().parse::<u16>().ok())
    .filter(|p| *p != 0)
    .unwrap_or(DEFAULT_PAIRING_PORT)
}

/// Persist the pairing listener port.
pub fn save_pairing_port_at(path: &Path, port: u16) -> Result<(), String> {
    put_qconnect_setting_at(path, "pairing_port", &port.to_string())
}

/// The volume to set on ourselves when a Connect session picks this device,
/// as a percentage; `None` (unset or 0) keeps whatever the player already has.
///
/// A controller that has never spoken to this renderer sends its own idea of the
/// volume, which for the iOS app is near 100 % — reported on the moOde forum as
/// "after connecting, the volume in [the] qobuz app is near 100%" with no safety
/// mechanism. Every comparable renderer has this knob (librespot's
/// `--initial-volume`, and moOde exposes it for Spotify and Deezer), and it
/// exists so a cast does not arrive at full scale on a system with no downstream
/// volume control.
pub fn load_initial_volume_at(path: &Path) -> Option<u8> {
    let conn = open_qconnect_settings_conn_at(path)?;
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'initial_volume'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.trim().parse::<u8>().ok())
    .filter(|pct| *pct > 0 && *pct <= 100)
}

/// Persist the join-time volume percentage; `0` disables it.
pub fn save_initial_volume_at(path: &Path, percent: u8) -> Result<(), String> {
    put_qconnect_setting_at(path, "initial_volume", &percent.to_string())
}

/// Build the WS transport config: env overrides first, then auto-discovery via
/// `qws/createToken`. Mirrors the Tauri `resolve_transport_config` (the per-option
/// knobs are deferred — the connect path uses defaults + env). `require_jwt =
/// true` (hard) and `reconnect_idle_retry_ms = 60s` match the hardened defaults.
pub async fn resolve_transport_config(runtime: &Runtime) -> Result<WsTransportConfig, String> {
    let mut endpoint_url = normalize_opt_string(std::env::var("QBZ_QCONNECT_WS_ENDPOINT").ok());
    let mut jwt_qws = normalize_opt_string(std::env::var("QBZ_QCONNECT_JWT_QWS").ok())
        .or_else(|| normalize_opt_string(std::env::var("QBZ_QCONNECT_JWT").ok()));

    if endpoint_url.is_none() || jwt_qws.is_none() {
        match fetch_qconnect_transport_credentials(runtime).await {
            Ok((discovered_endpoint, discovered_jwt_qws)) => {
                endpoint_url = endpoint_url.or(discovered_endpoint);
                jwt_qws = jwt_qws.or(discovered_jwt_qws);
            }
            Err(err) if endpoint_url.is_some() => {
                log::warn!(
                    "[QConnect] qws/createToken auto-discovery failed, using provided endpoint: {err}"
                );
            }
            Err(err) => {
                return Err(format!(
                    "QConnect endpoint_url is required (or QBZ_QCONNECT_WS_ENDPOINT). Auto-discovery via qws/createToken failed: {err}"
                ));
            }
        }
    }

    let endpoint_url = endpoint_url.ok_or_else(|| {
        "QConnect endpoint_url is required (or QBZ_QCONNECT_WS_ENDPOINT)".to_string()
    })?;

    let subscribe_channels = if let Ok(raw) = std::env::var("QBZ_QCONNECT_SUBSCRIBE_CHANNELS_HEX") {
        let channels: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
            .collect();
        parse_subscribe_channels(channels)?
    } else {
        // Default QConnect channels: connectionId(0x01), backend(0x02), controllers(0x03)
        vec![vec![0x01], vec![0x02], vec![0x03]]
    };

    let config = WsTransportConfig {
        endpoint_url,
        jwt_qws,
        require_jwt: true,
        reconnect_idle_retry_ms: 60_000,
        subscribe_channels,
        ..Default::default()
    };
    Ok(config)
}

async fn fetch_qconnect_transport_credentials(
    runtime: &Runtime,
) -> Result<(Option<String>, Option<String>), String> {
    // The client is Option-wrapped (None before API init).
    let client = runtime
        .core()
        .client()
        .read()
        .await
        .clone()
        .ok_or_else(|| "qws/createToken requires an initialized API client".to_string())?;

    let app_id = client
        .app_id()
        .await
        .map_err(|err| format!("qws/createToken requires initialized API client: {err}"))?;
    let user_auth_token = client
        .auth_token()
        .await
        .map_err(|err| format!("qws/createToken requires authenticated user: {err}"))?;

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "X-App-Id",
        reqwest::header::HeaderValue::from_str(&app_id)
            .map_err(|_| "invalid X-App-Id header value".to_string())?,
    );
    headers.insert(
        "X-User-Auth-Token",
        reqwest::header::HeaderValue::from_str(&user_auth_token)
            .map_err(|_| "invalid X-User-Auth-Token header value".to_string())?,
    );

    let url = qbz_qobuz::endpoints::build_url(QCONNECT_QWS_CREATE_TOKEN_PATH);
    let response = client
        .get_http()
        .post(&url)
        .headers(headers)
        .form(&[
            ("jwt", QCONNECT_QWS_TOKEN_KIND),
            ("user_auth_token_needed", "true"),
            ("strong_auth_needed", "true"),
        ])
        .send()
        .await
        .map_err(|err| format!("qws/createToken HTTP request failed: {err}"))?;

    let status = response.status();
    // Read the raw body and check status BEFORE decoding: a 403's body is an
    // edge/WAF HTML or empty page, not our JSON envelope, so a bare `.json()`
    // surfaced it as a misleading "response decode failed" instead of the real
    // status (issue #637).
    let body = response
        .text()
        .await
        .map_err(|err| format!("qws/createToken response read failed: {err}"))?;

    if !status.is_success() {
        let preview = body.trim().chars().take(300).collect::<String>();
        return Err(format!("qws/createToken status {status}: {preview}"));
    }

    let payload: Value = serde_json::from_str(&body)
        .map_err(|err| format!("qws/createToken response decode failed: {err}"))?;

    let jwt_qws_payload = payload
        .get("jwt_qws")
        .ok_or_else(|| "qws/createToken response missing jwt_qws payload".to_string())?;

    let endpoint_url = normalize_opt_string(
        jwt_qws_payload
            .get("endpoint")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    );
    let jwt_qws = normalize_opt_string(
        jwt_qws_payload
            .get("jwt")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    );

    if endpoint_url.is_none() {
        return Err("qws/createToken response missing jwt_qws.endpoint".to_string());
    }

    Ok((endpoint_url, jwt_qws))
}

fn normalize_opt_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub fn parse_subscribe_channels(items: Vec<String>) -> Result<Vec<Vec<u8>>, String> {
    items
        .into_iter()
        .map(|item| decode_hex_channel(&item))
        .collect()
}

pub fn decode_hex_channel(raw: &str) -> Result<Vec<u8>, String> {
    let normalized = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if normalized.is_empty() {
        return Err("empty subscribe channel hex value".to_string());
    }

    let needs_padding = !normalized.len().is_multiple_of(2);
    let value = if needs_padding {
        format!("0{normalized}")
    } else {
        normalized.to_string()
    };

    let mut bytes = Vec::with_capacity(value.len() / 2);
    let chars: Vec<char> = value.chars().collect();

    for idx in (0..chars.len()).step_by(2) {
        let pair = [chars[idx], chars[idx + 1]];
        let hex = pair.iter().collect::<String>();
        let byte = u8::from_str_radix(&hex, 16)
            .map_err(|_| format!("invalid subscribe channel hex byte '{hex}' in '{raw}'"))?;
        bytes.push(byte);
    }

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_type_defaults_to_streamer_and_rejects_values_outside_the_enum() {
        assert_eq!(device_type_from_env(None), DEVICE_TYPE_STREAMER);
        assert_eq!(device_type_from_env(Some(" 1 ")), 1);
        assert_eq!(device_type_from_env(Some("9")), 9);
        assert_eq!(device_type_from_env(Some("10")), DEVICE_TYPE_STREAMER);
        assert_eq!(device_type_from_env(Some("-1")), DEVICE_TYPE_STREAMER);
        assert_eq!(device_type_from_env(Some("tv")), DEVICE_TYPE_STREAMER);
        assert_eq!(device_type_name(2), Some("STREAMER"));
        assert_eq!(device_type_name(10), None);
    }

    #[test]
    fn only_locked_volume_refuses_remote_control() {
        assert_eq!(
            volume_remote_control(VolumeMode::Locked),
            VOLUME_REMOTE_CONTROL_NOT_ALLOWED
        );
        assert_eq!(
            volume_remote_control(VolumeMode::Software),
            VOLUME_REMOTE_CONTROL_ALLOWED
        );
        assert_eq!(
            volume_remote_control(VolumeMode::External),
            VOLUME_REMOTE_CONTROL_ALLOWED
        );
    }

    #[test]
    fn decode_hex_channel_pads_odd_length() {
        assert_eq!(decode_hex_channel("1").unwrap(), vec![0x01]);
        assert_eq!(decode_hex_channel("0x03").unwrap(), vec![0x03]);
        assert_eq!(decode_hex_channel("0102").unwrap(), vec![0x01, 0x02]);
        assert!(decode_hex_channel("").is_err());
        assert!(decode_hex_channel("zz").is_err());
    }

    #[test]
    fn default_device_name_is_qbz_hostname_form() {
        // OD6: the daemon default is "Pibuz (hostname)", never the desktop
        // "Qbz - hostname" form.
        std::env::set_var("HOSTNAME", "studio-pi");
        let name = resolve_default_qconnect_device_name();
        std::env::remove_var("HOSTNAME");
        assert_eq!(name, "Pibuz (studio-pi)");
    }

    #[test]
    fn device_uuid_env_override_takes_precedence() {
        // SAFETY: single-threaded test process for this var; restore after.
        std::env::set_var("QBZ_QCONNECT_DEVICE_UUID", "env-override-uuid-123");
        let uuid = resolve_qconnect_device_uuid();
        std::env::remove_var("QBZ_QCONNECT_DEVICE_UUID");
        assert_eq!(uuid, "env-override-uuid-123");
    }

    /// upstream#693 — what the daemon ADVERTISES has to be the configured
    /// cap, not a constant. A controller that respects the capability never
    /// offers above it, which is the half of the fix that keeps hi-res off the
    /// wire instead of merely off the DAC.
    #[test]
    fn the_advertised_ceiling_is_the_configured_playback_quality() {
        let root = std::env::temp_dir().join(format!(
            "pibuz_qconnect_cap_test_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch data root");

        let set = |key: &str| {
            let mut prefs = daemon_prefs::load_at(&root);
            prefs.streaming_quality = key.to_string();
            daemon_prefs::save_at(&prefs, &root).expect("save prefs");
        };

        // Every key `settings set playback.quality` accepts maps to the wire
        // level for the same tier — one table, shared with the local path.
        for (key, level, quality) in [
            ("mp3", AUDIO_QUALITY_MP3, Quality::Mp3),
            ("cd", AUDIO_QUALITY_CD, Quality::Lossless),
            ("hires", AUDIO_QUALITY_HIRES_LEVEL1, Quality::HiRes),
            (
                "hires_plus",
                AUDIO_QUALITY_HIRES_LEVEL2,
                Quality::UltraHiRes,
            ),
        ] {
            set(key);
            assert_eq!(local_max_quality_at(&root), quality, "{key}");
            assert_eq!(
                audio_quality_level(local_max_quality_at(&root)),
                level,
                "{key}"
            );
        }

        // An unreadable / unset root must not silently cap playback: fail open.
        assert_eq!(
            local_max_quality_at(&root.join("does-not-exist")),
            Quality::UltraHiRes
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kv_helpers_round_trip_against_a_temp_daemon_db() {
        let tmp =
            std::env::temp_dir().join(format!("pibuz_qconnect_kv_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        // Unset -> defaults / None.
        assert_eq!(
            load_startup_mode_at(&tmp),
            qconnect_app::QconnectStartupMode::Off
        );
        assert_eq!(load_device_name_at(&tmp), None);
        assert_eq!(load_volume_mode_at(&tmp), None);
        assert!(load_pairing_enabled_at(&tmp), "pairing defaults ON");
        assert_eq!(load_pairing_port_at(&tmp), DEFAULT_PAIRING_PORT);

        // Persist + read back.
        save_startup_mode_at(&tmp, qconnect_app::QconnectStartupMode::On).unwrap();
        persist_device_name_at(&tmp, Some("Living Room")).unwrap();
        save_volume_mode_at(&tmp, "locked").unwrap();

        assert_eq!(
            load_startup_mode_at(&tmp),
            qconnect_app::QconnectStartupMode::On
        );
        assert_eq!(load_device_name_at(&tmp).as_deref(), Some("Living Room"));
        assert_eq!(load_volume_mode_at(&tmp).as_deref(), Some("locked"));

        // Clearing the device name removes it.
        persist_device_name_at(&tmp, None).unwrap();
        assert_eq!(load_device_name_at(&tmp), None);

        // Pairing switch + port round-trip; port 0 / garbage fall back.
        save_pairing_enabled_at(&tmp, false).unwrap();
        assert!(!load_pairing_enabled_at(&tmp));
        save_pairing_enabled_at(&tmp, true).unwrap();
        assert!(load_pairing_enabled_at(&tmp));
        save_pairing_port_at(&tmp, 9000).unwrap();
        assert_eq!(load_pairing_port_at(&tmp), 9000);
        save_pairing_port_at(&tmp, 0).unwrap();
        assert_eq!(load_pairing_port_at(&tmp), DEFAULT_PAIRING_PORT);

        let _ = std::fs::remove_file(&tmp);
    }

    /// Every save used to swallow its error, so `pibuz settings set
    /// qconnect.volume_mode locked` exited 0 against a database it could not
    /// open. A plain file where the directory should be fails the open even
    /// as root, which a read-only file would not.
    #[test]
    fn every_save_reports_a_database_it_cannot_open() {
        let blocker =
            std::env::temp_dir().join(format!("pibuz_qconnect_kv_blocked_{}", std::process::id()));
        std::fs::write(&blocker, b"not a directory").unwrap();
        let db = blocker.join("qconnect_settings.db");

        let results = [
            (
                "startup_mode",
                save_startup_mode_at(&db, qconnect_app::QconnectStartupMode::On),
            ),
            (
                "device_name",
                persist_device_name_at(&db, Some("Living Room")),
            ),
            ("device_name cleared", persist_device_name_at(&db, None)),
            ("volume_mode", save_volume_mode_at(&db, "locked")),
            ("pairing", save_pairing_enabled_at(&db, false)),
            ("pairing_port", save_pairing_port_at(&db, 9000)),
            ("initial_volume", save_initial_volume_at(&db, 30)),
        ];
        for (key, result) in results {
            let err = result.expect_err(key);
            assert!(err.contains("qconnect settings"), "{key}: {err}");
        }
        let _ = std::fs::remove_file(&blocker);
    }

    /// A fresh profile has no data root until something creates it. A
    /// `qconnect.*` write that came first used to fail on the missing
    /// directory, silently.
    #[test]
    fn a_save_on_a_fresh_profile_creates_the_data_root() {
        let root =
            std::env::temp_dir().join(format!("pibuz_qconnect_kv_fresh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let db = root.join("data").join("qconnect_settings.db");

        save_volume_mode_at(&db, "locked").expect("creates the directory, then writes");
        assert_eq!(load_volume_mode_at(&db).as_deref(), Some("locked"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The daemon and a `settings set` from moOde open this file at the same
    /// time. Without a busy timeout the loser of a lock race failed at once,
    /// and the write was dropped without a word.
    #[test]
    fn a_save_waits_out_a_brief_lock_instead_of_dropping_the_write() {
        let db = std::env::temp_dir().join(format!(
            "pibuz_qconnect_kv_locked_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&db);
        save_volume_mode_at(&db, "software").unwrap();

        let holder = try_open_qconnect_settings_conn_at(&db).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            holder.execute_batch("COMMIT").unwrap();
        });

        save_volume_mode_at(&db, "locked").expect("waits for the lock, then writes");
        release.join().unwrap();
        assert_eq!(load_volume_mode_at(&db).as_deref(), Some("locked"));

        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
    }
}
