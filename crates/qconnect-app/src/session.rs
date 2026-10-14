//! Session/liveness primitives for Qobuz Connect.
//!
//! Pure functions and pure data enums: the session-arbitration and liveness
//! logic, plus the session-topology types the sync accumulator holds.

use qbz_models::Quality;
use qconnect_core::{QConnectQueueState, QConnectRendererState, QueueItem};
use serde::{Deserialize, Serialize};

use crate::queue_resolution::{
    find_cursor_index_by_queue_item_id, normalize_current_queue_item_id_from_queue_state,
    ordered_queue_cursors, QconnectOrderedQueueCursor,
};

/// Lifecycle state surfaced to the host so it can report what the user asked
/// for (`running`) separately from what the transport currently has
/// (`Connecting`/`Connected`/`Reconnecting`). Without this distinction a
/// reconnect loop that is still alive reads as "off", leaving the user unable
/// to disable QConnect (issue #358).
///
/// Pure data; the session layer emits it via `QconnectAppEvent::LifecycleChanged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QconnectLifecycleState {
    /// User has not enabled QConnect (or it has been fully torn down).
    #[default]
    Off,
    /// `connect()` has been called; transport is establishing the WS but no
    /// `SessionEstablished` yet.
    Connecting,
    /// Transport saw at least one `SESSION_STATE` frame on the active WS — the
    /// session-level handshake completed.
    Connected,
    /// Transport disconnected after at least one successful connect; the
    /// reconnect loop is running.
    Reconnecting,
    /// `MaxReconnectAttemptsExceeded` fired — runtime auto-stopped, last error
    /// surfaced.
    Exhausted,
}

/// Playing-state wire value for PLAYING (the renderer reports
/// playing_state == 2 while actively playing). `pub(crate)` so the
/// session-apply/watchdog logic in `app.rs` reads the same constant.
pub(crate) const PLAYING_STATE_PLAYING: i32 = 2;

/// Playing-state wire value for UNKNOWN (== 0). The freeze path stamps a dead
/// renderer's cached `playing_state` to UNKNOWN so the projection stops lying.
pub(crate) const PLAYING_STATE_UNKNOWN: i32 = 0;

/// JoinSession `reason` wire values (proto tag 3): a first join from a fresh
/// runtime is a controller request, a join after a transport drop carries the
/// reconnection reason so the server treats it as session continuity rather
/// than a brand-new controller.
pub const JOIN_SESSION_REASON_CONTROLLER_REQUEST: i32 = 1;
pub const JOIN_SESSION_REASON_RECONNECTION: i32 = 2;

/// Official "renderer LOST" silence budget. A *playing* active peer renderer
/// that sends no RENDERER_STATE_UPDATED for this long is considered
/// unreachable (webplayer arms setTimeout(...,12e3) on onPlayerStateUpdated
/// while playingState==PLAY).
pub const QCONNECT_RENDERER_LOST_TIMEOUT_MS: u64 = 12_000;

/// Pure arming predicate for the renderer-liveness watchdog: arm only while the
/// active renderer is a peer AND its reported playing_state is PLAYING.
pub fn should_arm_renderer_watchdog(playing_state: Option<i32>, is_active_peer: bool) -> bool {
    is_active_peer && playing_state == Some(PLAYING_STATE_PLAYING)
}

/// Whether to send `CtrlSrvrSetActiveRenderer` for ourselves on a SESSION_STATE:
/// only to take back the slot we held before a connection drop, when the cloud
/// now says nobody holds it. The official client does the same.
///
/// That is the WHOLE takeover policy, on purpose. The web player's
/// `computeConnectionState` also claims an idle session on start, and takes over
/// from a paused peer while it is itself playing locally — both for a device
/// that plays on its own. This one never does: a phone casting to it, or handing
/// it the session over the LAN, decides when it is the output. (An idle-session
/// auto-claim once made every daemon start, and so every moOde renderer toggle,
/// grab an idle session uninvited.) Nor does it ever
/// push its own queue on connect: the cloud's queue is the truth, and the local
/// one is only a copy of an earlier session's.
pub fn should_reclaim_render(was_active: bool, server_idle: bool) -> bool {
    was_active && server_idle
}

