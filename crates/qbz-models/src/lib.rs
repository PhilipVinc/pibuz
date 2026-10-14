//! QBZ Models - Shared types, events, and traits
//!
//! This crate provides the foundation for all QBZ crates:
//! - Type definitions (Track, Album, Artist, etc.)
//! - Event definitions (CoreEvent enum)
//! - Trait definitions (FrontendAdapter)
//! - Playback types (QueueTrack, PlaybackState)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      qbz-models (Tier 0)                    │
//! │  Types, Events, Traits - No dependencies on other qbz-*    │
//! └─────────────────────────────────────────────────────────────┘
//!                              ↑
//!     ┌────────────────────────┼────────────────────────┐
//!     │                        │                        │
//! ┌───┴───┐              ┌─────┴─────┐            ┌─────┴─────┐
//! │qbz-audio│            │qbz-qobuz  │            │qbz-player │
//! │ Tier 1 │             │  Tier 1   │            │  Tier 2   │
//! └────────┘             └───────────┘            └───────────┘
//! ```
//!
//! # Usage
//!
//! ```rust
//! use qbz_models::{Track, Album, CoreEvent, FrontendAdapter};
//! ```

pub(crate) mod events;
pub(crate) mod playback;
pub mod settings_file;
pub mod system_capabilities;
pub(crate) mod traits;
pub(crate) mod types;

// Re-export commonly used types at crate root
pub use events::CoreEvent;
pub use playback::{PlaybackState, QueueState, QueueTrack, RepeatMode};
pub use traits::{FrontendAdapter, LoggingAdapter, NoOpAdapter};
pub use types::{
    Album, AlbumSummary, Artist, ImageSet, Playlist, Quality, SessionStartResponse,
    StreamQualityInfo, StreamRestriction, StreamUrl, Track, TrackFileUrl, TrackToAnalyse,
    TracksContainer, UserSession,
};
