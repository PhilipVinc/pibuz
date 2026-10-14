#[derive(Debug, Clone)]
pub struct WsTransportConfig {
    pub endpoint_url: String,
    pub jwt_qws: Option<String>,
    /// When `true`, a connect attempt with `jwt_qws == None` is a hard
    /// credential error instead of silently skipping the AUTHENTICATE frame.
    /// Defaults to `false` so the InMemory / test transport path keeps working
    /// without a JWT.
    pub require_jwt: bool,
    pub reconnect_backoff_ms: u64,
    pub reconnect_backoff_max_ms: u64,
    /// Maximum number of consecutive reconnect attempts before the transport
    /// gives up and shuts down. The counter resets only when a session-level
    /// join is confirmed (cloud emits MESSAGE_TYPE_SRVR_CTRL_SESSION_STATE),
    /// not when the WS / TCP connection succeeds — Qobuz cloud accepts the WS
    /// connection before rejecting the session join, so a TCP-level reset
    /// would mask the failure mode behind issue #358.
    ///
    /// `None` means unlimited (legacy behavior, retained for tests).
    pub reconnect_max_attempts: Option<u32>,
    /// When `> 0`, reaching `Exhausted` no longer terminates the transport
    /// loop: it idles this long (shutdown-cancellable), resets the attempt
    /// counter / backoff to base, and retries instead of giving up.
    /// Default `0` preserves the legacy terminate-on-exhausted behavior used
    /// by tests; the real config sets 60s.
    pub reconnect_idle_retry_ms: u64,
    pub connect_timeout_ms: u64,
    pub keepalive_interval_ms: u64,
    pub auto_subscribe: bool,
    pub subscribe_channels: Vec<Vec<u8>>,
    pub qcloud_proto: u32,
}

impl Default for WsTransportConfig {
    fn default() -> Self {
        Self {
            endpoint_url: String::new(),
            jwt_qws: None,
            require_jwt: false,
            reconnect_backoff_ms: 2_000,
            reconnect_backoff_max_ms: 30_000,
            reconnect_max_attempts: Some(10),
            reconnect_idle_retry_ms: 0,
            connect_timeout_ms: 10_000,
            keepalive_interval_ms: 30_000,
            auto_subscribe: true,
            subscribe_channels: Vec::new(),
            qcloud_proto: 1,
        }
    }
}
