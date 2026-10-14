use qconnect_core::{PendingActionSlot, QConnectQueueState, QConnectRendererState};

#[derive(Debug, Default)]
pub struct QconnectRuntimeState {
    pub(crate) queue: QConnectQueueState,
    pub renderer: QConnectRendererState,
    pub(crate) pending: PendingActionSlot,
    pub(crate) transport_connected: bool,
    pub(crate) concurrency_canceled_action_uuid: Option<String>,
}
