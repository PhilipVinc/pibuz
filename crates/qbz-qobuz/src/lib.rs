//! QBZ Qobuz - Qobuz API client
//!
//! This crate provides the Qobuz API client:
//! - QobuzClient: Main API interface
//! - Authentication and token management
//! - Search, catalog browsing, streaming URLs
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     qbz-qobuz (Tier 1)                      │
//! │  Qobuz API client, authentication, streaming URLs          │
//! └─────────────────────────────────────────────────────────────┘
//!                              ↑
//!                      ┌───────┴───────┐
//!                      │  qbz-models   │
//!                      │   (Tier 0)    │
//!                      └───────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use qbz_qobuz::QobuzClient;
//!
//! let client = QobuzClient::new()?;
//! client.init().await?;
//! let session = client.login("email", "password").await?;
//! let stream_url = client.get_stream_url(track_id, quality).await?;
//! ```

pub(crate) mod auth;
pub mod bundle;
pub mod cdn;
pub mod client;
pub mod cmaf;
pub mod endpoints;
pub mod error;
pub(crate) mod forbidden_breaker;
pub mod link_resolver;
pub mod redact;
pub(crate) mod retry;

// Re-export main types
pub use bundle::BundleTokens;
pub use client::QobuzClient;
pub use cmaf::{DownloadedTrack, TrackDestination};
pub use error::{ApiError, Result};
pub use link_resolver::{resolve_link, LinkResolverError, ResolvedLink};
