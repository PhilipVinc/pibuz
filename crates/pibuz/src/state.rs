// crates/pibuz/src/state.rs — shared in-memory daemon state (one
// `Arc<Mutex<DaemonShared>>` shared by the playback driver + the HTTP API).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub(crate) struct LatchedErrors {
    // drain-once channels become latches
    pub(crate) stream: Option<String>,
    pub(crate) auth: Option<String>,
    pub(crate) transport: Option<String>,
}

pub(crate) struct DaemonShared {
    // one Arc<Mutex<...>> shared by driver + API
    pub(crate) last_errors: LatchedErrors,
    pub(crate) driver_last_tick: Option<std::time::Instant>,
    /// The daemon's one mute, box and controller alike — see `crate::mute`.
    pub(crate) mute: crate::mute::Mute,
    pub(crate) started_at: std::time::Instant,
    pub(crate) startup_warnings: u32,
    pub qconnect: QconnectStatus,
    /// Coarse network-reachability signal for `/api/status`'s `network.online`.
    /// Latched ONLY from real network-class outcomes — never active probing:
    /// false on a QConnect reconnect-exhausted, true on a successful QConnect
    /// (re)connect. Defaults true (optimistic) until the first outcome latches
    /// it.
    pub(crate) network_online: std::sync::atomic::AtomicBool,
    /// CoreEvent bus handle so state mutations here can publish matching bus
    /// events (`emit_qconnect_session_changed`). None until `daemon::run`
    /// attaches it right after boot; tests leave it None.
    pub(crate) bus: Option<tokio::sync::broadcast::Sender<qbz_models::CoreEvent>>,
}

