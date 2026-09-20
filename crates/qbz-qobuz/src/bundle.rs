//! Bundle token extraction from Qobuz web player
//!
//! Extracts app_id and secrets from the Qobuz JavaScript bundle.
//! This is necessary because Qobuz doesn't provide a public API.

use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use super::error::{ApiError, Result};

const LOGIN_PAGE_URL: &str = "https://play.qobuz.com/login";
const BUNDLE_BASE_URL: &str = "https://play.qobuz.com";

/// Per-request ceiling for the bundle fetch. The login page is tiny but the
/// bundle.js is ~7 MB and served from a CDN that is sometimes very slow; without
/// this, a stalled download blocks the entire app startup indefinitely.
const BUNDLE_FETCH_TIMEOUT: Duration = Duration::from_secs(45);
/// Extra attempts after the first on a failed/timed-out extraction.
const BUNDLE_EXTRACTION_RETRIES: usize = 2;

/// Extracted bundle tokens
#[derive(Debug, Clone)]
pub struct BundleTokens {
    pub app_id: String,
    pub secrets: Vec<String>,
    /// OAuth private key used for the /oauth/callback exchange.
    /// Present in recent bundle versions; None on older bundles.
    pub private_key: Option<String>,
    /// CMAF seed CANDIDATES, best-scoring first — see
    /// [`extract_cmaf_seed_candidates`]. The bundle gives no label to match on,
    /// so the winner is decided by `QobuzClient::cmaf_seed`, which signs a
    /// `session/start` with each in turn exactly as `secret()` does with
    /// `test_secret`. Empty means no CMAF on this bundle; the legacy path
    /// needs none.
    pub cmaf_seeds: Vec<String>,
}

/// On-disk cache of the extracted tokens, keyed by the Qobuz bundle version
/// (e.g. `8.1.0-b019`) so we can detect when Qobuz rotates the bundle and the
/// secrets change. Lives in the regenerable cache dir, never in precious data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedBundle {
    pub bundle_version: String,
    pub app_id: String,
    pub secrets: Vec<String>,
    #[serde(default)]
    pub private_key: Option<String>,
    /// `default` so a cache file written before CMAF seeds were extracted still
    /// deserializes — it just reports no candidates, and the next bundle
    /// rotation refills it.
    #[serde(default)]
    pub cmaf_seeds: Vec<String>,
    /// Unix seconds when these tokens were fetched (freshness only; not a TTL).
    pub fetched_at: i64,
}

impl From<CachedBundle> for BundleTokens {
    fn from(c: CachedBundle) -> Self {
        BundleTokens {
            app_id: c.app_id,
            secrets: c.secrets,
            private_key: c.private_key,
            cmaf_seeds: c.cmaf_seeds,
        }
    }
}

fn cache_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("qbz").join("bundle_tokens.json"))
}

/// Load cached tokens if a valid cache file exists. Returns `None` on any error
/// (missing file, malformed JSON, empty fields) so the caller falls back to a
/// live fetch.
pub fn load_cached_bundle() -> Option<CachedBundle> {
    let path = cache_path()?;
    let data = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<CachedBundle>(&data) {
        Ok(c) if !c.app_id.is_empty() && !c.secrets.is_empty() => Some(c),
        Ok(_) => {
            log::warn!("[Bundle] Cached tokens missing app_id/secrets, ignoring");
            None
        }
        Err(e) => {
            log::warn!("[Bundle] Failed to parse token cache: {}", e);
            None
        }
    }
}

