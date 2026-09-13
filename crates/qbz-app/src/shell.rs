//! Application runtime facade.
//!
//! [`AppRuntime`] is the composition root the daemon builds on. It owns the
//! `Arc<QbzCore<A>>` every host command goes through.

use std::sync::Arc;

use qbz_audio::{AudioDiagnostic, AudioSettings};
use qbz_core::{FrontendAdapter, QbzCore};
use qbz_player::Player;

/// Composition root.
///
/// Generic over the [`FrontendAdapter`] the host supplies.
pub struct AppRuntime<A: FrontendAdapter + Send + Sync + 'static> {
    core: Arc<QbzCore<A>>,
}

impl<A: FrontendAdapter + Send + Sync + 'static> AppRuntime<A> {
    /// Build with explicit audio settings.
    ///
    /// Performs no disk or network access — used by tests and by shells that
    /// already have audio settings loaded.
    pub fn with_audio_settings(
        adapter: A,
        device_name: Option<String>,
        audio_settings: AudioSettings,
    ) -> Self {
        let diagnostic = AudioDiagnostic::new();
        let player = Player::new(device_name, audio_settings, diagnostic);
        let core = QbzCore::new(adapter, player);
        Self {
            core: Arc::new(core),
        }
    }

    /// Initialize the core (extracts Qobuz bundle tokens).
    ///
    /// Best-effort and offline-tolerant: a network failure here leaves the
    /// core usable for local/offline playback, matching [`QbzCore::init`].
    pub async fn init(&self) -> Result<(), String> {
        self.core.init().await.map_err(|e| e.to_string())
    }

    /// The orchestrator. Shells reach catalog, playback, queue, and auth
    /// functionality through this handle.
    pub fn core(&self) -> &Arc<QbzCore<A>> {
        &self.core
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qbz_core::NoOpAdapter;

    fn test_runtime() -> AppRuntime<NoOpAdapter> {
        // Building the runtime builds a `reqwest` client, and the workspace
        // pins reqwest's `rustls-tls-webpki-roots-no-provider` feature — so
        // the process-level rustls `CryptoProvider` must already be installed
        // or the constructor panics with "No provider set". `qbzd` installs it
        // in `main`; a test binary has no `main`, so it happens here.
        // Idempotent, so every test can call it.
        crate::ensure_crypto_provider();
        AppRuntime::with_audio_settings(NoOpAdapter, None, AudioSettings::default())
    }

    #[test]
    fn builds_with_explicit_audio_settings() {
        let rt = test_runtime();
        let _core = rt.core();
    }

    #[tokio::test]
    async fn core_reports_no_session_before_login() {
        let rt = test_runtime();
        assert!(!rt.core().has_session().await);
        assert!(!rt.core().is_api_initialized().await);
    }
}