/// Pure selector for the JoinSession `reason`: a post-drop rejoin carries
/// RECONNECTION, the first join from a fresh runtime carries CONTROLLER_REQUEST.
pub fn deferred_join_reason(has_disconnected: bool) -> i32 {
    if has_disconnected {
        JOIN_SESSION_REASON_RECONNECTION
    } else {
        JOIN_SESSION_REASON_CONTROLLER_REQUEST
    }
}

/// Pure predicate: keep re-asking for queue state after a Lagged
/// broadcast drop until the session_uuid is confirmed or the attempt budget is
/// spent. Stops immediately once the session_uuid is known.
pub fn should_reask_queue_state(
    session_uuid_known: bool,
    attempts: u32,
    max_attempts: u32,
) -> bool {
    !session_uuid_known && attempts < max_attempts
}

/// Wire renderer status from `MESSAGE_TYPE_SRVR_CTRL_RENDERER_STATE_UPDATED`
/// (`status` field, decoded in qconnect-protocol's `decoder.rs`).
/// Wire enum: UNKNOWN=0, ACTIVE_CONNECTED=1, ACTIVE_DISCONNECTED=2, INACTIVE=3.
/// Per the official client's `Ya` collapse, UNKNOWN and any UNRECOGNIZED value
/// map to INACTIVE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererStatus {
    ActiveConnected,
    ActiveDisconnected,
    Inactive,
}

impl RendererStatus {
    pub fn from_wire(value: Option<i64>) -> Self {
        match value {
            Some(1) => Self::ActiveConnected,
            Some(2) => Self::ActiveDisconnected,
            // 0 (UNKNOWN), 3 (INACTIVE), any UNRECOGNIZED value, and a missing
            // field all collapse to INACTIVE.
            _ => Self::Inactive,
        }
    }
}

/// Map a QConnect `max_audio_quality` level to a qbz `Quality`.
/// QConnect levels: 0/1 ~ MP3, 2 ~ CD/Lossless, 3 ~ Hi-Res (<=96kHz),
/// 4 ~ Hi-Res (>96kHz), 5/None ~ uncapped. The qbz `Quality` enum only has
/// four variants (Mp3, Lossless, HiRes, UltraHiRes), so 4 and uncapped both
/// resolve to UltraHiRes.
pub fn quality_from_max_audio_quality(level: Option<i32>) -> Quality {
    match level {
        Some(l) if l <= 1 => Quality::Mp3,
        Some(2) => Quality::Lossless,
        Some(3) => Quality::HiRes,
        Some(4) => Quality::UltraHiRes,
        _ => Quality::UltraHiRes,
    }
}

/// Where a `Quality` sits on the QConnect `max_audio_quality` scale, so two
/// tiers can be compared. Spelled out rather than leaning on the enum's
/// discriminants (5/6/7/27), which happen to sort correctly today but are Qobuz
/// format ids, not a rank.
pub fn quality_tier(quality: Quality) -> u8 {
    match quality {
        Quality::Mp3 => 0,
        Quality::Lossless => 1,
        Quality::HiRes => 2,
        Quality::UltraHiRes => 3,
    }
}

/// Hold `requested` to this renderer's OWN ceiling.
///
/// The controller's `max_audio_quality` is a REQUEST, not an instruction: a
/// phone asking for Ultra Hi-Res must not override a renderer configured to cap
/// at CD (upstream#693 — the local cap was applied only on the local playback
/// path, so casting bypassed it entirely). `cap` of `None` means uncapped, and a
/// cap ABOVE the request never upgrades it — the lower of the two always wins.
pub fn cap_quality(requested: Quality, cap: Option<Quality>) -> Quality {
    match cap {
        Some(cap) if quality_tier(cap) < quality_tier(requested) => cap,
        _ => requested,
    }
}

// ---------------------------------------------------------------------------
// Session topology types.
//
// These are pure data describing who is in a QConnect session and the cloud's
// per-renderer view, held in the sync accumulator under a single lock. Fields
// are `pub` because the host reads them across the crate boundary.
// ---------------------------------------------------------------------------

