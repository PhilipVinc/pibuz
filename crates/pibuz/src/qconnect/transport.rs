//! QConnect transport config + credential discovery + device identity for the
//! pibuz daemon.
//!
//! The Qobuz client is reached via `runtime.core().client()` — an
//! `Arc<RwLock<Option<QobuzClient>>>` (may be `None` before the API is
//! initialized). The device uuid and Connect settings resolve against the
//! DAEMON root's `settings.toml` — its OWN device_uuid, minted fresh — and the
//! default device name is "Pibuz (hostname)". The load/persist helpers take
//! the data root (`*_at`), so `pibuz settings` drives the same file the
//! connect path reads.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use qbz_app::playback_driver;
use qbz_app::settings::daemon_prefs;
use qbz_app::settings::qconnect as qconnect_settings;
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
// the apps draw next to the name; the daemon is a network streamer, not a
// "computer". `qconnect.device_type` picks another.
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

/// The daemon's data root, set once by `qconnect::start` before any connect.
/// Every device-identity and settings read on the connect path goes to
/// `settings.toml` under it, and the quality cap reads `daemon_prefs` there.
static DAEMON_DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Point the connect path at the daemon's data root. Idempotent (a second call
/// is ignored). Called from `qconnect::start(roots)`.
pub(crate) fn init_data_root(path: PathBuf) {
    let _ = DAEMON_DATA_ROOT.set(path);
}

/// Persistent QConnect device identity: minted once and kept in the daemon
/// root's `settings.toml`. An explicit `QBZ_QCONNECT_DEVICE_UUID` env value
/// still wins. Fail-open: a fresh v4 when `init_data_root` has not run.
pub(crate) fn resolve_qconnect_device_uuid() -> String {
    if let Some(explicit) = std::env::var("QBZ_QCONNECT_DEVICE_UUID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return explicit;
    }
    match DAEMON_DATA_ROOT.get() {
        Some(root) => qconnect_settings::device_uuid(root),
        None => uuid::Uuid::new_v4().to_string(),
    }
}

/// `pibuz/<version>` software-version string for the device-info payload.
pub(crate) fn resolve_qconnect_software_version() -> String {
    std::env::var("QBZ_QCONNECT_SOFTWARE_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            // Build id when stamped, so the controller (and Qobuz-side logs)
            // name the exact build rather than a bare upstream version.
            format!("{DEFAULT_QCONNECT_SOFTWARE_PREFIX}/{}", crate::VERSION)
        })
}

pub(crate) fn resolve_qconnect_device_brand() -> String {
    std::env::var("QBZ_QCONNECT_DEVICE_BRAND")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_QCONNECT_DEVICE_BRAND.to_string())
}

pub(crate) fn resolve_qconnect_device_model() -> String {
    std::env::var("QBZ_QCONNECT_DEVICE_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_QCONNECT_DEVICE_MODEL.to_string())
}

/// The configured `qconnect.device_type`, read from settings.toml on every call
/// like the name, so the cloud announcement and the pairing surface agree. A
/// name the enum does not define (a hand-edited file) falls back to the default
/// rather than announcing a type no app knows how to draw.
pub(crate) fn resolve_qconnect_device_type() -> i32 {
    device_type_from_setting(
        DAEMON_DATA_ROOT
            .get()
            .and_then(|root| load_device_type_at(root))
            .as_deref(),
    )
}

fn device_type_from_setting(value: Option<&str>) -> i32 {
    value
        .and_then(device_type_wire)
        .unwrap_or(DEFAULT_QCONNECT_DEVICE_TYPE)
}

/// The wire value for a DeviceType name, case-insensitively. `None` for a name
/// the enum does not define, and for `unknown`, which no one should pick.
pub(crate) fn device_type_wire(name: &str) -> Option<i32> {
    let name = name.trim();
    DEVICE_TYPE_NAMES
        .iter()
        .position(|known| known.eq_ignore_ascii_case(name))
        .filter(|&index| index != 0)
        .and_then(|index| i32::try_from(index).ok())
}

/// The names `qconnect.device_type` accepts, lowercase, in wire order.
pub(crate) fn device_type_choices() -> impl Iterator<Item = String> {
    DEVICE_TYPE_NAMES[1..]
        .iter()
        .map(|name| name.to_ascii_lowercase())
}

/// The pairing-surface name of a DeviceType wire value, `None` outside the enum.
pub(crate) fn device_type_name(kind: i32) -> Option<&'static str> {
    usize::try_from(kind)
        .ok()
        .and_then(|index| DEVICE_TYPE_NAMES.get(index))
        .copied()
}

