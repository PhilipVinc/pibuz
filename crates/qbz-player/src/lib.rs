//! QBZ Player - Playback engine and queue management
//!
//! This crate provides:
//! - QueueManager: Track queue management with shuffle/repeat
//! - Player: Main playback engine
//! - StreamingSource: HTTP audio streaming
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     qbz-player (Tier 2)                     │
//! │  Queue management, playback engine, streaming              │
//! └─────────────────────────────────────────────────────────────┘
//!                              ↑
//!              ┌───────────────┼───────────────┐
//!              │               │               │
//!         ┌────┴────┐    ┌─────┴─────┐   ┌─────┴─────┐
//!         │qbz-audio│    │qbz-models │   │qbz-qobuz  │
//!         │ Tier 1  │    │  Tier 0   │   │  Tier 1   │
//!         └─────────┘    └───────────┘   └───────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use qbz_player::{Player, QueueManager};
//! use qbz_audio::{AudioSettings, AudioDiagnostic};
//!
//! // `None` for the profile root: the loudness cache then stays in memory.
//! let player = Player::new(None, AudioSettings::default(), AudioDiagnostic::new(), None);
//! let queue = QueueManager::new();
//! ```

pub mod player;
pub mod queue;

// Re-export main types
pub use player::{
    release_finished_track_from, BufferWriter, BufferedMediaSource, CacheReport, DiskCacheReport,
    DiskTee, FetchPlan, IncrementalStreamingSource, PlaybackEvent, PlaybackState, Player,
    SharedState, StreamSeekMode, StreamingConfig, TrackAudio,
};
pub use qbz_cache::{AudioCache, PlaybackCache, TrackBytes};
pub use queue::QueueManager;
