//! Qobuz purchase HTTP methods (Slice 2 of the Purchases port).
//!
//! Ported 1:1 from the Tauri reference at `src-tauri/src/api/client.rs`
//! (`get_user_purchases_page_typed` 1538, `get_user_purchases_ids_page_typed`
//! 1570, `get_user_purchases_all` 1612, `get_user_purchases_all_typed` 1683,
//! `get_track_file_url_by_format` 1768) plus `download_audio` from
//! `src-tauri/src/commands_v2/helpers.rs:332`.
//!
//! Behavioral divergence from the Tauri reference, intentional and correct:
//!
//! - Every purchase service request routes through `self.http()?` (the in-tree
//!   offline choke point) instead of the raw `self.http` the Tauri build uses.
//!   Purchases therefore fail fast without a connection, consistent
//!   with the rest of `qbz-qobuz`. The Tauri build had no shared offline gate.
//! - The CDN cross-feature gate (`CDN_STREAMING_ACTIVE`) is `src-tauri`-only; it
//!   guarded `download_audio` against concurrent streaming-vs-download CDN rate
//!   limiting. There is no equivalent shared counter in the Slint stack, so the
//!   ported `download_audio` has NO playback-vs-download collision protection.
//!   This is a documented limitation, NOT a 1:1 download-function gap — a true
//!   cross-frontend gate is a separate hardening item. Do NOT add a total
//!   request timeout "to be safe": large hi-res downloads legitimately exceed
//!   any fixed budget and the connect-timeout already bounds the dial phase.
//! - `download_audio` omits the reference's `.use_native_tls()` — this crate is
//!   rustls-only (no `native-tls` feature), matching `cmaf.rs::build_cdn_client`.
//!   `http1_only()` (the RST_STREAM/EOF fix) is retained.

use reqwest::StatusCode;
use serde_json::Value;

use super::auth::{get_timestamp, sign_get_file_url};
use super::client::QobuzClient;
use super::endpoints::{self, paths};
use super::error::{ApiError, Result};
use qbz_models::{
    PurchaseAlbum, PurchaseIdsResponse, PurchaseResponse, PurchaseTrack, SearchResultsPage,
    StreamRestriction, StreamUrl,
};