/// Per-renderer cached state derived from session-management events
/// (RENDERER_STATE_UPDATED etc.): the cloud's latest view of one renderer.
#[derive(Debug, Clone, Default)]
pub struct QconnectSessionRendererState {
    pub active: Option<bool>,
    pub playing_state: Option<i32>,
    pub current_position_ms: Option<u64>,
    pub current_queue_item_id: Option<u64>,
    pub volume: Option<i32>,
    pub muted: Option<bool>,
    pub max_audio_quality: Option<i32>,
    pub loop_mode: Option<i32>,
    pub shuffle_mode: Option<bool>,
    pub updated_at_ms: u64,
}

/// Session topology — stored from session management events (types 81-87):
/// who is in the session, who is active, and which renderer is us.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QconnectSessionState {
    pub session_uuid: Option<String>,
    pub active_renderer_id: Option<i32>,
    pub local_renderer_id: Option<i32>,
    pub renderers: Vec<QconnectRendererInfo>,
}

/// One renderer registered in the QConnect session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QconnectRendererInfo {
    pub renderer_id: i32,
    pub device_uuid: Option<String>,
    pub friendly_name: Option<String>,
    pub brand: Option<String>,
    pub model: Option<String>,
    pub device_type: Option<i32>,
    /// capabilities.volume_remote_control: 1 == NOT_ALLOWED, 2 == ALLOWED (the
    /// web player's enum). None == not advertised.
    #[serde(default)]
    pub volume_remote_control: Option<i32>,
}

/// Resolved local-device identity injected by the host into the session layer.
/// `refresh_local_renderer_id` matches on this data, so qconnect-app never
/// depends on the persistence/transport crates.
#[derive(Debug, Clone, Default)]
pub struct LocalIdentity {
    pub device_uuid: String,
    pub friendly_name: Option<String>,
    pub brand: Option<String>,
    pub model: Option<String>,
    pub device_type: Option<i32>,
}

/// Snapshot of the last reported file (stream) audio quality, used to dedup
/// outbound RndrSrvrFileAudioQualityChanged reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QconnectFileAudioQualitySnapshot {
    pub sampling_rate: i32,
    pub bit_depth: i32,
    pub nb_channels: i32,
    pub audio_quality: i32,
}

/// Resolve the local renderer id within the session by matching the injected
/// local identity: first by exact device_uuid, then by a unique device
/// fingerprint (name+brand+model+type), then a looser name+type fingerprint.
pub fn refresh_local_renderer_id(session: &mut QconnectSessionState, identity: &LocalIdentity) {
    if let Some(renderer_id) = session
        .renderers
        .iter()
        .find(|renderer| renderer.device_uuid.as_deref() == Some(identity.device_uuid.as_str()))
        .map(|renderer| renderer.renderer_id)
    {
        session.local_renderer_id = Some(renderer_id);
        return;
    }

    let local_friendly_name = identity.friendly_name.as_deref();
    let local_brand = identity.brand.as_deref();
    let local_model = identity.model.as_deref();
    let local_device_type = identity.device_type;

    // Some server ADD_RENDERER payloads omit device_uuid for the local renderer.
    // Fall back to a unique device fingerprint so the handoff logic can still
    // distinguish local vs peer renderers.
    if let Some(renderer_id) = find_unique_renderer_id(session, |renderer| {
        renderer.friendly_name.as_deref() == local_friendly_name
            && renderer.brand.as_deref() == local_brand
            && renderer.model.as_deref() == local_model
            && renderer.device_type == local_device_type
    }) {
        session.local_renderer_id = Some(renderer_id);
        return;
    }

    if let Some(renderer_id) = find_unique_renderer_id(session, |renderer| {
        renderer.friendly_name.as_deref() == local_friendly_name
            && renderer.device_type == local_device_type
    }) {
        session.local_renderer_id = Some(renderer_id);
        return;
    }

    session.local_renderer_id = None;
}

pub fn normalize_active_renderer_id(value: Option<i64>) -> Option<i32> {
    value
        .filter(|renderer_id| *renderer_id >= 0)
        .and_then(|renderer_id| i32::try_from(renderer_id).ok())
}

