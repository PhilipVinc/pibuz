// crates/pibuz/src/api/settings.rs — `POST /api/settings/reload`. No body. The
// real work lives in `crate::daemon::reload` (the "reload entry point" —
// re-reads audio, the streaming-quality cell and the `[qconnect]` section);
// this route handler is a thin wrapper: run it, then answer with the SAME body
// as `GET /api/status` — zero new shapes (the reinit/reload narrative is
// composed CLIENT-side from the CLI's own copy of the Apply-ladder
// classification — never carried on the wire).
use std::io::Cursor;

use tiny_http::Response;

use super::ApiState;

/// `POST /api/settings/reload`. The serving thread is a plain `std::thread`
/// (never a tokio worker), so the async reload orchestration runs via
/// `state.rt.block_on` — the same pattern `status::assemble_live` already uses
/// for its queue read.
pub(crate) fn reload(state: &ApiState) -> Response<Cursor<Vec<u8>>> {
    state.rt.block_on(crate::daemon::reload(state));
    super::status::status(state)
}
