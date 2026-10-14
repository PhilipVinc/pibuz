use async_trait::async_trait;
use qconnect_core::{QConnectQueueState, QConnectRendererState, RendererCommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::QconnectLifecycleState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QconnectAppEvent {
    TransportConnected,
    TransportDisconnected,
    QueueUpdated(QConnectQueueState),
    RendererUpdated(QConnectRendererState),
    RendererCommandApplied {
        command: RendererCommand,
        state: QConnectRendererState,
    },
    PendingActionStarted {
        uuid: String,
    },
    PendingActionCompleted {
        uuid: String,
    },
    PendingActionTimedOut {
        uuid: String,
        timeout_ms: u64,
    },
    PendingActionCanceledByConcurrentRemoteEvent {
        pending_uuid: String,
        remote_action_uuid: String,
    },
    QueueErrorIgnoredByConcurrency {
        action_uuid: String,
    },
    QueueResyncTriggered,
    /// Renderer per-track playback failure (PlaybackErrorMessage) received
    /// from the server.
    PlaybackError {
        queue_item_id: u64,
        error_type: qconnect_protocol::ErrorType,
        queue_version: Option<qconnect_core::QueueVersion>,
    },
    /// Session-level error from the server (envelope `error`, message type 1).
    ServerError {
        code: String,
        message: String,
    },
    /// Session management event from server (types 81-87, 97-101).
    /// These don't affect the queue reducer but provide session topology info.
    SessionManagementEvent {
        message_type: String,
        payload: Value,
    },
    /// Active peer renderer went silent >=12s while PLAYING (liveness watchdog).
    RendererUnreachable {
        renderer_id: i32,
    },
    /// Active peer renderer left gracefully (status == ACTIVE_DISCONNECTED).
    RendererDisconnected {
        renderer_id: i32,
    },
    /// Post-reconnect resync has been issued (safe-to-replay advisory).
    ResyncComplete,
    /// Granular connection lifecycle changed (Connecting/Reconnecting/Connected/Exhausted/Off).
    LifecycleChanged {
        state: QconnectLifecycleState,
    },
    /// Pass-through diagnostic (inbound-event preview, cloud error, max-reconnect, ...).
    /// `channel` names the source, e.g. `qconnect:cloud_error`.
    Diagnostic {
        channel: String,
        level: String,
        payload: Value,
    },
}

// clippy 1.99 `double_must_use` fires on `#[async_trait]`'s output, not on
// this code; see `qbz_models::FrontendAdapter`.
#[allow(unknown_lints, clippy::double_must_use)]
#[async_trait]
pub trait QconnectEventSink: Send + Sync {
    async fn on_event(&self, event: QconnectAppEvent);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;

    #[derive(Debug, Default, Clone)]
    struct TestSink {
        events: Arc<Mutex<Vec<QconnectAppEvent>>>,
    }

    #[async_trait]
    impl QconnectEventSink for TestSink {
        async fn on_event(&self, event: QconnectAppEvent) {
            self.events.lock().await.push(event);
        }
    }

    /// The liveness/lifecycle/diagnostic variants flow through the
    /// `QconnectEventSink` trait. A sink receives each one verbatim.
    #[tokio::test]
    async fn new_variants_flow_through_sink() {
        let sink = TestSink::default();
        sink.on_event(QconnectAppEvent::RendererUnreachable { renderer_id: 7 })
            .await;
        sink.on_event(QconnectAppEvent::RendererDisconnected { renderer_id: 9 })
            .await;
        sink.on_event(QconnectAppEvent::ResyncComplete).await;
        sink.on_event(QconnectAppEvent::LifecycleChanged {
            state: QconnectLifecycleState::Reconnecting,
        })
        .await;
        sink.on_event(QconnectAppEvent::Diagnostic {
            channel: "qconnect:cloud_error".to_string(),
            level: "warning".to_string(),
            payload: serde_json::json!({ "code": 13 }),
        })
        .await;

        let events = sink.events.lock().await.clone();
        assert_eq!(events.len(), 5);
        assert!(matches!(
            events[0],
            QconnectAppEvent::RendererUnreachable { renderer_id: 7 }
        ));
        assert!(matches!(
            events[1],
            QconnectAppEvent::RendererDisconnected { renderer_id: 9 }
        ));
        assert!(matches!(events[2], QconnectAppEvent::ResyncComplete));
        assert!(matches!(
            events[3],
            QconnectAppEvent::LifecycleChanged {
                state: QconnectLifecycleState::Reconnecting
            }
        ));
        assert!(matches!(
            &events[4],
            QconnectAppEvent::Diagnostic { channel, level, .. }
                if channel == "qconnect:cloud_error" && level == "warning"
        ));
    }

    /// The lifecycle state must serialize snake_case.
    #[test]
    fn lifecycle_state_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(QconnectLifecycleState::Reconnecting).unwrap(),
            serde_json::json!("reconnecting")
        );
        assert_eq!(
            serde_json::to_value(QconnectLifecycleState::Connected).unwrap(),
            serde_json::json!("connected")
        );
        assert_eq!(
            serde_json::to_value(QconnectLifecycleState::Exhausted).unwrap(),
            serde_json::json!("exhausted")
        );
    }
}
