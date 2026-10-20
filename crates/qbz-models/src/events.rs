//! Core events
//!
//! This module defines all events that flow from the core to the host.
//! The host implements FrontendAdapter to receive these events.

use serde::Serialize;

use crate::playback::{PlaybackState, QueueState, QueueTrack};
use crate::types::UserSession;

/// All events emitted by QBZ core to the host's adapter
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum CoreEvent {
    // ============ Playback Events ============
    /// Track started playing
    TrackStarted {
        track: QueueTrack,
        position_secs: u64,
    },

    /// Playback state changed (play/pause/stop)
    PlaybackStateChanged { state: PlaybackState },

    /// Playback position updated (periodic, e.g., every second)
    PositionUpdated {
        position_secs: u64,
        duration_secs: u64,
    },

    /// Volume changed
    VolumeChanged { volume: f32 },

    // ============ Queue Events ============
    /// Queue state changed (tracks added/removed/reordered)
    QueueUpdated { state: QueueState },

    /// Shuffle mode changed
    ShuffleChanged { enabled: bool },

    /// Repeat mode changed
    RepeatModeChanged { mode: crate::playback::RepeatMode },

    // ============ Qobuz Connect Events ============
    /// Qobuz Connect session lifecycle changed (headless renderer)
    QconnectSessionChanged {
        /// "off" | "connecting" | "connected" | "retrying" | "exhausted"
        state: String,
        /// The advertised renderer name, when one is set
        device_name: Option<String>,
        /// True only while a session is established ("connected")
        session_active: bool,
        /// True while the session's active renderer is this device. A
        /// controller that disconnects, or moves playback elsewhere, leaves
        /// `session_active` true and turns this false.
        is_active: bool,
    },

    // ============ Authentication Events ============
    /// User logged in successfully
    LoggedIn { session: UserSession },

    /// User logged out
    LoggedOut,

    // ============ Error Events ============
    /// An error occurred
    Error {
        code: String,
        message: String,
        /// Whether error is recoverable
        recoverable: bool,
    },

    /// Playback error (track couldn't play)
    PlaybackError { track_id: u64, message: String },
}
