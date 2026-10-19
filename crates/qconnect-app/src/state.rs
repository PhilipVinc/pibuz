use qconnect_core::{PendingActionSlot, QConnectQueueState, QConnectRendererState};

#[derive(Debug, Default)]
pub struct QconnectRuntimeState {
    pub(crate) queue: QConnectQueueState,
    pub renderer: QConnectRendererState,
    pub(crate) pending: PendingActionSlot,
    pub(crate) transport_connected: bool,
    pub(crate) concurrency_canceled_action_uuid: Option<String>,
    /// The queue items our last state report named, as `(current, next)`:
    /// what the controller is showing for us now. See
    /// `QconnectApp::send_renderer_report`.
    pub(crate) reported_queue_items: Option<(u64, Option<u64>)>,
}