impl QobuzClient {
    /// Get one purchases page from Qobuz, optionally constrained by purchase
    /// type (`"albums"` / `"tracks"`; omitted if `None`).
    ///
    /// Header-auth, UNSIGNED — requires login (`authenticated_headers`).
    /// Ported from `src-tauri/src/api/client.rs:1538`.
    pub async fn get_user_purchases_page_typed(
        &self,
        purchase_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<PurchaseResponse> {
        let url = endpoints::build_url(paths::PURCHASE_GET_USER_PURCHASES);
        let mut query: Vec<(&str, String)> =
            vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(kind) = purchase_type {
            query.push(("type", kind.to_string()));
        }

        let http_response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&query)
            .send()
            .await?;
        log::debug!(
            "[Purchases] get_user_purchases_page(type={:?}, limit={}, offset={}) status={}",
            purchase_type,
            limit,
            offset,
            http_response.status()
        );
        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// Get one purchases-ids page from Qobuz, optionally constrained by purchase
    /// type. The items are OPAQUE — the UI reads only `.total` per type.
    ///
    /// Header-auth, UNSIGNED. `getUserPurchasesIds` is NOT in the OpenAPI spec;
    /// the code is the source of truth for its `{albums:{...}, tracks:{...}}`
    /// envelope. Ported from `src-tauri/src/api/client.rs:1570`.
    pub async fn get_user_purchases_ids_page_typed(
        &self,
        purchase_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<PurchaseIdsResponse> {
        let url = endpoints::build_url(paths::PURCHASE_GET_USER_PURCHASES_IDS);
        let mut query: Vec<(&str, String)> =
            vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(kind) = purchase_type {
            query.push(("type", kind.to_string()));
        }

        let http_response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&query)
            .send()
            .await?;
        log::debug!(
            "[Purchases] get_user_purchases_ids_page(type={:?}, limit={}, offset={}) status={}",
            purchase_type,
            limit,
            offset,
            http_response.status()
        );
        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// Get all purchases by paginating through the Qobuz purchases API.
    ///
    /// Paginates `"albums"` then `"tracks"` separately (`page_limit = 500`); per
    /// type reads `.total` on the first page, accumulates, and breaks when
    /// `got == 0` OR `offset + got >= total`. Final totals fall back to
    /// `items.len()` when the server total was 0. Returns offset=0, limit=500 on
    /// both pages. Ported from `src-tauri/src/api/client.rs:1612`.
    pub async fn get_user_purchases_all(&self) -> Result<PurchaseResponse> {
        let page_limit = 500u32;
        let mut all_albums: Vec<PurchaseAlbum> = Vec::new();
        let mut all_tracks: Vec<PurchaseTrack> = Vec::new();
        let mut albums_total = 0u32;
        let mut tracks_total = 0u32;

        let mut albums_offset = 0u32;
        loop {
            let page = self
                .get_user_purchases_page_typed(Some("albums"), page_limit, albums_offset)
                .await?;
            if albums_offset == 0 {
                albums_total = page.albums.total;
            }

            let got = page.albums.items.len() as u32;
            all_albums.extend(page.albums.items);

            if got == 0 || albums_offset + got >= albums_total {
                break;
            }
            albums_offset += got;
        }

        let mut tracks_offset = 0u32;
        loop {
            let page = self
                .get_user_purchases_page_typed(Some("tracks"), page_limit, tracks_offset)
                .await?;
            if tracks_offset == 0 {
                tracks_total = page.tracks.total;
            }

            let got = page.tracks.items.len() as u32;
            all_tracks.extend(page.tracks.items);

            if got == 0 || tracks_offset + got >= tracks_total {
                break;
            }
            tracks_offset += got;
        }

        let final_albums_total = if albums_total == 0 {
            all_albums.len() as u32
        } else {
            albums_total
        };
        let final_tracks_total = if tracks_total == 0 {
            all_tracks.len() as u32
        } else {
            tracks_total
        };

        Ok(PurchaseResponse {
            albums: SearchResultsPage {
                items: all_albums,
                total: final_albums_total,
                offset: 0,
                limit: page_limit,
            },
            tracks: SearchResultsPage {
                items: all_tracks,
                total: final_tracks_total,
                offset: 0,
                limit: page_limit,
            },
        })
    }

    /// Get all purchases for a single type by paginating through the Qobuz
    /// purchases API.
    ///
    /// Same loop as `get_user_purchases_all` but for ONE type; the OTHER type's
    /// `total` is forced to 0 in the returned envelope (the root of the per-type
    /// totals gotcha — the controller must call `get_user_purchases_ids_page_typed`
    /// separately per type to recover both totals). Unsupported type →
    /// `ApiError::ApiResponse("Unsupported purchase type: {}")`. Ported from
    /// `src-tauri/src/api/client.rs:1683`.
    pub async fn get_user_purchases_all_typed(
        &self,
        purchase_type: &str,
    ) -> Result<PurchaseResponse> {
        let page_limit = 500u32;
        let mut offset = 0u32;

        let mut all_albums: Vec<PurchaseAlbum> = Vec::new();
        let mut all_tracks: Vec<PurchaseTrack> = Vec::new();
        let mut total = 0u32;

        loop {
            let page = self
                .get_user_purchases_page_typed(Some(purchase_type), page_limit, offset)
                .await?;

            match purchase_type {
                "albums" => {
                    if offset == 0 {
                        total = page.albums.total;
                    }
                    let got = page.albums.items.len() as u32;
                    all_albums.extend(page.albums.items);
                    if got == 0 || offset + got >= total {
                        break;
                    }
                    offset += got;
                }
                "tracks" => {
                    if offset == 0 {
                        total = page.tracks.total;
                    }
                    let got = page.tracks.items.len() as u32;
                    all_tracks.extend(page.tracks.items);
                    if got == 0 || offset + got >= total {
                        break;
                    }
                    offset += got;
                }
                _ => {
                    return Err(ApiError::ApiResponse(format!(
                        "Unsupported purchase type: {}",
                        purchase_type
                    )));
                }
            }
        }

        let final_total = if total == 0 {
            if purchase_type == "albums" {
                all_albums.len() as u32
            } else {
                all_tracks.len() as u32
            }
        } else {
            total
        };

        Ok(PurchaseResponse {
            albums: SearchResultsPage {
                items: all_albums,
                total: if purchase_type == "albums" {
                    final_total
                } else {
                    0
                },
                offset: 0,
                limit: page_limit,
            },
            tracks: SearchResultsPage {
                items: all_tracks,
                total: if purchase_type == "tracks" {
                    final_total
                } else {
                    0
                },
                offset: 0,
                limit: page_limit,
            },
        })
    }

    /// Get a signed file URL for a specific `format_id`.
    ///
    /// RPC-SIGNED (the ONLY signed purchase call): the signature HARD-CODES
    /// `intentstream` (`sign_get_file_url`), and the query `intent` is likewise
    /// `"stream"` — purchases reuse the streaming intent even though the OpenAPI
    /// lists `"download"`. A raw-`format_id` entry point is required because the
    /// in-tree `get_stream_url` takes a `Quality` enum that can't express every
    /// purchase format id (27/7/6/5). Empty `url` → `ApiError::TrackUnavailable`,
    /// HTTP 400 → `ApiError::InvalidAppSecret`. Ported from
    /// `src-tauri/src/api/client.rs:1768`.
    pub async fn get_track_file_url_by_format(
        &self,
        track_id: u64,
        format_id: u32,
    ) -> Result<StreamUrl> {
        let url = endpoints::build_url(paths::TRACK_GET_FILE_URL);
        let timestamp = get_timestamp();
        let secret = self.secret().await?;
        let signature = sign_get_file_url(track_id, format_id, timestamp, &secret);

        let response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&[
                ("track_id", track_id.to_string()),
                ("format_id", format_id.to_string()),
                ("intent", "stream".to_string()),
                ("request_ts", timestamp.to_string()),
                ("request_sig", signature),
            ])
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let json: Value = response.json().await?;

                let restrictions: Vec<StreamRestriction> = json
                    .get("restrictions")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                let stream_url = json
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if stream_url.is_empty() {
                    return Err(ApiError::TrackUnavailable(track_id));
                }

                Ok(StreamUrl {
                    url: stream_url,
                    format_id: json.get("format_id").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    mime_type: json
                        .get("mime_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    sampling_rate: json
                        .get("sampling_rate")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                    bit_depth: json
                        .get("bit_depth")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32),
                    track_id,
                    restrictions,
                })
            }
            StatusCode::BAD_REQUEST => Err(ApiError::InvalidAppSecret),
            status => Err(ApiError::ApiResponse(format!(
                "Unexpected status: {}",
                status
            ))),
        }
    }

    /// Download raw audio bytes from a Qobuz CDN URL.
    ///
    /// Ported from `src-tauri/src/commands_v2/helpers.rs:332`, preserving the
    /// load-bearing behavior: HTTP/1.1-only (`http1_only` — Qobuz CDN sends
    /// RST_STREAM on large HTTP/2 downloads, causing "1 byte then EOF"), a 10s
    /// connect timeout, and crucially NO total request timeout (large hi-res
    /// downloads must not be capped). The body is streamed in chunks; on a stream
    /// error the full `.source()` cause chain is folded into the returned message.
    ///
    /// TLS divergence from the reference: the Tauri build called
    /// `.use_native_tls()` because its Cargo opts into both TLS stacks. This crate
    /// stays on rustls (no `native-tls` feature — same decision documented in
    /// `cmaf.rs::build_cdn_client`), so we omit `.use_native_tls()`. If Akamai/
    /// Qobuz's CDN ever surfaces a cert issue, adding the `native-tls` feature to
    /// `qbz-qobuz` is the escape hatch. This is NOT a download-function regression.
    ///
    /// The Tauri build wrapped this in the `CDN_STREAMING_ACTIVE` busy-wait gate
    /// to avoid concurrent streaming-vs-download CDN throttling. That counter is
    /// `src-tauri`-only — the Slint stack has no shared equivalent — so there is
    /// no collision protection here (documented limitation; see the module-level
    /// note). The error type is `String` to mirror the reference exactly.
    pub async fn download_audio(url: &str) -> std::result::Result<Vec<u8>, String> {
        use std::time::Duration;

        // Force HTTP/1.1 — Qobuz CDN sends RST_STREAM on large downloads over
        // HTTP/2, causing "1 byte then EOF". curl (HTTP/1.1) downloads the same
        // URLs successfully.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .http1_only()
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

        log::info!("[Purchases] Downloading audio...");

        let response = client
            .get(url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await
            .map_err(|e| format!("Failed to fetch audio: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }

        // Log response headers for CDN diagnostics (helps debug "1 byte EOF").
        {
            let headers = response.headers();
            let h_ce = headers
                .get("content-encoding")
                .map(|v| v.to_str().unwrap_or("?"));
            let h_te = headers
                .get("transfer-encoding")
                .map(|v| v.to_str().unwrap_or("?"));
            let h_conn = headers.get("connection").map(|v| v.to_str().unwrap_or("?"));
            let h_server = headers.get("server").map(|v| v.to_str().unwrap_or("?"));
            let h_ct = headers
                .get("content-type")
                .map(|v| v.to_str().unwrap_or("?"));
            let h_via = headers.get("via").map(|v| v.to_str().unwrap_or("?"));
            log::info!(
                "[Purchases] CDN response: status={}, content-encoding={:?}, transfer-encoding={:?}, connection={:?}, server={:?}, content-type={:?}, via={:?}, version={:?}",
                response.status(),
                h_ce,
                h_te,
                h_conn,
                h_server,
                h_ct,
                h_via,
                response.version()
            );
        }

        let content_length = response.content_length();
        if let Some(len) = content_length {
            log::info!("[Purchases] Downloading audio: {} bytes expected", len);
        }

        // Stream body in chunks to handle partial reads gracefully.
        let expected_len = content_length.unwrap_or(0) as usize;
        let mut all_data = Vec::with_capacity(expected_len);
        let mut stream = response.bytes_stream();

        use futures_util::StreamExt;
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => all_data.extend_from_slice(&chunk),
                Err(e) => {
                    use std::error::Error as _;
                    let mut msg = format!("Failed to read audio bytes: {}", e);
                    let mut source = e.source();
                    while let Some(cause) = source {
                        msg.push_str(&format!(" | caused by: {}", cause));
                        source = cause.source();
                    }
                    // If we got some data but not all, log what we received.
                    if !all_data.is_empty() {
                        log::error!(
                            "[Purchases] Download error after {}/{} bytes: {}",
                            all_data.len(),
                            expected_len,
                            msg
                        );
                    } else {
                        log::error!("[Purchases] Download error (0 bytes received): {}", msg);
                    }
                    return Err(msg);
                }
            }
        }

        if expected_len > 0 && all_data.len() != expected_len {
            log::warn!(
                "[Purchases] Download size mismatch: got {} bytes, expected {}",
                all_data.len(),
                expected_len
            );
        }

        log::info!("[Purchases] Downloaded {} bytes", all_data.len());
        Ok(all_data)
    }
}