impl DaemonShared {
    /// Read the latched network-reachability signal. `Relaxed` is
    /// sufficient — this is a coarse status flag read under the same
    /// `Mutex<DaemonShared>` guard as every other field here, not a
    /// synchronization primitive of its own.
    pub(crate) fn network_online(&self) -> bool {
        self.network_online
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Latch the network-reachability signal. See the field doc for exactly
    /// which call sites are allowed to call this.
    pub(crate) fn set_network_online(&self, online: bool) {
        self.network_online
            .store(online, std::sync::atomic::Ordering::Relaxed);
    }

    /// Publish an event on the daemon bus (SSE subscribers and the event hook).
    /// A no-op before the bus exists, which is the case during early startup.
    pub(crate) fn emit(&self, event: qbz_models::CoreEvent) {
        if let Some(bus) = &self.bus {
            let _ = bus.send(event);
        }
    }

    /// The Connect session is gone (`state` says how: "off", "exhausted").
    ///
    /// Clears `is_active` with `session_active`: with no session we are
    /// nobody's renderer, and `is_active` used to be written only by the sink's
    /// ownership latch, so `/api/status` went on saying `is_active: true` after
    /// `pibuz qconnect disable` or a reconnect that gave up.
    pub(crate) fn latch_qconnect_session_down(&mut self, state: &str) {
        self.qconnect.state = state.to_string();
        self.qconnect.session_active = false;
        self.qconnect.is_active = false;
    }

    /// Publish the CURRENT `qconnect` block as a `QconnectSessionChanged` bus
    /// event (SSE `/api/events`, `pibuz watch`, the event hook). Call AFTER
    /// mutating `self.qconnect`; a best-effort no-op before the bus attaches
    /// or when no receiver is subscribed.
    pub(crate) fn emit_qconnect_session_changed(&self) {
        if let Some(bus) = &self.bus {
            let _ = bus.send(qbz_models::CoreEvent::QconnectSessionChanged {
                state: self.qconnect.state.clone(),
                device_name: (!self.qconnect.device_name.is_empty())
                    .then(|| self.qconnect.device_name.clone()),
                session_active: self.qconnect.session_active,
            });
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub(crate) struct QconnectStatus {
    pub(crate) enabled: bool,
    pub state: String, // "off"|"connecting"|"connected"|"retrying"|"exhausted"
    pub(crate) session_active: bool,
    /// The session's ACTIVE RENDERER is this device.
    ///
    /// `is_active` is what every other Qobuz Connect implementation calls this,
    /// and what the wire calls it (`RndrSrvrJoinSession.is_active`,
    /// `SrvrRndrSetActive.active`).
    ///
    /// Distinct from `session_active`, which only says the cloud connection is
    /// up: a controller that switches to its own speakers leaves us connected
    /// but no longer rendering. moOde reads this to decide whether the Qobuz
    /// overlay should still own the screen.
    pub(crate) is_active: bool,
    pub device_name: String,
    pub(crate) last_transport_reconnect: Option<String>,
    /// The /streamcore pairing listener is serving (mDNS advertisement is
    /// best-effort and may have failed independently).
    pub pairing: bool,
    /// The pairing listener's port, when serving.
    pub pairing_port: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latched_errors_default_all_none() {
        let e = LatchedErrors::default();
        assert!(e.stream.is_none());
        assert!(e.auth.is_none());
        assert!(e.transport.is_none());
    }

    #[test]
    fn qconnect_status_default_is_off_and_inactive() {
        let q = QconnectStatus::default();
        assert!(!q.enabled);
        assert_eq!(q.state, "");
        assert!(!q.session_active);
        assert_eq!(q.device_name, "");
        assert!(q.last_transport_reconnect.is_none());
        assert!(!q.pairing);
        assert!(q.pairing_port.is_none());
    }

    #[test]
    fn daemon_shared_holds_the_fields_the_status_route_needs() {
        // Construction smoke test: DaemonShared has no derive (Instant isn't
        // Serialize) so this is the only compile-time guard that the field
        // set/types stay what api::status::assemble expects.
        let shared = DaemonShared {
            last_errors: LatchedErrors::default(),
            driver_last_tick: None,
            mute: crate::mute::Mute::default(),
            started_at: std::time::Instant::now(),
            startup_warnings: 0,
            qconnect: QconnectStatus::default(),
            network_online: std::sync::atomic::AtomicBool::new(true),
            bus: None,
        };
        assert!(shared.network_online());
    }

    #[test]
    fn a_session_going_down_clears_the_active_renderer_flag() {
        let mut shared = DaemonShared {
            last_errors: LatchedErrors::default(),
            driver_last_tick: None,
            mute: crate::mute::Mute::default(),
            started_at: std::time::Instant::now(),
            startup_warnings: 0,
            qconnect: QconnectStatus::default(),
            network_online: std::sync::atomic::AtomicBool::new(true),
            bus: None,
        };
        shared.qconnect.state = "connected".to_string();
        shared.qconnect.session_active = true;
        shared.qconnect.is_active = true;
        shared.latch_qconnect_session_down("off");
        assert_eq!(shared.qconnect.state, "off");
        assert!(!shared.qconnect.session_active);
        assert!(
            !shared.qconnect.is_active,
            "/api/status still says we render"
        );
    }

    #[test]
    fn network_online_latches_false_then_true() {
        // Pure latch semantics: a real network-class failure flips
        // it false, a real success flips it back true — the exact two
        // transitions every call site above drives. Defaults true.
        let shared = DaemonShared {
            last_errors: LatchedErrors::default(),
            driver_last_tick: None,
            mute: crate::mute::Mute::default(),
            started_at: std::time::Instant::now(),
            startup_warnings: 0,
            qconnect: QconnectStatus::default(),
            network_online: std::sync::atomic::AtomicBool::new(true),
            bus: None,
        };
        assert!(shared.network_online(), "defaults true (optimistic)");

        shared.set_network_online(false);
        assert!(!shared.network_online(), "set false -> reads back false");

        shared.set_network_online(true);
        assert!(shared.network_online(), "set true -> reads back true");
    }
}
