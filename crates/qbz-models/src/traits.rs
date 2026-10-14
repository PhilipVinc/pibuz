//! Core traits for QBZ
//!
//! - FrontendAdapter: Receives events from the core

use async_trait::async_trait;

use crate::events::CoreEvent;

/// Adapter trait the host implements to receive events from QBZ core.
///
/// The core calls `on_event` whenever something happens that the host should
/// know about. pibuz's adapter fans every event out on a broadcast bus.
///
/// # Example
///
/// ```rust,ignore
/// use qbz_models::{FrontendAdapter, CoreEvent};
///
/// struct BusAdapter {
///     tx: tokio::sync::broadcast::Sender<CoreEvent>,
/// }
///
/// #[async_trait]
/// impl FrontendAdapter for BusAdapter {
///     async fn on_event(&self, event: CoreEvent) {
///         // No live receiver is not an error.
///         let _ = self.tx.send(event);
///     }
/// }
/// ```
// `double_must_use` (clippy 1.99) fires on what `#[async_trait]` generates, not
// on anything written here: it marks each method `#[must_use]` and returns a
// boxed future, which is must_use already. `unknown_lints` because toolchains
// before 1.99 do not know the name.
#[allow(
    unknown_lints,
    clippy::double_must_use,
    reason = "fires on #[async_trait]'s generated code, not on this trait"
)]
#[async_trait]
pub trait FrontendAdapter: Send + Sync {
    /// Called when the core emits an event.
    ///
    /// Implementations should handle this efficiently (non-blocking) since
    /// events may be emitted frequently (e.g., position updates every second).
    async fn on_event(&self, event: CoreEvent);

    /// Optional: Called when the core is ready for interaction.
    /// Default implementation does nothing.
    async fn on_ready(&self) {}

    /// Optional: Called when the core is shutting down.
    /// Default implementation does nothing.
    async fn on_shutdown(&self) {}
}

/// A no-op adapter for testing or headless operation
pub struct NoOpAdapter;

#[async_trait]
impl FrontendAdapter for NoOpAdapter {
    async fn on_event(&self, _event: CoreEvent) {
        // Intentionally empty - events are discarded
    }
}

/// An adapter that logs all events (useful for debugging)
pub struct LoggingAdapter {
    prefix: String,
}

impl LoggingAdapter {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
}

impl Default for LoggingAdapter {
    fn default() -> Self {
        Self::new("CoreEvent")
    }
}

#[async_trait]
impl FrontendAdapter for LoggingAdapter {
    async fn on_event(&self, event: CoreEvent) {
        log::debug!("{}: {:?}", self.prefix, event);
    }

    async fn on_ready(&self) {
        log::info!("{}: Core is ready", self.prefix);
    }

    async fn on_shutdown(&self) {
        log::info!("{}: Core is shutting down", self.prefix);
    }
}
