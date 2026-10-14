//! Playback-related types for QBZ
//!
//! This module contains types related to audio playback:
//! - Queue track representation
//! - Repeat mode
//! - Queue state snapshots
//! - Playback state

use serde::{Deserialize, Serialize};

// ============ Queue Types ============

/// Track info stored in the queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTrack {
    pub id: u64,
    pub title: String,
    /// Subtitle/edition info from Qobuz (e.g. "Player's Ball Mix"), kept
    /// apart from the title (issue #360).
    #[serde(default)]
    pub version: Option<String>,
    pub artist: String,
    pub album: String,
    /// Album-level release variant ("2009 Remaster", "Hi-Res", …) — distinct
    /// from the per-track `version`. No enqueue path sets it.
    #[serde(default)]
    pub album_version: Option<String>,
    pub duration_secs: u64,
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub hires: bool,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<f64>,
    /// Local-library flag. There is no local library here; every enqueue
    /// path sets false.
    #[serde(default)]
    pub is_local: bool,
    /// Album ID for navigation
    pub album_id: Option<String>,
    /// Artist ID for navigation
    pub artist_id: Option<u64>,
    /// Whether the track is streamable (false = removed/unavailable)
    #[serde(default = "default_streamable")]
    pub streamable: bool,
    /// Source identifier (e.g. "qobuz", or the QConnect remote-queue source)
    #[serde(default)]
    pub source: Option<String>,
    /// Parental advisory / explicit content
    #[serde(default)]
    pub parental_warning: bool,
    /// Opaque identifier of the item that produced this track. The QConnect
    /// renderer sets it to the track's album_id; other enqueue paths leave it
    /// None. Nothing reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_item_id_hint: Option<String>,
    /// The container this track was launched FROM — its "playing from" origin.
    /// `context_kind` is one of "album" | "artist" | "playlist" | "label";
    /// `context_id` is that container's navigation id. Stamped per-track at
    /// enqueue time, so it always names the CURRENT track's true source (never
    /// a stale single global). None = no container origin (bare single-track /
    /// search play). `serde(default)`: older payloads restore as None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
}

fn default_streamable() -> bool {
    true
}

/// Repeat mode options
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

/// Queue state snapshot
#[derive(Debug, Clone, Serialize)]
pub struct QueueState {
    pub current_track: Option<QueueTrack>,
    pub current_index: Option<usize>,
    pub upcoming: Vec<QueueTrack>,
    pub history: Vec<QueueTrack>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub total_tracks: usize,
    pub stop_after_track_id: Option<u64>,
}

// ============ Playback State ============

/// Current playback state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PlaybackState {
    /// No track loaded
    #[default]
    Stopped,
    /// Track loaded and playing
    Playing,
    /// Track loaded but paused
    Paused,
    /// Loading/buffering track
    Loading,
}

// Note: Audio backend types (AudioBackendType, AudioDevice, etc.) are defined
// in qbz-audio crate to keep the audio module self-contained and immutable.
