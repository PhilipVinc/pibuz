//! qconnect-core
//!
//! Queue and renderer-state domain primitives for Qobuz Connect.

pub mod pending;
pub mod queue;
pub mod reducer;
pub mod renderer;

pub use pending::{PendingActionError, PendingActionSlot, PendingCorrelation, PendingQueueAction};
pub use queue::{QConnectQueueState, QueueEvent, QueueItem, QueueVersion};
pub use reducer::{apply_event, ReducerOutcome};
pub use renderer::{apply_renderer_command, QConnectRendererState, RendererCommand};