fn save_cached_bundle(c: &CachedBundle) {
    let Some(path) = cache_path() else {
        log::warn!("[Bundle] No cache dir available, skipping token cache write");
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_vec_pretty(c) {
        Ok(bytes) => match std::fs::write(&path, bytes) {
            Ok(_) => log::info!("[Bundle] Cached tokens (version {})", c.bundle_version),
            Err(e) => log::warn!("[Bundle] Failed to write token cache: {}", e),
        },
        Err(e) => log::warn!("[Bundle] Failed to serialize token cache: {}", e),
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Parse the bundle version out of the `/resources/<version>/bundle.js` path,
/// e.g. `/resources/8.1.0-b019/bundle.js` -> `8.1.0-b019`.
fn bundle_version_from_url(bundle_url: &str) -> String {
    bundle_url
        .trim_start_matches("/resources/")
        .trim_end_matches("/bundle.js")
        .to_string()
}

/// Fetch the login page and return the current bundle URL + parsed version.
/// Cheap (~small page); used both by the full extraction and the background
/// version check.
async fn fetch_bundle_url(client: &Client) -> Result<(String, String)> {
    let login_page = client
        .get(LOGIN_PAGE_URL)
        .timeout(BUNDLE_FETCH_TIMEOUT)
        .send()
        .await?
        .text()
        .await?;
    let bundle_url = extract_bundle_url(&login_page)?;
    let version = bundle_version_from_url(&bundle_url);
    Ok((bundle_url, version))
}

/// Single network extraction attempt: fetch login page -> bundle.js -> parse.
/// Returns the tokens together with the bundle version they came from.
async fn extract_bundle_tokens_once(client: &Client) -> Result<(BundleTokens, String)> {
    // Step 1: Get login page to find bundle URL + version
    let (bundle_url, version) = fetch_bundle_url(client).await?;
    let full_bundle_url = format!("{}{}", BUNDLE_BASE_URL, bundle_url);

    // Step 2: Fetch the bundle (large; bounded by BUNDLE_FETCH_TIMEOUT)
    let bundle_content = client
        .get(&full_bundle_url)
        .timeout(BUNDLE_FETCH_TIMEOUT)
        .send()
        .await?
        .text()
        .await?;

    // Step 3: Extract app_id
    let app_id = extract_app_id(&bundle_content)?;

    // Step 4: Extract secrets
    let secrets = extract_secrets(&bundle_content)?;

    if secrets.is_empty() {
        return Err(ApiError::BundleExtractionError(
            "No secrets found in bundle".to_string(),
        ));
    }

    // Step 5: Extract OAuth private_key (optional - present in newer bundles)
    let private_key = extract_private_key(&bundle_content);
    if private_key.is_some() {
        log::info!("OAuth private_key extracted from bundle");
    } else {
        log::debug!("OAuth private_key not found in bundle (older bundle version)");
    }

    // Step 6: Collect CMAF seed candidates (optional). None found is not an
    // error — it costs the CMAF path, and the legacy path carries on.
    let cmaf_seeds = extract_cmaf_seed_candidates(&bundle_content, &secrets);
    if cmaf_seeds.is_empty() {
        log::info!("[Bundle] No CMAF seed candidates in bundle; CMAF playback unavailable");
    } else {
        log::info!(
            "[Bundle] {} CMAF seed candidate(s) extracted; the first to sign a session/start wins",
            cmaf_seeds.len()
        );
    }

    Ok((
        BundleTokens {
            app_id,
            secrets,
            private_key,
            cmaf_seeds,
        },
        version,
    ))
}

/// Extract app_id, secrets, and OAuth private_key from the live Qobuz bundle,
/// with a small retry loop, and persist the result to the on-disk cache.
///
/// This is the network ("cold") path. Prefer [`load_cached_bundle`] +
/// [`refresh_bundle_if_changed`] on warm starts so the UI never blocks on the
/// 7 MB download.
pub async fn extract_and_cache_bundle_tokens(client: &Client) -> Result<BundleTokens> {
    let mut last_err: Option<ApiError> = None;
    let attempts = BUNDLE_EXTRACTION_RETRIES + 1;
    for attempt in 1..=attempts {
        match extract_bundle_tokens_once(client).await {
            Ok((tokens, version)) => {
                save_cached_bundle(&CachedBundle {
                    bundle_version: version,
                    app_id: tokens.app_id.clone(),
                    secrets: tokens.secrets.clone(),
                    private_key: tokens.private_key.clone(),
                    cmaf_seeds: tokens.cmaf_seeds.clone(),
                    fetched_at: now_unix(),
                });
                return Ok(tokens);
            }
            Err(e) => {
                log::warn!(
                    "[Bundle] Extraction attempt {}/{} failed: {}",
                    attempt,
                    attempts,
                    e
                );
                last_err = Some(e);
                // Back off before the next attempt. The attempts used to fire
                // back-to-back, so a brief network hiccup (DNS blip, dropped
                // connection, captive-portal redirect) failed all of them in a
                // few ms — the retries were effectively useless. A short growing
                // delay gives a transient failure time to clear.
                if attempt < attempts {
                    tokio::time::sleep(Duration::from_millis(600 * attempt as u64)).await;
                }
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| ApiError::BundleExtractionError("bundle extraction failed".into())))
}

/// Background refresh: cheaply re-check the current bundle version. If Qobuz
/// rotated the bundle, re-extract (and re-cache) the new secrets and return
/// them; if unchanged, just bump the cache freshness timestamp and return
/// `None`. Never blocks the UI — call from a spawned task.
pub async fn refresh_bundle_if_changed(
    client: &Client,
    cached_version: &str,
) -> Option<BundleTokens> {
    let (_, version) = fetch_bundle_url(client).await.ok()?;
    if version == cached_version {
        if let Some(mut c) = load_cached_bundle() {
            c.fetched_at = now_unix();
            save_cached_bundle(&c);
        }
        log::debug!("[Bundle] Background check: version {} unchanged", version);
        return None;
    }
    log::info!(
        "[Bundle] Background check: version changed {} -> {}, re-extracting",
        cached_version,
        version
    );
    extract_and_cache_bundle_tokens(client).await.ok()
}

/// Backwards-compatible one-shot extraction (no caching). Retained for callers
/// that just want a live fetch; the app startup path uses
/// [`extract_and_cache_bundle_tokens`] instead.
pub async fn extract_bundle_tokens(client: &Client) -> Result<BundleTokens> {
    extract_bundle_tokens_once(client).await.map(|(t, _)| t)
}

fn extract_bundle_url(html: &str) -> Result<String> {
    // Pattern: <script src="/resources/X.X.X-bXXX/bundle.js"></script>
    let re =
        Regex::new(r#"<script src="(/resources/\d+\.\d+\.\d+-[a-z]\d{3}/bundle\.js)"></script>"#)
            .expect("Invalid regex");

    re.captures(html)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| ApiError::BundleExtractionError("Bundle URL not found".to_string()))
}

fn extract_app_id(bundle: &str) -> Result<String> {
    // Pattern: production:{api:{appId:"XXXXXXXXX"
    let re = Regex::new(r#"production:\{api:\{appId:"(?P<app_id>\d{9})""#).expect("Invalid regex");

    re.captures(bundle)
        .and_then(|caps| caps.name("app_id"))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| ApiError::BundleExtractionError("App ID not found".to_string()))
}

fn extract_secrets(bundle: &str) -> Result<Vec<String>> {
    // Extract seeds with their timezone keys
    // Pattern: X.initialSeed("SEED",window.utimezone.TIMEZONE)
    let seed_re = Regex::new(
        r#"[a-z]\.initialSeed\("(?P<seed>[\w=]+)",window\.utimezone\.(?P<timezone>[a-z]+)\)"#,
    )
    .expect("Invalid regex");

    let mut seeds: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut timezones: Vec<String> = Vec::new();

    for caps in seed_re.captures_iter(bundle) {
        if let (Some(seed), Some(tz)) = (caps.name("seed"), caps.name("timezone")) {
            let tz_str = tz.as_str().to_string();
            seeds.insert(tz_str.clone(), seed.as_str().to_string());
            timezones.push(tz_str);
        }
    }

    log::debug!(
        "Found {} seeds with timezones: {:?}",
        seeds.len(),
        timezones
    );

    if seeds.is_empty() {
        return Err(ApiError::BundleExtractionError(
            "No seeds found".to_string(),
        ));
    }

    // Build dynamic regex with found timezones (capitalize first letter for matching)
    // Pattern: name:"\w+/Timezone",info:"INFO",extras:"EXTRAS"
    let tz_pattern: Vec<String> = timezones
        .iter()
        .map(|tz| {
            // Capitalize first letter: "berlin" -> "Berlin"
            let mut chars = tz.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect();

    let tz_alternatives = tz_pattern.join("|");
    let info_pattern = format!(
        r#"name:"\w+/(?P<timezone>{})",info:"(?P<info>[\w=]+)",extras:"(?P<extras>[\w=]+)""#,
        tz_alternatives
    );

    log::debug!("Info regex pattern: {}", info_pattern);

    let info_re = Regex::new(&info_pattern).expect("Invalid info regex");

    let mut secrets = Vec::new();

    for caps in info_re.captures_iter(bundle) {
        if let (Some(tz), Some(info), Some(extras)) = (
            caps.name("timezone"),
            caps.name("info"),
            caps.name("extras"),
        ) {
            // Convert capitalized timezone back to lowercase for lookup
            let tz_lower = tz.as_str().to_lowercase();
            if let Some(seed) = seeds.get(&tz_lower) {
                // Concatenate seed + info + extras, remove last 44 chars, base64 decode
                let combined = format!("{}{}{}", seed, info.as_str(), extras.as_str());
                log::debug!(
                    "Combined length: {}, timezone: {}",
                    combined.len(),
                    tz_lower
                );

                if combined.len() > 44 {
                    let trimmed = &combined[..combined.len() - 44];
                    match base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        trimmed,
                    ) {
                        Ok(decoded) => {
                            if let Ok(secret) = String::from_utf8(decoded) {
                                log::info!(
                                    "Successfully extracted secret for timezone: {}",
                                    tz_lower
                                );
                                secrets.push(secret);
                            }
                        }
                        Err(e) => {
                            log::debug!("Base64 decode failed for {}: {}", tz_lower, e);
                        }
                    }
                }
            }
        }
    }

    // If the complex extraction fails, try a simpler pattern
    // that might work for some bundle versions
    if secrets.is_empty() {
        log::warn!("Complex extraction failed, trying simple appSecret pattern");
        let simple_re = Regex::new(r#"appSecret:"([a-f0-9]{32})""#).expect("Invalid regex");
        for caps in simple_re.captures_iter(bundle) {
            if let Some(secret) = caps.get(1) {
                secrets.push(secret.as_str().to_string());
            }
        }
    }

    log::info!("Extracted app secrets from bundle");
    Ok(secrets)
}

/// How many candidates `QobuzClient::cmaf_seed` may spend a `session/start`
/// probe on. Each costs one request, but only on a cold cache or after Qobuz
/// rotates the bundle — the winner is cached by bundle version.
pub const MAX_CMAF_SEED_CANDIDATES: usize = 8;

/// Bytes either side of a literal that count as its context when scoring.
const CMAF_SEED_CONTEXT: usize = 512;

/// Tokens that suggest a nearby 32-hex literal is the CMAF seed, and what each
/// is worth. `cmaf` and `qbz-1` are the strong ones: `qbz-1` is the profile
/// name posted to `session/start` and appears essentially nowhere else.
const CMAF_SEED_HINTS: &[(&str, u32)] = &[
    ("cmaf", 10),
    ("qbz-1", 8),
    ("sessionstart", 6),
    ("session/start", 6),
    ("fileurl", 4),
    ("file/url", 4),
    ("hkdf", 4),
    ("seed", 3),
    ("request_sig", 2),
];

/// Snap `[lo, hi)` outwards to the nearest char boundaries so a slice of a
/// minified bundle can't split a multi-byte character.
fn char_bounded(s: &str, lo: usize, hi: usize) -> &str {
    let mut lo = lo.min(s.len());
    let mut hi = hi.min(s.len());
    while lo > 0 && !s.is_char_boundary(lo) {
        lo -= 1;
    }
    while hi < s.len() && !s.is_char_boundary(hi) {
        hi += 1;
    }
    &s[lo..hi]
}

/// CMAF seed candidates from the bundle, best-scoring first.
///
/// Unlike `appId` and `appSecret`, the seed carries no label to anchor a regex
/// on — the value is just a 32-hex string literal, and a 7 MB minified bundle
/// holds plenty of those. So this does not try to identify it: it collects
/// every 32-hex literal, scores each by what appears NEAR it
/// ([`CMAF_SEED_HINTS`]), and returns the best few for
/// `QobuzClient::cmaf_seed` to settle against the live endpoint. That mirrors
/// how `secret()` already picks among `secrets` with `test_secret`, and it is
/// why a hint list being slightly wrong costs a wasted probe rather than a
/// broken CMAF path.
///
/// `known_secrets` are filtered out: the legacy app secrets are also 32 hex
/// and would otherwise burn probe slots.
///
/// An empty result is a normal outcome, not an error — it means this bundle
/// exposes no seed in a shape this sees, and CMAF is simply unavailable.
pub fn extract_cmaf_seed_candidates(bundle: &str, known_secrets: &[String]) -> Vec<String> {
    let re = Regex::new(r#""([0-9a-f]{32})""#).expect("Invalid regex");

    // Best score wins per distinct value: the same literal can appear more than
    // once, and only its most promising neighbourhood matters.
    let mut scored: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for caps in re.captures_iter(bundle) {
        let Some(m) = caps.get(1) else { continue };
        let value = m.as_str();
        if known_secrets.iter().any(|s| s == value) {
            continue;
        }

        let context = char_bounded(
            bundle,
            m.start().saturating_sub(CMAF_SEED_CONTEXT),
            m.end() + CMAF_SEED_CONTEXT,
        )
        .to_ascii_lowercase();

        let score = CMAF_SEED_HINTS
            .iter()
            .filter(|(token, _)| context.contains(token))
            .map(|(_, weight)| weight)
            .sum();

        let slot = scored.entry(value.to_string()).or_insert(0);
        *slot = (*slot).max(score);
    }

    // A literal with no hint at all anywhere near it is just one of the
    // bundle's many hashes. Keeping those would fill every probe slot with
    // noise on a bundle that has no seed to find.
    let mut candidates: Vec<(String, u32)> =
        scored.into_iter().filter(|(_, score)| *score > 0).collect();
    // Score first, then the value itself, so the order is stable across runs
    // (HashMap iteration is not) and a cached candidate list matches a
    // re-extracted one.
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    candidates.truncate(MAX_CMAF_SEED_CANDIDATES);

    log::debug!(
        "[Bundle] {} CMAF seed candidate(s) after scoring",
        candidates.len()
    );
    candidates.into_iter().map(|(value, _)| value).collect()
}

fn extract_private_key(bundle: &str) -> Option<String> {
    // Pattern: privateKey:"VALUE" (the static OAuth key used in /oauth/callback)
    let re = Regex::new(r#"privateKey:\s*"(?P<key>[A-Za-z0-9]{6,30})""#).expect("Invalid regex");

    re.captures(bundle)
        .and_then(|caps| caps.name("key"))
        .map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_bundle_url() {
        let html = r#"<script src="/resources/7.0.1-b001/bundle.js"></script>"#;
        let result = extract_bundle_url(html);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/resources/7.0.1-b001/bundle.js");
    }

    #[test]
    fn test_extract_app_id() {
        let bundle = r#"production:{api:{appId:"123456789",appSecret:"abc"}"#;
        let result = extract_app_id(bundle);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "123456789");
    }

    /// A 32-hex literal with no CMAF vocabulary near it is one of the bundle's
    /// many hashes. Scoring those in would fill every probe slot with noise.
    #[test]
    fn cmaf_seed_candidates_ignore_unhinted_hex() {
        let bundle = r#"var a={etag:"0123456789abcdef0123456789abcdef",x:1}"#;
        assert!(extract_cmaf_seed_candidates(bundle, &[]).is_empty());
    }

    /// The fixture value here is invented, and must stay that way: putting a
    /// real seed in a test would re-commit to this repository the exact thing
    /// this module exists to stop shipping.
    #[test]
    fn cmaf_seed_candidates_find_a_hinted_literal() {
        let fake_seed = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        let bundle = format!(r#"cmafProfile:"qbz-1",k:"{fake_seed}""#);
        assert_eq!(
            extract_cmaf_seed_candidates(&bundle, &[]),
            vec![fake_seed.to_string()]
        );
    }

    /// The whole point of scoring: the bundle holds many 32-hex literals and
    /// the seed is not labelled, so the one in CMAF company must sort first.
    #[test]
    fn cmaf_seed_candidates_rank_by_context() {
        let near = "a".repeat(32);
        let far = "b".repeat(32).replace('b', "c");
        let bundle = format!(
            r#"{{hash:"{far}",note:"seed"}} ... cmaf:{{profile:"qbz-1",sessionstart:1,k:"{near}"}}"#
        );
        let got = extract_cmaf_seed_candidates(&bundle, &[]);
        assert_eq!(got.first(), Some(&near), "got {got:?}");
    }

    /// The legacy app secrets are 32 hex too, and are already known to be
    /// something else — they must not burn probe slots.
    #[test]
    fn cmaf_seed_candidates_exclude_known_secrets() {
        let secret = "d".repeat(32);
        let bundle = format!(r#"cmaf:{{appSecret:"{secret}"}}"#);
        assert!(extract_cmaf_seed_candidates(&bundle, &[secret]).is_empty());
    }

    #[test]
    fn cmaf_seed_candidates_are_capped_and_deterministic() {
        // Twice the cap, all equally hinted, so only the tie-break on value
        // decides the order.
        let mut bundle = String::from("cmaf:{");
        for i in 0..(MAX_CMAF_SEED_CANDIDATES * 2) {
            bundle.push_str(&format!(r#"k{i}:"{:032x}","#, i));
        }
        bundle.push('}');

        let first = extract_cmaf_seed_candidates(&bundle, &[]);
        assert_eq!(first.len(), MAX_CMAF_SEED_CANDIDATES);
        assert_eq!(first, extract_cmaf_seed_candidates(&bundle, &[]));
    }

    /// The context window is sliced out of a 7 MB minified bundle by byte
    /// offset; a multi-byte character straddling the edge must not panic.
    #[test]
    fn cmaf_seed_candidates_survive_multibyte_context() {
        let bundle = format!(
            r#"{}cmaf:"{}"{}"#,
            "é".repeat(400),
            "e".repeat(32),
            "ü".repeat(400)
        );
        assert_eq!(
            extract_cmaf_seed_candidates(&bundle, &[]),
            vec!["e".repeat(32)]
        );
    }
}