pub(crate) fn resolve_system_hostname() -> String {
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

/// Returns "Pibuz ({hostname})" as the default device name. Only a
/// default: a name set in the daemon settings wins, and so does
/// `QBZ_QCONNECT_DEVICE_NAME`. A box that never set one is renamed in the
/// Qobuz app by this, which is the point — it is the same endpoint, under the
/// name the project now has.
pub(crate) fn resolve_default_qconnect_device_name() -> String {
    format!("Pibuz ({})", resolve_system_hostname())
}

/// Effective friendly name: custom override -> env -> "Pibuz ({hostname})".
pub(crate) fn resolve_qconnect_friendly_name(custom_name: Option<&str>) -> String {
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
// Device-info + join payload shapes, exactly as the Connect server expects
// them on the wire. Built from the device-identity resolvers above; the uuid
// resolves against the daemon root so the daemon keeps ONE identity of its
// own.
// ---------------------------------------------------------------------------

// AudioQuality wire levels: 1=mp3, 2=cd, 3=hires_l1(96k), 4=hires_l2(192k) --
// the same four tiers `playback.quality` names. Capabilities advertise the
// device's min/max decode support; VolumeRemoteControl is NOT_ALLOWED(1) or
// ALLOWED(2), the web player's enum values.
pub(crate) const AUDIO_QUALITY_MP3: i32 = 1;
pub(crate) const AUDIO_QUALITY_CD: i32 = 2;
pub(crate) const AUDIO_QUALITY_HIRES_LEVEL1: i32 = 3;
pub(crate) const AUDIO_QUALITY_HIRES_LEVEL2: i32 = 4;
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
/// Renderer buffer-state wire value for OK/ready.
pub(crate) const BUFFER_STATE_OK: i32 = 2;
/// Renderer buffer-state wire value for "filling the buffer, not yet audible"
/// (`BufferState.BUFFER_STATE_BUFFERING`, field 2 of the protocol's
/// RendererState). Reported while a stream is loading so the controller shows a
/// loading state instead of advancing its progress bar against silence — the
/// progressive feeder can take seconds to reach a deep resume position, and
/// claiming OK the whole time is what made the app rubber-band.
pub(crate) const BUFFER_STATE_BUFFERING: i32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct QconnectDeviceCapabilitiesPayload {
    pub(crate) min_audio_quality: Option<i32>,
    pub max_audio_quality: Option<i32>,
    pub(crate) volume_remote_control: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct QconnectDeviceInfoPayload {
    pub device_uuid: Option<String>,
    pub friendly_name: Option<String>,
    pub brand: Option<String>,
    pub model: Option<String>,
    pub(crate) serial_number: Option<String>,
    pub device_type: Option<i32>,
    pub(crate) capabilities: Option<QconnectDeviceCapabilitiesPayload>,
    pub(crate) software_version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct QconnectJoinSessionRequest {
    pub(crate) session_uuid: Option<String>,
    pub(crate) device_info: Option<QconnectDeviceInfoPayload>,
}

/// Load the persisted custom device name. Fail-open: None when
/// `init_data_root` never ran or on any read error.
fn load_persisted_device_name() -> Option<String> {
    DAEMON_DATA_ROOT
        .get()
        .and_then(|root| load_device_name_at(root))
}

/// Read from settings.toml like the name, rather than threaded from
/// `connect()`, so every path that builds a device info agrees with the
/// others. `connect()` reads the same key once per connect, and a change
/// applies on the next one, so the capability and the mode the engine enforces
/// move together.
fn load_persisted_volume_mode() -> VolumeMode {
    VolumeMode::from_kv(
        DAEMON_DATA_ROOT
            .get()
            .and_then(|root| load_volume_mode_at(root))
            .as_deref(),
    )
}

// ---------------------------------------------------------------------------
// The local quality cap on the cast path.
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
pub(crate) fn local_max_quality_at(data_root: &Path) -> Quality {
    playback_driver::quality_from_key(&daemon_prefs::load_at(data_root).streaming_quality)
}

/// The configured ceiling, or `None` when `init_data_root` never ran (fail-open
/// = uncapped, matching every other resolver in this module).
pub(crate) fn local_max_quality() -> Option<Quality> {
    DAEMON_DATA_ROOT
        .get()
        .map(|root| local_max_quality_at(root))
}

/// The ceiling as a wire level, for the capability payload and the renderer
/// report. Uncapped advertises the top tier, which is what it always did.
pub(crate) fn local_max_audio_quality_level() -> i32 {
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
pub(crate) fn default_qconnect_device_info() -> QconnectDeviceInfoPayload {
    default_qconnect_device_info_with_name(load_persisted_device_name().as_deref())
}

/// Build the device-info payload, overriding the friendly name when a custom
/// device name is set.
pub(crate) fn default_qconnect_device_info_with_name(
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
/// `LocalIdentity` so the crate never depends on the persistence layer).
pub(crate) fn resolve_local_identity() -> qconnect_app::LocalIdentity {
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
// The `[qconnect]` section of settings.toml, read and written field by field
// (`qbz_app::settings::qconnect` owns the struct and the file). The CLI
// (`pibuz qconnect enable|disable`, `settings show/set`) and the connect path
// both go through these, against the daemon root.
// Every LOAD is fail-open (returns the default on any error); every SAVE
// returns the error, so `pibuz settings set qconnect.*` can exit 1 instead of
// reporting a write that never happened.
// ---------------------------------------------------------------------------

pub(crate) use qconnect_settings::DEFAULT_PAIRING_PORT;

/// Load the persisted custom device name. None when unset or blank.
pub(crate) fn load_device_name_at(root: &Path) -> Option<String> {
    qconnect_settings::load(root)
        .device_name
        .filter(|v| !v.trim().is_empty())
}

/// Persist the custom device name (None clears it).
pub(crate) fn persist_device_name_at(root: &Path, name: Option<&str>) -> Result<(), String> {
    qconnect_settings::update(root, |s| s.device_name = name.map(str::to_string))
}

/// Load the QConnect startup mode (`startup_mode` = "off" | "on"). `Off` (the
/// default) when unset or unknown.
pub(crate) fn load_startup_mode_at(root: &Path) -> qconnect_app::QconnectStartupMode {
    qconnect_app::QconnectStartupMode::from_str(&qconnect_settings::load(root).startup_mode)
        .unwrap_or_default()
}

/// Persist the QConnect startup mode.
pub(crate) fn save_startup_mode_at(
    root: &Path,
    mode: qconnect_app::QconnectStartupMode,
) -> Result<(), String> {
    qconnect_settings::update(root, |s| s.startup_mode = mode.as_str().to_string())
}

/// Load the daemon volume mode (`volume_mode` = "software" | "locked" |
/// "external"). None when unset.
pub(crate) fn load_volume_mode_at(root: &Path) -> Option<String> {
    qconnect_settings::load(root)
        .volume_mode
        .filter(|v| !v.trim().is_empty())
}

/// Persist the daemon volume mode.
pub(crate) fn save_volume_mode_at(root: &Path, mode: &str) -> Result<(), String> {
    qconnect_settings::update(root, |s| s.volume_mode = Some(mode.to_string()))
}

/// Load the configured device type name (`streamer`, `speaker`, ...). None
/// when unset.
pub(crate) fn load_device_type_at(root: &Path) -> Option<String> {
    qconnect_settings::load(root)
        .device_type
        .filter(|v| !v.trim().is_empty())
}

/// Persist the device type name.
pub(crate) fn save_device_type_at(root: &Path, name: &str) -> Result<(), String> {
    qconnect_settings::update(root, |s| s.device_type = Some(name.to_string()))
}

/// Load the pairing-surface switch (pairing.rs). ON when unset — local pairing
/// is the daemon's reason to exist on a shared-household box.
pub(crate) fn load_pairing_enabled_at(root: &Path) -> bool {
    qconnect_settings::load(root).pairing
}

/// Persist the pairing-surface switch.
pub(crate) fn save_pairing_enabled_at(root: &Path, enabled: bool) -> Result<(), String> {
    qconnect_settings::update(root, |s| s.pairing = enabled)
}

/// Load the pairing listener port. The default when unset or 0.
pub(crate) fn load_pairing_port_at(root: &Path) -> u16 {
    match qconnect_settings::load(root).pairing_port {
        0 => DEFAULT_PAIRING_PORT,
        port => port,
    }
}

/// Persist the pairing listener port.
pub(crate) fn save_pairing_port_at(root: &Path, port: u16) -> Result<(), String> {
    qconnect_settings::update(root, |s| s.pairing_port = port)
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
pub(crate) fn load_initial_volume_at(root: &Path) -> Option<u8> {
    Some(qconnect_settings::load(root).initial_volume).filter(|pct| *pct > 0 && *pct <= 100)
}

/// Persist the join-time volume percentage; `0` disables it.
pub(crate) fn save_initial_volume_at(root: &Path, percent: u8) -> Result<(), String> {
    qconnect_settings::update(root, |s| s.initial_volume = percent)
}

/// Build the WS transport config: env overrides first, then auto-discovery via
/// `qws/createToken`. The connect path uses defaults + env; there are no
/// per-option knobs. `require_jwt = true` (hard) and
/// `reconnect_idle_retry_ms = 60s` match the hardened defaults.
pub(crate) async fn resolve_transport_config(
    runtime: &Runtime,
) -> Result<WsTransportConfig, String> {
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

pub(crate) fn parse_subscribe_channels(items: Vec<String>) -> Result<Vec<Vec<u8>>, String> {
    items
        .into_iter()
        .map(|item| decode_hex_channel(&item))
        .collect()
}

pub(crate) fn decode_hex_channel(raw: &str) -> Result<Vec<u8>, String> {
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
    fn device_type_defaults_to_streamer_and_rejects_names_outside_the_enum() {
        assert_eq!(device_type_from_setting(None), DEVICE_TYPE_STREAMER);
        assert_eq!(device_type_from_setting(Some(" speaker ")), 1);
        assert_eq!(device_type_from_setting(Some("TV")), 3);
        assert_eq!(device_type_from_setting(Some("tablet")), 9);
        assert_eq!(
            device_type_from_setting(Some("unknown")),
            DEVICE_TYPE_STREAMER
        );
        assert_eq!(device_type_from_setting(Some("2")), DEVICE_TYPE_STREAMER);
        assert_eq!(
            device_type_from_setting(Some("laptop")),
            DEVICE_TYPE_STREAMER
        );
        assert_eq!(device_type_name(2), Some("STREAMER"));
        assert_eq!(device_type_name(10), None);
    }

    /// Every name `settings set` offers maps back to the wire value whose
    /// pairing-surface name it is, so the cloud and the LAN name one type.
    #[test]
    fn every_device_type_choice_round_trips() {
        let choices: Vec<String> = device_type_choices().collect();
        assert_eq!(choices.len(), 9);
        assert_eq!(choices[1], "streamer");
        for choice in &choices {
            let wire = device_type_wire(choice).expect(choice);
            assert_eq!(
                device_type_name(wire).unwrap().to_ascii_lowercase(),
                *choice
            );
        }
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
        // The daemon default is "Pibuz (hostname)", never the inherited
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

    /// What the daemon ADVERTISES has to be the configured
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
            daemon_prefs::update_at(&root, |p| p.streaming_quality = key.to_string())
                .expect("save prefs");
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

    fn scratch_root(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pibuz_qconnect_kv_{name}_{}_{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn kv_helpers_round_trip_against_a_temp_daemon_root() {
        let root = scratch_root("roundtrip");

        // Unset -> defaults / None.
        assert_eq!(
            load_startup_mode_at(&root),
            qconnect_app::QconnectStartupMode::Off
        );
        assert_eq!(load_device_name_at(&root), None);
        assert_eq!(load_volume_mode_at(&root), None);
        assert_eq!(load_device_type_at(&root), None);
        assert!(load_pairing_enabled_at(&root), "pairing defaults ON");
        assert_eq!(load_pairing_port_at(&root), DEFAULT_PAIRING_PORT);
        assert_eq!(load_initial_volume_at(&root), None);

        // Persist + read back.
        save_startup_mode_at(&root, qconnect_app::QconnectStartupMode::On).unwrap();
        persist_device_name_at(&root, Some("Living Room")).unwrap();
        save_volume_mode_at(&root, "locked").unwrap();
        save_device_type_at(&root, "speaker").unwrap();
        save_initial_volume_at(&root, 30).unwrap();

        assert_eq!(
            load_startup_mode_at(&root),
            qconnect_app::QconnectStartupMode::On
        );
        assert_eq!(load_device_name_at(&root).as_deref(), Some("Living Room"));
        assert_eq!(load_device_type_at(&root).as_deref(), Some("speaker"));
        assert_eq!(load_volume_mode_at(&root).as_deref(), Some("locked"));
        assert_eq!(load_initial_volume_at(&root), Some(30));

        // Clearing the device name removes it; 0 turns the join volume off.
        persist_device_name_at(&root, None).unwrap();
        assert_eq!(load_device_name_at(&root), None);
        save_initial_volume_at(&root, 0).unwrap();
        assert_eq!(load_initial_volume_at(&root), None);

        // Pairing switch + port round-trip; port 0 falls back.
        save_pairing_enabled_at(&root, false).unwrap();
        assert!(!load_pairing_enabled_at(&root));
        save_pairing_enabled_at(&root, true).unwrap();
        assert!(load_pairing_enabled_at(&root));
        save_pairing_port_at(&root, 9000).unwrap();
        assert_eq!(load_pairing_port_at(&root), 9000);
        save_pairing_port_at(&root, 0).unwrap();
        assert_eq!(load_pairing_port_at(&root), DEFAULT_PAIRING_PORT);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every save used to swallow its error, so `pibuz settings set
    /// qconnect.volume_mode locked` exited 0 against a store it could not
    /// open. A plain file where the directory should be fails the write even
    /// as root, which a read-only file would not.
    #[test]
    fn every_save_reports_a_root_it_cannot_write() {
        let blocker = scratch_root("blocked");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let root = blocker.join("data");

        let results = [
            (
                "startup_mode",
                save_startup_mode_at(&root, qconnect_app::QconnectStartupMode::On),
            ),
            (
                "device_name",
                persist_device_name_at(&root, Some("Living Room")),
            ),
            ("device_name cleared", persist_device_name_at(&root, None)),
            ("volume_mode", save_volume_mode_at(&root, "locked")),
            ("pairing", save_pairing_enabled_at(&root, false)),
            ("pairing_port", save_pairing_port_at(&root, 9000)),
            ("initial_volume", save_initial_volume_at(&root, 30)),
        ];
        for (key, result) in results {
            let err = result.expect_err(key);
            assert!(err.contains(&root.display().to_string()), "{key}: {err}");
        }
        let _ = std::fs::remove_file(&blocker);
    }

    /// A fresh profile has no data root until something creates it. A
    /// `qconnect.*` write that came first used to fail on the missing
    /// directory, silently.
    #[test]
    fn a_save_on_a_fresh_profile_creates_the_data_root() {
        let base = scratch_root("fresh");
        let root = base.join("data");

        save_volume_mode_at(&root, "locked").expect("creates the directory, then writes");
        assert_eq!(load_volume_mode_at(&root).as_deref(), Some("locked"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The daemon and a `settings set` from moOde write this file at the same
    /// time. Each save rewrites the whole `[qconnect]` section, so without the
    /// file lock the slower one would put back the value the faster one had
    /// just changed.
    #[test]
    fn concurrent_saves_of_different_keys_both_land() {
        let root = scratch_root("concurrent");
        for _ in 0..5 {
            let a = {
                let root = root.clone();
                std::thread::spawn(move || save_volume_mode_at(&root, "locked").unwrap())
            };
            let b = {
                let root = root.clone();
                std::thread::spawn(move || persist_device_name_at(&root, Some("Kitchen")).unwrap())
            };
            a.join().unwrap();
            b.join().unwrap();
            assert_eq!(load_volume_mode_at(&root).as_deref(), Some("locked"));
            assert_eq!(load_device_name_at(&root).as_deref(), Some("Kitchen"));
            save_volume_mode_at(&root, "software").unwrap();
            persist_device_name_at(&root, None).unwrap();
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
