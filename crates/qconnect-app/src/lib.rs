//! qconnect-app
//!
//! Application adapter that composes qconnect core + protocol + transport.

mod app;
#[cfg(test)]
mod controller_harness;
#[cfg(test)]
mod controller_sync_tests;
mod error;
mod events;
pub mod queue_resolution;
pub mod renderer;
mod renderer_engine;
pub mod session;
pub(crate) mod startup;
mod state;
mod sync_state;

pub use app::{QconnectApp, SessionLoopHost};
pub use events::{QconnectAppEvent, QconnectEventSink};
pub use qconnect_core::RendererCommand;
pub use qconnect_protocol::{QueueCommandType, RendererReport, RendererReportType};
pub use renderer_engine::QconnectRendererEngine;
pub use session::{
    build_session_renderer_snapshot, is_local_renderer_active, is_peer_renderer_active,
    LocalIdentity, QconnectFileAudioQualitySnapshot, QconnectLifecycleState, QconnectSessionState,
    JOIN_SESSION_REASON_RECONNECTION,
};
pub use startup::QconnectStartupMode;
pub use sync_state::{cache_renderer_snapshot, QconnectRemoteSyncState};

pub use error::QconnectAppError;
pub use qconnect_core::{QConnectQueueState, QConnectRendererState};
pub use state::QconnectRuntimeState;
pub(crate) use sync_state::{ensure_session_renderer_state, sync_session_renderer_active_flags};
