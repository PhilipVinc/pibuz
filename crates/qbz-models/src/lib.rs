//! QBZ Models - Shared types, events, and traits
//!
//! This crate provides the foundation for all QBZ crates:
//! - Type definitions (Track, Album, Artist, etc.)
//! - Event definitions (CoreEvent enum)
//! - Trait definitions (FrontendAdapter)
//! - Playback types (QueueTrack, PlaybackState)
//! - Error types
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

pub mod error;
pub mod events;
pub mod lenient;
pub mod playback;
pub mod purchase_serde;
pub mod system_capabilities;
pub mod traits;
pub mod types;

// Re-export commonly used types at crate root
pub use error::{QbzError, QbzResult};
pub use events::CoreEvent;
pub use lenient::{parse_items_array, parse_items_lenient};
pub use playback::{PlaybackState, PlaybackStatus, QueueState, QueueTrack, RepeatMode};
pub use traits::{FrontendAdapter, LoggingAdapter, NoOpAdapter};
pub use types::{
    Album,
    AlbumAward,
    // Award types
    AlbumSummary,
    Artist,
    ArtistAlbums,
    ArtistBiography,
    // Artist page types
    // Discover types
    DiscoverAlbum,
    DiscoverAlbumDates,
    DiscoverAlbumImage,
    DiscoverArtist,
    DiscoverAudioInfo,
    Favorites,
    Genre,
    Goody,
    ImageSet,
    Label,
    Playlist,
    PlaylistGenre,
    PlaylistOwner,
    // Purchase types
    PurchaseAlbum,
    PurchaseIdsResponse,
    PurchaseResponse,
    PurchaseTrack,
    Quality,
    RadioResponse,
    SearchResults,
    SearchResultsPage,
    SessionStartResponse,
    StreamQualityInfo,
    StreamRestriction,
    StreamUrl,
    Track,
    TrackFileUrl,
    TrackToAnalyse,
    TracksContainer,
    UserSession,
};
