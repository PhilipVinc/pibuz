//! Qobuz API client implementation

use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::auth::{
    get_timestamp, parse_login_response, sign_file_url, sign_get_file_url, sign_request,
    sign_session_start,
};
use super::bundle::{self, BundleTokens};
use super::endpoints::{self, paths};
use super::error::{ApiError, Result};
use super::forbidden_breaker::ForbiddenBreaker;
use qbz_models::*;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0";

/// Read a short, log-safe preview of a response body — for diagnosing an
/// unexpected non-2xx (e.g. distinguishing an edge/WAF HTML 403 from the API's
/// JSON error envelope, issue #637). Bounded so a large/HTML body can't bloat
/// the log; prefixed with " : " so it reads well appended to an error message.
async fn body_preview(response: reqwest::Response) -> String {
    match response.text().await {
        Ok(body) => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                " : <empty body>".to_string()
            } else {
                let preview: String = trimmed.chars().take(200).collect();
                format!(" : {preview}")
            }
        }
        Err(_) => String::new(),
    }
}

/// CMAF session state (session/start + infos for key derivation)
struct CmafSession {
    session_id: String,
    infos: String,
    expires_at: u64,
}

/// Qobuz API client
pub struct QobuzClient {
    http: Client,
    tokens: Arc<RwLock<Option<BundleTokens>>>,
    session: Arc<RwLock<Option<UserSession>>>,
    /// Bearer credential for account-less operation (a Qobuz Connect pairing
    /// handoff's `jwt_api`). Used ONLY when no user session exists: everywhere
    /// a request would carry `X-User-Auth-Token`, it carries
    /// `Authorization: Bearer <jwt>` instead.
    bearer_api_jwt: Arc<RwLock<Option<String>>>,
    validated_secret: Arc<RwLock<Option<String>>>,
    locale: Arc<RwLock<String>>,
    cmaf_session: Arc<RwLock<Option<CmafSession>>>,
    /// Backs off the hot streaming/favorites paths after repeated 403s so a
    /// post-outage account hiccup can't be escalated into a per-IP edge block
    /// by the no-backoff prefetch scheduler (issue #637).
    forbidden_breaker: Arc<ForbiddenBreaker>,
}

impl Clone for QobuzClient {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            tokens: Arc::clone(&self.tokens),
            session: Arc::clone(&self.session),
            bearer_api_jwt: Arc::clone(&self.bearer_api_jwt),
            validated_secret: Arc::clone(&self.validated_secret),
            locale: Arc::clone(&self.locale),
            cmaf_session: Arc::clone(&self.cmaf_session),
            forbidden_breaker: Arc::clone(&self.forbidden_breaker),
        }
    }
}

impl QobuzClient {
    /// Create a new client
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .cookie_store(true)
            // Bound the TCP connect phase so a dead route (e.g. a stale CDN
            // address) can't hang startup. Does not affect body-read time, so
            // long streaming reads are unaffected.
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;