pub fn is_peer_renderer_active(session: &QconnectSessionState) -> bool {
    match (session.active_renderer_id, session.local_renderer_id) {
        (Some(active_renderer_id), Some(local_renderer_id)) => {
            active_renderer_id != local_renderer_id
        }
        _ => false,
    }
}

/// Whether the SESSION's active renderer is this device, as far as the renderer
/// ids can tell — `None` when they cannot tell.
///
/// The ids answer the question outright in two of the three shapes:
///
/// | active id | local id | answer |
/// |---|---|---|
/// | a peer / us | known | `Some(a == l)` |
/// | none (the cloud's `-1`) | anything | `Some(false)` — nobody renders |
/// | someone | **not resolved yet** | `None` — undetermined |
///
/// That last row is the one worth separating. `local_renderer_id` is `None`
/// until our own renderer shows up in the session's renderer list, so during
/// that window "are we the active renderer?" has no answer from the ids — and
/// collapsing it to `false`, as [`is_local_renderer_active`] must for its
/// report gates, means a transient "id pending" reads exactly like "a peer took
/// the render". Callers that publish the role rather than gate on it should
/// keep the `None` and fall back to the last `SetActive` the cloud sent
/// (`QconnectRemoteSyncState::local_render_active`), which is authoritative.
pub fn local_renderer_role(session: &QconnectSessionState) -> Option<bool> {
    match (session.active_renderer_id, session.local_renderer_id) {
        (Some(active_renderer_id), Some(local_renderer_id)) => {
            Some(active_renderer_id == local_renderer_id)
        }
        // `normalize_active_renderer_id` maps the cloud's `-1` to `None`: the
        // session genuinely has no active renderer, so we are not it.
        (None, _) => Some(false),
        (Some(_), None) => None,
    }
}

pub fn is_local_renderer_active(session: &QconnectSessionState) -> bool {
    local_renderer_role(session).unwrap_or(false)
}

pub fn find_unique_renderer_id(
    session: &QconnectSessionState,
    predicate: impl Fn(&QconnectRendererInfo) -> bool,
) -> Option<i32> {
    let mut matches = session
        .renderers
        .iter()
        .filter(|renderer| predicate(renderer))
        .map(|renderer| renderer.renderer_id);

    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }

    Some(first)
}

// ---------------------------------------------------------------------------
// Session-projection helpers. Pure functions over the cloud queue + cached
// per-renderer state, building a renderer projection from one definition.
// ---------------------------------------------------------------------------

/// Snapshot the queue item a cursor points at, normalizing the queue_item_id for
/// in-queue cursors (autoplay items carry their own id verbatim).
pub fn queue_item_snapshot_for_cursor(
    queue: &QConnectQueueState,
    cursor: QconnectOrderedQueueCursor,
) -> Option<QueueItem> {
    match cursor {
        QconnectOrderedQueueCursor::Queue(index) => {
            queue.queue_items.get(index).cloned().map(|mut item| {
                item.queue_item_id = normalize_current_queue_item_id_from_queue_state(queue, index);
                item
            })
        }
        QconnectOrderedQueueCursor::Autoplay(index) => queue.autoplay_items.get(index).cloned(),
    }
}

