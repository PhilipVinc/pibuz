// crates/pibuz/src/adapter.rs — the daemon's FrontendAdapter.
//
// The daemon has no UI, so it fans every CoreEvent out over a tokio broadcast
// bus; SSE (`/api/events`), the event hook and the QConnect session all
// subscribe to the same bus. The adapter itself is a thin, cheap forwarder —
// `on_event` never blocks.
use async_trait::async_trait;
use qbz_models::{CoreEvent, FrontendAdapter};
use tokio::sync::broadcast;

/// Broadcast fan-out adapter handed to `AppRuntime`/`QbzCore`. Every event the
/// core emits is re-published to all live subscribers.
pub(crate) struct DaemonAdapter {
    tx: broadcast::Sender<CoreEvent>,
}

impl DaemonAdapter {
    /// Build the adapter and return an initial subscriber. Callers that only
    /// need a producing handle can `drop` the receiver and later `subscribe()`
    /// via [`DaemonAdapter::sender`].
    pub fn new() -> (Self, broadcast::Receiver<CoreEvent>) {
        // CoreEvent is Clone (qbz-models/src/events.rs); 256 slots absorbs a
        // burst of position ticks before a slow subscriber sees Lagged.
        let (tx, rx) = broadcast::channel(256);
        (Self { tx }, rx)
    }

    /// A cloned producer handle to the same bus (used to hand the sender to
    /// other boot components while the adapter is moved into the runtime).
    pub(crate) fn sender(&self) -> broadcast::Sender<CoreEvent> {
        self.tx.clone()
    }
}

#[async_trait]
impl FrontendAdapter for DaemonAdapter {
    async fn on_event(&self, event: CoreEvent) {
        // A send with no live receivers returns Err — expected during boot,
        // before the driver subscribes. Never fatal.
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwards_events_to_bus() {
        let (a, mut rx) = DaemonAdapter::new();
        a.on_event(CoreEvent::LoggedOut).await;
        assert!(matches!(rx.recv().await.unwrap(), CoreEvent::LoggedOut));
    }

    #[tokio::test]
    async fn sender_reaches_a_late_subscriber() {
        let (a, _rx) = DaemonAdapter::new();
        let mut late = a.sender().subscribe();
        a.on_event(CoreEvent::LoggedOut).await;
        assert!(matches!(late.recv().await.unwrap(), CoreEvent::LoggedOut));
    }

    #[tokio::test]
    async fn send_with_no_receivers_is_not_fatal() {
        let (a, rx) = DaemonAdapter::new();
        drop(rx);
        // Must not panic even though nobody is listening (boot ordering).
        a.on_event(CoreEvent::LoggedOut).await;
    }
}