        Ok(Self {
            http,
            tokens: Arc::new(RwLock::new(None)),
            session: Arc::new(RwLock::new(None)),
            bearer_api_jwt: Arc::new(RwLock::new(None)),
            validated_secret: Arc::new(RwLock::new(None)),
            locale: Arc::new(RwLock::new("en".to_string())),
            cmaf_session: Arc::new(RwLock::new(None)),
            forbidden_breaker: Arc::new(ForbiddenBreaker::new()),
        })
    }

    // === 403 circuit breaker (issue #637) ===

    /// Short-circuit an authenticated request when the 403 breaker is open.
    /// Returns `Err(ForbiddenCircuitOpen)` — no network is touched — so a
    /// post-outage 403 storm cannot get the user's IP edge-blocked. Callers put
    /// this at the top of the hot streaming/favorites paths.
    fn forbidden_guard(&self) -> Result<()> {
        // Test hook: `QBZ_FORCE_403=1` forces the breaker open so the whole 403
        // back-off path (no-network short-circuit + abort-fallback + the audible
        // "backing off" toast) can be smoke-tested on a HEALTHY account, which
        // otherwise can't reproduce the incident. Off by default; issue #637.
        if std::env::var_os("QBZ_FORCE_403").is_some() {
            return Err(ApiError::ForbiddenCircuitOpen(30));
        }
        if let Some(remaining) = self.forbidden_breaker.blocked_for() {
            return Err(ApiError::ForbiddenCircuitOpen(remaining.as_secs()));
        }
        Ok(())
    }

    /// Feed an authenticated response's status to the breaker: a 403 counts
    /// toward opening it; any success resets it. Other statuses are neutral
    /// (they have their own handling and must not open the breaker).
    fn note_forbidden_status(&self, status: StatusCode) {
        if status == StatusCode::FORBIDDEN {
            if let Some(cooldown) = self.forbidden_breaker.record_forbidden() {
                log::warn!(
                    "[403-breaker] Repeated 403s from Qobuz — backing off for {}s (no network) \
                     to avoid an edge/IP block. See issue #637.",
                    cooldown.as_secs()
                );
            }
        } else if status.is_success() {
            self.forbidden_breaker.record_success();
        }
    }

    /// Initialize client by extracting bundle tokens.
    ///
    /// Warm start: if cached tokens exist, use them immediately so the UI never
    /// blocks on Qobuz's (sometimes very slow) ~7 MB bundle download, then
    /// refresh in the background — re-downloading only if Qobuz rotated the
    /// bundle version. Cold start (first run or after a cache wipe): fetch now,
    /// bounded by a per-request timeout + a small retry so a slow/dead CDN can't
    /// hang forever.
    ///
    /// Returns `true` if it served cached tokens (warm), `false` if it had to do
    /// a live extraction (cold) — callers can use this to drive a "connecting"
    /// UI only when it actually matters.
    pub async fn init(&self) -> Result<bool> {
        if let Some(cached) = bundle::load_cached_bundle() {
            let version = cached.bundle_version.clone();
            log::info!("[Bundle] Using cached tokens (version {})", version);
            *self.tokens.write().await = Some(cached.into());

            // Cache reads are never gated, but the background refresh is a
            // network request — gate it once before cloning the client into
            // the spawned task, skipping the refresh instead of failing the
            // warm start.
            match self.http() {
                Ok(client) => {
                    let client = client.clone();
                    let tokens_arc = Arc::clone(&self.tokens);
                    tokio::spawn(async move {
                        if let Some(fresh) =
                            bundle::refresh_bundle_if_changed(&client, &version).await
                        {
                            *tokens_arc.write().await = Some(fresh);
                            log::info!("[Bundle] Background refresh applied rotated tokens");
                        }
                    });
                }
                Err(_) => {
                    log::info!("[Bundle] Offline mode - skipping background bundle refresh");
                }
            }
            return Ok(true);
        }

        log::info!("[Bundle] No cached tokens, extracting from Qobuz...");
        // Cold start: a live bundle fetch is a network request — gated on
        // purpose so an offline cold start fails fast instead of waiting out
        // the network timeouts.
        let tokens = bundle::extract_and_cache_bundle_tokens(self.http()?).await?;
        *self.tokens.write().await = Some(tokens);
        Ok(false)
    }

    /// Get the current locale (internal use)
    async fn locale(&self) -> String {
        self.locale.read().await.clone()
    }

    /// Get app ID (public for catalog search)
    pub async fn app_id(&self) -> Result<String> {
        self.tokens
            .read()
            .await
            .as_ref()
            .map(|t| t.app_id.clone())
            .ok_or_else(|| ApiError::BundleExtractionError("Client not initialized".to_string()))
    }

    /// Get HTTP client reference (public for catalog search)
    pub fn get_http(&self) -> &Client {
        &self.http
    }

    /// The single offline choke point (D3): every Qobuz SERVICE request flows
    /// through here. While offline mode is active, fail fast with a typed,
    /// non-transient error instead of timing out against the network.
    ///
    pub(crate) fn http(&self) -> Result<&Client> {
        Ok(&self.http)
    }

    /// Get validated secret (validates on first use)
    pub(crate) async fn secret(&self) -> Result<String> {
        // Check if we already have a validated secret
        if let Some(secret) = self.validated_secret.read().await.clone() {
            return Ok(secret);
        }

        // Need to validate secrets
        let tokens = self.tokens.read().await;
        let tokens = tokens
            .as_ref()
            .ok_or_else(|| ApiError::BundleExtractionError("Client not initialized".to_string()))?;

        for secret in &tokens.secrets {
            if self.test_secret(secret).await? {
                *self.validated_secret.write().await = Some(secret.clone());
                return Ok(secret.clone());
            }
        }

        Err(ApiError::InvalidAppSecret)
    }

    /// Test if a secret is valid using a known track
    async fn test_secret(&self, secret: &str) -> Result<bool> {
        let test_track_id = 5966783u64; // Known test track
        let timestamp = get_timestamp();
        let signature = sign_get_file_url(test_track_id, 5, timestamp, secret);

        let url = endpoints::build_url(paths::TRACK_GET_FILE_URL);
        let response = self
            .http()?
            .get(&url)
            .headers(self.api_headers().await?)
            .query(&[
                ("track_id", test_track_id.to_string()),
                ("format_id", "5".to_string()),
                ("intent", "stream".to_string()),
                ("request_ts", timestamp.to_string()),
                ("request_sig", signature),
            ])
            .send()
            .await?;

        Ok(response.status() != StatusCode::BAD_REQUEST)
    }

    /// Login with email and password
    pub async fn login(&self, email: &str, password: &str) -> Result<UserSession> {
        let url = endpoints::build_url(paths::USER_LOGIN);
        // Auth exemption: raw client, bypasses the offline gate (sign-in is
        // explicit user intent to reach Qobuz; the gate governs services).
        let response = self
            .http
            .get(&url)
            .headers(self.app_id_headers().await?)
            .query(&[("email", email), ("password", password)])
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let json: Value = response.json().await?;
                let session = parse_login_response(&json)?;
                *self.session.write().await = Some(session.clone());
                Ok(session)
            }
            StatusCode::UNAUTHORIZED => Err(ApiError::AuthenticationError(
                "Invalid credentials".to_string(),
            )),
            StatusCode::BAD_REQUEST => Err(ApiError::InvalidAppId),
            status => Err(ApiError::ApiResponse(format!(
                "Unexpected status: {}",
                status
            ))),
        }
    }

    /// Check if logged in
    pub async fn is_logged_in(&self) -> bool {
        self.session.read().await.is_some()
    }

    /// Get user auth token header value (public for catalog search)
    pub async fn auth_token(&self) -> Result<String> {
        self.session
            .read()
            .await
            .as_ref()
            .map(|s| s.user_auth_token.clone())
            .ok_or_else(|| ApiError::AuthenticationError("Not logged in".to_string()))
    }

    // === Header helpers ===

    /// Set (or clear) the Bearer credential for account-less operation. The
    /// user session, when present, always outranks it.
    pub async fn set_bearer_api_token(&self, jwt: Option<String>) {
        *self.bearer_api_jwt.write().await = jwt;
    }

    /// X-App-Id only — for sign-in requests, which must never carry a stray
    /// credential (in particular not the pairing Bearer token, which belongs
    /// to whoever cast to this device, not to the account logging in).
    async fn app_id_headers(&self) -> Result<reqwest::header::HeaderMap> {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        let app_id = self.app_id().await?;
        headers.insert(
            "X-App-Id",
            HeaderValue::from_str(&app_id).map_err(|_| ApiError::InvalidAppId)?,
        );
        Ok(headers)
    }

    /// Build standard API headers.
    /// Always includes X-App-Id. Includes X-User-Auth-Token when logged in,
    /// else `Authorization: Bearer` when a pairing credential is installed.
    async fn api_headers(&self) -> Result<reqwest::header::HeaderMap> {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();

        let app_id = self.app_id().await?;
        headers.insert(
            "X-App-Id",
            HeaderValue::from_str(&app_id).map_err(|_| ApiError::InvalidAppId)?,
        );

        if let Ok(token) = self.auth_token().await {
            if let Ok(val) = HeaderValue::from_str(&token) {
                headers.insert("X-User-Auth-Token", val);
            }
        } else if let Some(jwt) = self.bearer_api_jwt.read().await.as_deref() {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {jwt}")) {
                headers.insert("Authorization", val);
            }
        }

        Ok(headers)
    }

    /// Build headers that REQUIRE a credential: the user session's
    /// X-User-Auth-Token, or the pairing Bearer token when not logged in.
    /// Fails when neither exists.
    pub(crate) async fn authenticated_headers(&self) -> Result<reqwest::header::HeaderMap> {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();

        let app_id = self.app_id().await?;
        headers.insert(
            "X-App-Id",
            HeaderValue::from_str(&app_id).map_err(|_| ApiError::InvalidAppId)?,
        );

        match self.auth_token().await {
            Ok(token) => {
                headers.insert(
                    "X-User-Auth-Token",
                    HeaderValue::from_str(&token).map_err(|_| {
                        ApiError::AuthenticationError("Invalid auth token format".into())
                    })?,
                );
            }
            Err(err) => {
                let jwt = self.bearer_api_jwt.read().await.clone().ok_or(err)?;
                headers.insert(
                    "Authorization",
                    HeaderValue::from_str(&format!("Bearer {jwt}")).map_err(|_| {
                        ApiError::AuthenticationError("Invalid bearer token format".into())
                    })?,
                );
            }
        }

        Ok(headers)
    }

    /// Build a signed GET request. Computes request_sig from the endpoint method name
    /// and query params, then appends request_ts + request_sig to the query.
    /// `method_name` is the endpoint path without slashes, e.g. "albumget".
    async fn signed_get(
        &self,
        url: &str,
        method_name: &str,
        params: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let timestamp = get_timestamp();
        let secret = self.secret().await?;
        let kv: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let sig = sign_request(method_name, &kv, timestamp, &secret);
        let ts_str = timestamp.to_string();

        let mut query_params: Vec<(&str, &str)> = kv;
        query_params.push(("request_ts", &ts_str));
        query_params.push(("request_sig", &sig));

        let response = self
            .http()?
            .get(url)
            .headers(self.api_headers().await?)
            .query(&query_params)
            .send()
            .await?;
        Ok(response)
    }

    // === Search endpoints ===

    /// Get an artist's tracks (public endpoint via artist/get?extra=tracks)
    pub async fn get_artist_tracks(
        &self,
        artist_id: u64,
        limit: u32,
        offset: u32,
    ) -> Result<TracksContainer> {
        let url = endpoints::build_url(paths::ARTIST_GET);
        let locale = self.locale().await;

        let http_response = self
            .signed_get(
                &url,
                "artistget",
                &[
                    ("artist_id", artist_id.to_string()),
                    ("extra", "tracks".to_string()),
                    ("lang", locale),
                    ("limit", limit.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        log::debug!(
            "[API] get_artist_tracks({}) status={}",
            artist_id,
            http_response.status()
        );
        let response: Value = http_response.json().await?;

        let tracks = response
            .get("tracks")
            .ok_or_else(|| ApiError::ApiResponse("No tracks in artist response".to_string()))?;

        Ok(serde_json::from_value(tracks.clone())?)
    }

    // === Get endpoints ===

    /// Get album by ID
    pub async fn get_album(&self, album_id: &str) -> Result<Album> {
        let url = endpoints::build_url(paths::ALBUM_GET);
        let http_response = self
            .signed_get(&url, "albumget", &[("album_id", album_id.to_string())])
            .await?;
        let status = http_response.status();
        log::debug!("[API] get_album({}) status={}", album_id, status);

        if status == StatusCode::NOT_FOUND {
            log::warn!(
                "[API] get_album({}) returned 404 — album not found",
                album_id
            );
            return Err(ApiError::ApiResponse(format!(
                "Album {} not found (404)",
                album_id
            )));
        }
        if !status.is_success() {
            log::error!("[API] get_album({}) unexpected status={}", album_id, status);
            return Err(ApiError::ApiResponse(format!(
                "get_album({}) status {}",
                album_id, status
            )));
        }

        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    /// Like [`get_dynamic_suggest`] but carrying the `track_to_analysed`
    /// payload — the PRIMARY DailyQ/WeeklyQ path. Tauri seeds this with up to 9
    /// resolved `{track_id, artist_id, genre_id, label_id}` tuples and only
    /// falls back to an empty analysis when a call returns zero items.
    pub async fn get_dynamic_suggest_full(
        &self,
        listened_track_ids: &[u64],
        tracks_to_analyse: &[TrackToAnalyse],
        limit: u32,
    ) -> Result<Vec<Track>> {
        let url = endpoints::build_url(paths::DYNAMIC_SUGGEST);
        let body = serde_json::json!({
            "limit": limit,
            "listened_tracks_ids": listened_track_ids,
            "track_to_analysed": tracks_to_analyse,
        });
        let http_response = self
            .http()?
            .post(&url)
            .headers(self.authenticated_headers().await?)
            .json(&body)
            .send()
            .await?;
        let status = http_response.status();
        if !status.is_success() {
            return Err(ApiError::ApiResponse(format!(
                "get_dynamic_suggest status {status}"
            )));
        }
        let response: Value = http_response.json().await?;
        Ok(qbz_models::lenient::parse_items_array(
            &response,
            "tracks",
            "dynamic-suggest track",
        ))
    }

    /// Get track by ID
    pub async fn get_track(&self, track_id: u64) -> Result<Track> {
        let url = endpoints::build_url(paths::TRACK_GET);
        let http_response = self
            .signed_get(&url, "trackget", &[("track_id", track_id.to_string())])
            .await?;
        let status = http_response.status();
        log::debug!("[API] get_track({}) status={}", track_id, status);

        if status == StatusCode::NOT_FOUND {
            log::warn!(
                "[API] get_track({}) returned 404 — track no longer available",
                track_id
            );
            return Err(ApiError::TrackUnavailable(track_id));
        }
        if !status.is_success() {
            log::error!("[API] get_track({}) unexpected status={}", track_id, status);
            return Err(ApiError::ApiResponse(format!(
                "get_track({}) status {}",
                track_id, status
            )));
        }

        let response: Value = http_response.json().await?;
        Ok(serde_json::from_value(response)?)
    }

    // === Lyrics (v9.9.0.0-beta delta) ===

    /// Fetch full Track objects for a batch of track IDs.
    /// Uses the `track/getList` endpoint, which caps at 50 IDs per call,
    /// so larger inputs are split into 50-ID windows and fetched serially.
    /// Input order is preserved in the returned vector.
    pub async fn get_tracks_batch(&self, track_ids: &[u64]) -> Result<Vec<Track>> {
        const MAX_PER_CALL: usize = 50;

        if track_ids.is_empty() {
            return Ok(Vec::new());
        }

        if track_ids.len() <= MAX_PER_CALL {
            return self.get_tracks_batch_chunk(track_ids).await;
        }

        log::debug!(
            "[API] get_tracks_batch chunking {} IDs into {}-windows",
            track_ids.len(),
            MAX_PER_CALL
        );
        let mut all = Vec::with_capacity(track_ids.len());
        for chunk in track_ids.chunks(MAX_PER_CALL) {
            let mut tracks = self.get_tracks_batch_chunk(chunk).await?;
            all.append(&mut tracks);
        }
        Ok(all)
    }

    /// Single `track/getList` POST. Caller is responsible for keeping
    /// `track_ids.len() <= 50` — `get_tracks_batch` handles that.
    async fn get_tracks_batch_chunk(&self, track_ids: &[u64]) -> Result<Vec<Track>> {
        let url = endpoints::build_url(paths::TRACK_GET_LIST);
        let headers = self.api_headers().await?;
        let timestamp = get_timestamp();
        let secret = self.secret().await?;
        let ids_str: String = track_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sig = sign_request(
            "trackgetList",
            &[("tracks_id", &ids_str)],
            timestamp,
            &secret,
        );

        let body = serde_json::json!({ "tracks_id": track_ids });
        log::debug!("[API] get_tracks_batch POST ({} IDs)", track_ids.len());

        let http_response = self
            .http()?
            .post(&url)
            .headers(headers)
            .query(&[("request_ts", timestamp.to_string()), ("request_sig", sig)])
            .json(&body)
            .send()
            .await?;

        let status = http_response.status();
        log::debug!("[API] get_tracks_batch POST status={}", status);

        let value: Value = http_response.json().await?;

        // Response: { "tracks": { "total": N, "items": [...] } }
        let items = value
            .get("tracks")
            .and_then(|t| t.get("items"))
            .ok_or_else(|| {
                let preview = serde_json::to_string(&value)
                    .unwrap_or_default()
                    .chars()
                    .take(500)
                    .collect::<String>();
                ApiError::ApiResponse(format!(
                    "Missing tracks.items in getList response: {}",
                    preview
                ))
            })?;

        let tracks: Vec<Track> = serde_json::from_value(items.clone())?;
        log::debug!("[API] get_tracks_batch returned {} tracks", tracks.len());
        Ok(tracks)
    }

    /// Get playlist by ID (paginates automatically to fetch all tracks)
    ///
    /// After the first page, remaining pages are fetched concurrently
    /// since we know the total track count from the first response.
    pub async fn get_playlist(&self, playlist_id: u64) -> Result<Playlist> {
        let url = endpoints::build_url(paths::PLAYLIST_GET);
        const PAGE_SIZE: u32 = 500;

        let start = std::time::Instant::now();

        // First page — gives us metadata + total track count
        let http_response = self
            .signed_get(
                &url,
                "playlistget",
                &[
                    ("playlist_id", playlist_id.to_string()),
                    ("limit", PAGE_SIZE.to_string()),
                    ("offset", "0".to_string()),
                    ("extra", "tracks".to_string()),
                ],
            )
            .await?;
        log::debug!(
            "[API] get_playlist({}) status={}",
            playlist_id,
            http_response.status()
        );
        let response: Value = http_response.json().await?;
        let mut playlist: Playlist = serde_json::from_value(response)?;

        // Fetch remaining pages concurrently
        if let Some(ref mut container) = playlist.tracks {
            let total = container.total;
            let fetched = container.items.len() as u32;

            if fetched < total {
                // Build all remaining page offsets
                let offsets: Vec<u32> = (fetched..total).step_by(PAGE_SIZE as usize).collect();
                log::debug!(
                    "[API] get_playlist({}) fetching {} remaining pages concurrently ({}/{})",
                    playlist_id,
                    offsets.len(),
                    fetched,
                    total
                );

                // Prepare headers and per-page signatures for concurrent requests
                let headers = self.api_headers().await?;
                let secret = self.secret().await.unwrap_or_default();

                // Offline gate checked ONCE for the whole page batch (the
                // first page above already passed through it) — not inside
                // the per-page loop.
                let gated_http = self.http()?;

                // Launch all page requests concurrently
                let futures: Vec<_> = offsets
                    .iter()
                    .map(|&offset| {
                        let http = gated_http;
                        let url = &url;
                        let headers = headers.clone();
                        let pid = playlist_id.to_string();
                        let limit = PAGE_SIZE.to_string();
                        let offset_str = offset.to_string();
                        let ts = get_timestamp();
                        let sig = sign_request(
                            "playlistget",
                            &[
                                ("extra", "tracks"),
                                ("limit", &limit),
                                ("offset", &offset_str),
                                ("playlist_id", &pid),
                            ],
                            ts,
                            &secret,
                        );
                        let ts_str = ts.to_string();
                        async move {
                            let resp = http
                                .get(url)
                                .headers(headers)
                                .query(&[
                                    ("playlist_id", pid.as_str()),
                                    ("limit", limit.as_str()),
                                    ("offset", offset_str.as_str()),
                                    ("extra", "tracks"),
                                    ("request_ts", ts_str.as_str()),
                                    ("request_sig", sig.as_str()),
                                ])
                                .send()
                                .await?;
                            let value: Value = resp.json().await?;
                            let page: Playlist = serde_json::from_value(value)?;
                            Ok::<_, anyhow::Error>((offset, page))
                        }
                    })
                    .collect();

                let results = futures_util::future::join_all(futures).await;

                // Collect results sorted by offset to maintain track order
                let mut pages: Vec<(u32, Playlist)> = Vec::new();
                for result in results {
                    match result {
                        Ok(page) => pages.push(page),
                        Err(e) => {
                            log::warn!(
                                "[API] get_playlist({}) page fetch failed: {}",
                                playlist_id,
                                e
                            );
                            // Continue with what we have
                        }
                    }
                }
                pages.sort_by_key(|(offset, _)| *offset);

                // Append tracks in order
                for (_, page_playlist) in pages {
                    if let Some(page_tracks) = page_playlist.tracks {
                        if !page_tracks.items.is_empty() {
                            container.items.extend(page_tracks.items);
                        }
                    }
                }
            }
        }

        let elapsed = start.elapsed();
        log::debug!(
            "[API] get_playlist({}) complete: {} tracks in {:.2}s",
            playlist_id,
            playlist.tracks.as_ref().map(|t| t.items.len()).unwrap_or(0),
            elapsed.as_secs_f64()
        );

        Ok(playlist)
    }

    // === Authenticated endpoints ===

    /// Get stream URL for a track (requires auth + signature)
    pub async fn get_stream_url(&self, track_id: u64, quality: Quality) -> Result<StreamUrl> {
        // Back off before the network if the 403 breaker is open (issue #637).
        self.forbidden_guard()?;
        log::info!(
            "Getting stream URL for track {} with quality {:?}",
            track_id,
            quality
        );
        let url = endpoints::build_url(paths::TRACK_GET_FILE_URL);
        let timestamp = get_timestamp();
        log::debug!("Getting secret for signing...");
        let secret = self.secret().await?;
        log::debug!("Secret obtained, signing request...");
        let signature = sign_get_file_url(track_id, quality.id(), timestamp, &secret);

        log::debug!("Sending stream URL request...");
        let response = self
            .http()?
            .get(&url)
            .headers(self.authenticated_headers().await?)
            .query(&[
                ("track_id", track_id.to_string()),
                ("format_id", quality.id().to_string()),
                ("intent", "stream".to_string()),
                ("request_ts", timestamp.to_string()),
                ("request_sig", signature),
            ])
            .send()
            .await?;

        let status = response.status();
        log::info!("Stream URL response status: {}", status);
        // Feed the breaker: a 403 counts toward opening it; a 200 resets it.
        self.note_forbidden_status(status);
        match status {
            StatusCode::OK => {
                let json: Value = response.json().await?;
                log::debug!(
                    "Stream URL response JSON keys: {:?}",
                    json.as_object().map(|o| o.keys().collect::<Vec<_>>())
                );

                // Check for restrictions
                let restrictions: Vec<StreamRestriction> = json
                    .get("restrictions")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                // Validate that we got an actual URL (track may be unavailable)
                let url = json["url"].as_str().unwrap_or("").to_string();
                if url.is_empty() {
                    // Log the restriction codes for debugging
                    let restriction_codes: Vec<&str> =
                        restrictions.iter().map(|r| r.code.as_str()).collect();
                    log::warn!(
                        "Stream URL missing for track {} - restrictions: {:?}",
                        track_id,
                        restriction_codes
                    );
                    return Err(ApiError::TrackUnavailable(track_id));
                }

                Ok(StreamUrl {
                    url,
                    format_id: json["format_id"].as_u64().unwrap_or(0) as u32,
                    mime_type: json["mime_type"].as_str().unwrap_or("").to_string(),
                    sampling_rate: json["sampling_rate"].as_f64().unwrap_or(0.0),
                    bit_depth: json["bit_depth"].as_u64().map(|v| v as u32),
                    track_id,
                    restrictions,
                })
            }
            StatusCode::BAD_REQUEST => Err(ApiError::InvalidAppSecret),
            StatusCode::UNAUTHORIZED => Err(ApiError::AuthenticationError(
                "stream URL 401 — user auth token invalid or expired".to_string(),
            )),
            StatusCode::FORBIDDEN => {
                let preview = body_preview(response).await;
                log::warn!("Stream URL 403 for track {}{}", track_id, preview);
                Err(ApiError::Forbidden(preview))
            }
            status => Err(ApiError::ApiResponse(format!(
                "Unexpected status: {}",
                status
            ))),
        }
    }

    /// Get stream URL with quality fallback
    pub async fn get_stream_url_with_fallback(
        &self,
        track_id: u64,
        preferred: Quality,
    ) -> Result<StreamUrl> {
        log::info!(
            "Getting stream URL with fallback for track {}, preferred quality: {:?}",
            track_id,
            preferred
        );
        let qualities = Quality::fallback_order();
        let start_idx = qualities.iter().position(|q| *q == preferred).unwrap_or(0);

        let mut track_unavailable = false;

        for quality in &qualities[start_idx..] {
            log::info!("Trying quality: {:?}", quality);
            match self.get_stream_url(track_id, *quality).await {
                Ok(url) if !url.has_restrictions() => {
                    log::info!(
                        "Got stream URL for requested quality format_id={}",
                        quality.id()
                    );
                    return Ok(url);
                }
                Ok(_) => {
                    log::info!("Quality {:?} has restrictions, trying next", quality);
                    continue;
                }
                Err(ApiError::InvalidAppSecret) => {
                    log::error!("Invalid app secret");
                    return Err(ApiError::InvalidAppSecret);
                }
                // A 403 (or an open breaker, or a 401) is NOT a per-quality
                // restriction — every quality would 403 the same way. Abort the
                // whole fallback loop immediately instead of firing 5 more
                // requests per track and feeding the storm (issue #637).
                Err(
                    e @ (ApiError::Forbidden(_)
                    | ApiError::ForbiddenCircuitOpen(_)
                    | ApiError::AuthenticationError(_)),
                ) => {
                    log::warn!("Stream URL aborting quality fallback: {}", e);
                    return Err(e);
                }
                Err(ApiError::TrackUnavailable(_)) => {
                    // Track is completely unavailable on Qobuz
                    track_unavailable = true;
                    continue;
                }
                Err(e) => {
                    log::warn!("Quality {:?} failed: {}, trying next", quality, e);
                    continue;
                }
            }
        }

        // If all quality levels reported track unavailable, return that specific error
        if track_unavailable {
            log::error!("Track {} is no longer available on Qobuz", track_id);
            return Err(ApiError::TrackUnavailable(track_id));
        }

        log::error!("No quality available for track {}", track_id);
        Err(ApiError::NoQualityAvailable)
    }

    // ============ Artist Page Endpoints ============

    // === CMAF streaming endpoints ===

    /// Ensure we have a valid CMAF session, renewing if expired.
    /// Returns `(session_id, infos)` for use with file/url and key derivation.
    ///
    /// Concurrency note: this method serializes concurrent session
    /// renewals. Without that, two overlapping callers could both see
    /// "no session" on the read side, each POST /session/start, each get
    /// DIFFERENT `infos`, and the second one to finish would overwrite
    /// the first in the cache. Any `get_file_url` response whose wrapped
    /// key was tied to the first session then unwrapped with the second
    /// session's key and blew up with AES-CBC "Unpad Error" — which
    /// manifested as prefetch CMAF failures + downloaded-but-gappy
    /// transitions between offline tracks.
    ///
    /// Fix: a double-checked lock pattern on the write guard. Fast path
    /// uses a read guard; slow path acquires the write guard, re-checks
    /// under exclusive ownership, and only one caller hits the network.
    pub async fn ensure_cmaf_session(&self) -> Result<(String, String)> {
        let now = get_timestamp();

        // Fast path: existing session with > 60s left.
        {
            let guard = self.cmaf_session.read().await;
            if let Some(ref cs) = *guard {
                if cs.expires_at > now + 60 {
                    return Ok((cs.session_id.clone(), cs.infos.clone()));
                }
            }
        }

        // Slow path: take the write lock and re-check. Concurrent callers
        // end up here one at a time; after the first finishes POST
        // session/start, the rest find the freshly-populated cache and
        // return without hitting the network.
        let mut guard = self.cmaf_session.write().await;
        if let Some(ref cs) = *guard {
            if cs.expires_at > now + 60 {
                return Ok((cs.session_id.clone(), cs.infos.clone()));
            }
        }

        // We're the one task that actually starts a session. Back off before
        // the network if the 403 breaker is open (issue #637) — a cached
        // session above is still served; only the network POST is gated.
        self.forbidden_guard()?;
        log::info!("[CMAF] Starting new session");
        let timestamp = get_timestamp();
        let sig = sign_session_start(timestamp);

        let url = endpoints::build_url(paths::SESSION_START);
        let response = self
            .http()?
            .post(&url)
            .headers(self.authenticated_headers().await?)
            .form(&[
                ("profile", "qbz-1"),
                ("request_ts", &timestamp.to_string()),
                ("request_sig", &sig),
            ])
            .send()
            .await?;

        let status = response.status();
        // Feed the breaker: a 403 here counts toward opening it; success resets.
        self.note_forbidden_status(status);
        if !status.is_success() {
            if status == StatusCode::FORBIDDEN {
                let preview = body_preview(response).await;
                log::warn!("[CMAF] session/start 403{}", preview);
                return Err(ApiError::Forbidden(preview));
            }
            return Err(ApiError::ApiResponse(format!(
                "session/start failed with status {}",
                status
            )));
        }

        let resp: SessionStartResponse = response.json().await?;
        let infos = resp.infos.unwrap_or_default();
        log::info!(
            "[CMAF] Session started: id={}..., expires_at={}",
            &resp.session_id[..resp.session_id.len().min(8)],
            resp.expires_at
        );

        let session_id = resp.session_id.clone();
        let infos_clone = infos.clone();

        *guard = Some(CmafSession {
            session_id: resp.session_id,
            infos,
            expires_at: resp.expires_at,
        });

        Ok((session_id, infos_clone))
    }

    /// Get CMAF segmented file URL for a track.
    ///
    /// This is the new streaming endpoint that returns encrypted CMAF segments
    /// instead of a direct file URL.
    pub async fn get_file_url(&self, track_id: u64, quality: Quality) -> Result<TrackFileUrl> {
        let format_id = quality.id();
        let url = endpoints::build_url(paths::FILE_URL);

        // Back off before the network if the 403 breaker is open (issue #637).
        self.forbidden_guard()?;

        // Retry transient failures (5xx / 429 / network blips) with backoff so
        // a momentary hiccup on the next track's file/url is not turned into a
        // queue skip. A real 404 → TrackUnavailable is terminal and returns
        // immediately to the (bounded) skip path. Issue #467.
        crate::retry::retry_transient(
            crate::retry::DEFAULT_MAX_ATTEMPTS,
            "CMAF file/url",
            ApiError::is_transient,
            |_attempt| {
                let url = url.clone();
                async move {
                    let (session_id, _infos) = self.ensure_cmaf_session().await?;

                    // Fresh timestamp + signature per attempt — the request
                    // signature is time-bound and would expire across retries.
                    let timestamp = get_timestamp();
                    let sig = sign_file_url(track_id, format_id, timestamp);

                    let mut headers = self.authenticated_headers().await?;
                    headers.insert(
                        "X-Session-Id",
                        reqwest::header::HeaderValue::from_str(&session_id).map_err(|_| {
                            ApiError::ApiResponse("Invalid session ID format".into())
                        })?,
                    );

                    let response = self
                        .http()?
                        .get(&url)
                        .headers(headers)
                        .query(&[
                            ("track_id", track_id.to_string()),
                            ("format_id", format_id.to_string()),
                            ("intent", "stream".to_string()),
                            ("request_ts", timestamp.to_string()),
                            ("request_sig", sig),
                        ])
                        .send()
                        .await?;

                    let status = response.status();
                    log::info!(
                        "[CMAF] file/url track_id={} format_id={} status={}",
                        track_id,
                        format_id,
                        status
                    );
                    // Feed the breaker: 403 counts toward opening it; 2xx resets.
                    self.note_forbidden_status(status);

                    if !status.is_success() {
                        let code = status.as_u16();
                        if status == reqwest::StatusCode::FORBIDDEN {
                            let preview = body_preview(response).await;
                            log::warn!("[CMAF] file/url 403 for track {}{}", track_id, preview);
                            return Err(ApiError::Forbidden(preview));
                        }
                        return Err(if code == 404 {
                            ApiError::TrackUnavailable(track_id)
                        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                            // Honor Retry-After (seconds) when the server sends it.
                            let retry_after = response
                                .headers()
                                .get(reqwest::header::RETRY_AFTER)
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.trim().parse::<u64>().ok())
                                .unwrap_or(2);
                            ApiError::RateLimited(retry_after)
                        } else if status.is_server_error() {
                            ApiError::ServerError(code)
                        } else {
                            ApiError::ApiResponse(format!("file/url failed with status {}", status))
                        });
                    }

                    let file_url: TrackFileUrl = response.json().await?;
                    log::info!(
                        "[CMAF] file/url result: segments={}, mime={:?}, sampling_rate={:?}",
                        file_url.n_segments,
                        file_url.mime_type,
                        file_url.sampling_rate
                    );

                    Ok(file_url)
                }
            },
        )
        .await
    }
}

impl Default for QobuzClient {
    fn default() -> Self {
        Self::new().expect("Failed to create client")
    }
}