/// Build a renderer-state projection from the cloud queue + a cached per-renderer
/// state, resolving the current/next track by the renderer's current_queue_item_id
/// against the ordered cursors. `session_loop_mode` is the fallback loop mode when
/// the renderer state omits one.
pub fn build_session_renderer_snapshot(
    queue: &QConnectQueueState,
    renderer_state: Option<&QconnectSessionRendererState>,
    session_loop_mode: Option<i32>,
) -> QConnectRendererState {
    let renderer_state = renderer_state.cloned().unwrap_or_default();
    let cursors = ordered_queue_cursors(queue);
    let current_index =
        find_cursor_index_by_queue_item_id(&cursors, queue, renderer_state.current_queue_item_id);
    let current_track =
        current_index.and_then(|index| queue_item_snapshot_for_cursor(queue, cursors[index]));
    let next_track = current_index
        .and_then(|index| cursors.get(index + 1).copied())
        .and_then(|cursor| queue_item_snapshot_for_cursor(queue, cursor));

    QConnectRendererState {
        active: renderer_state.active,
        playing_state: renderer_state.playing_state,
        current_position_ms: renderer_state.current_position_ms,
        // Projection of the CLOUD's view of a renderer, which never carries a
        // duration. Only a local renderer knows its own, and it publishes that
        // separately (`update_renderer_duration`).
        current_duration_ms: None,
        current_track,
        next_track,
        volume: renderer_state.volume,
        volume_delta: None,
        muted: renderer_state.muted,
        max_audio_quality: renderer_state.max_audio_quality,
        loop_mode: renderer_state.loop_mode.or(session_loop_mode),
        shuffle_mode: renderer_state.shuffle_mode,
        updated_at_ms: renderer_state.updated_at_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_join_reason_is_reconnection_only_after_a_drop() {
        assert_eq!(
            deferred_join_reason(false),
            JOIN_SESSION_REASON_CONTROLLER_REQUEST
        );
        assert_eq!(deferred_join_reason(true), JOIN_SESSION_REASON_RECONNECTION);
    }

    #[test]
    fn reask_queue_state_stops_once_session_uuid_known_or_budget_spent() {
        assert!(should_reask_queue_state(false, 0, 5));
        assert!(should_reask_queue_state(false, 4, 5));
        assert!(!should_reask_queue_state(false, 5, 5));
        assert!(!should_reask_queue_state(true, 0, 5));
    }

    #[test]
    fn only_a_slot_held_before_the_drop_is_reclaimed() {
        // A reconnect after holding the slot, which nobody took meanwhile.
        assert!(should_reclaim_render(true, true));
        // Someone else took it while we were away: theirs now.
        assert!(!should_reclaim_render(true, false));
        // A fresh connect to an idle session: not ours to take. This was the
        // auto-claim, and the case the rule exists to refuse.
        assert!(!should_reclaim_render(false, true));
        assert!(!should_reclaim_render(false, false));
    }

    #[test]
    fn renderer_status_from_wire_maps_known_values() {
        assert_eq!(RendererStatus::from_wire(Some(0)), RendererStatus::Inactive); // UNKNOWN collapses
        assert_eq!(
            RendererStatus::from_wire(Some(1)),
            RendererStatus::ActiveConnected
        );
        assert_eq!(
            RendererStatus::from_wire(Some(2)),
            RendererStatus::ActiveDisconnected
        );
        assert_eq!(RendererStatus::from_wire(Some(3)), RendererStatus::Inactive);
    }

    #[test]
    fn renderer_status_from_wire_collapses_unknown_and_missing_to_inactive() {
        assert_eq!(
            RendererStatus::from_wire(Some(99)),
            RendererStatus::Inactive
        ); // UNRECOGNIZED
        assert_eq!(RendererStatus::from_wire(None), RendererStatus::Inactive); // absent field
    }

    #[test]
    fn watchdog_arms_only_for_playing_active_peer() {
        const PLAYING_STATE_UNKNOWN: i32 = 0;
        const PLAYING_STATE_STOPPED: i32 = 1;
        const PLAYING_STATE_PAUSED: i32 = 3;
        // Arm: playing AND active peer.
        assert!(should_arm_renderer_watchdog(
            Some(PLAYING_STATE_PLAYING),
            true
        ));
        // Do not arm when paused/stopped/unknown even if active peer.
        assert!(!should_arm_renderer_watchdog(
            Some(PLAYING_STATE_PAUSED),
            true
        ));
        assert!(!should_arm_renderer_watchdog(
            Some(PLAYING_STATE_STOPPED),
            true
        ));
        assert!(!should_arm_renderer_watchdog(
            Some(PLAYING_STATE_UNKNOWN),
            true
        ));
        assert!(!should_arm_renderer_watchdog(None, true));
        // Do not arm when not an active peer (e.g. local renderer is active).
        assert!(!should_arm_renderer_watchdog(
            Some(PLAYING_STATE_PLAYING),
            false
        ));
    }

    #[test]
    fn quality_from_max_audio_quality_maps_levels() {
        // qbz Quality has four variants: Mp3, Lossless (CD), HiRes (<=96kHz),
        // UltraHiRes (>96kHz). QConnect levels collapse onto these.
        assert_eq!(quality_from_max_audio_quality(Some(0)), Quality::Mp3);
        assert_eq!(quality_from_max_audio_quality(Some(1)), Quality::Mp3);
        assert_eq!(quality_from_max_audio_quality(Some(2)), Quality::Lossless);
        assert_eq!(quality_from_max_audio_quality(Some(3)), Quality::HiRes);
        assert_eq!(quality_from_max_audio_quality(Some(4)), Quality::UltraHiRes);
        assert_eq!(quality_from_max_audio_quality(Some(5)), Quality::UltraHiRes);
        assert_eq!(quality_from_max_audio_quality(None), Quality::UltraHiRes);
        assert_eq!(
            quality_from_max_audio_quality(Some(99)),
            Quality::UltraHiRes
        );
    }

    /// upstream#693 — the cap is a CEILING on the controller's request.
    #[test]
    fn cap_quality_is_a_ceiling_and_never_a_floor() {
        use Quality::{HiRes, Lossless, Mp3, UltraHiRes};

        // No cap: the controller's request stands.
        assert_eq!(cap_quality(UltraHiRes, None), UltraHiRes);

        // A cap below the request clamps — including the `None` level that
        // resolves to UltraHiRes, which is how a silent controller used to win.
        assert_eq!(cap_quality(UltraHiRes, Some(Lossless)), Lossless);
        assert_eq!(cap_quality(HiRes, Some(Mp3)), Mp3);

        // A cap at or ABOVE the request leaves it alone: a phone asking for MP3
        // on cellular data does not get upgraded to hi-res by our config.
        assert_eq!(cap_quality(Lossless, Some(Lossless)), Lossless);
        assert_eq!(cap_quality(Mp3, Some(UltraHiRes)), Mp3);
    }

    /// The tiers must sort by audio quality, not by the Qobuz format ids the
    /// enum carries — a new variant added out of id order must not reorder the
    /// cap silently.
    #[test]
    fn quality_tiers_ascend_with_quality() {
        assert!(quality_tier(Quality::Mp3) < quality_tier(Quality::Lossless));
        assert!(quality_tier(Quality::Lossless) < quality_tier(Quality::HiRes));
        assert!(quality_tier(Quality::HiRes) < quality_tier(Quality::UltraHiRes));
    }

    /// The ids answer the role outright in two shapes and cannot in the third.
    /// Keeping the third as `None` is the whole point: a renderer whose own id
    /// has not resolved yet is NOT the same thing as a renderer a peer took
    /// over, and publishing them as the same value dropped moOde's overlay
    /// mid-playback.
    #[test]
    fn local_renderer_role_separates_undetermined_from_not_active() {
        let role = |active, local| {
            let session = QconnectSessionState {
                active_renderer_id: active,
                local_renderer_id: local,
                ..Default::default()
            };
            local_renderer_role(&session)
        };

        assert_eq!(role(Some(7), Some(7)), Some(true), "we are the active one");
        assert_eq!(role(Some(9), Some(7)), Some(false), "a peer took it");
        // `normalize_active_renderer_id` already mapped the cloud's -1 to None.
        assert_eq!(role(None, Some(7)), Some(false), "nobody renders");
        assert_eq!(role(None, None), Some(false), "idle session");
        assert_eq!(role(Some(9), None), None, "our own id is not resolved yet");
    }

    /// The gating callers must keep behaving exactly as before — they are
    /// report suppressors, and `None` has to collapse to "do not report".
    #[test]
    fn is_local_renderer_active_still_collapses_undetermined_to_false() {
        let session = QconnectSessionState {
            active_renderer_id: Some(9),
            local_renderer_id: None,
            ..Default::default()
        };
        assert_eq!(local_renderer_role(&session), None);
        assert!(!is_local_renderer_active(&session));
    }
}
