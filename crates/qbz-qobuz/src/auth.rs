//! Authentication and request signing

use md5::{Digest, Md5};
use std::time::{SystemTime, UNIX_EPOCH};

/// Generate MD5 signature for protected API endpoints
///
/// Signature format: MD5(method + params + timestamp + secret)
pub(crate) fn generate_signature(
    method: &str,
    params: &str,
    timestamp: u64,
    secret: &str,
) -> String {
    let sig_string = format!("{}{}{}{}", method, params, timestamp, secret);
    let mut hasher = Md5::new();
    hasher.update(sig_string.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate signature for track/getFileUrl endpoint
pub(crate) fn sign_get_file_url(
    track_id: u64,
    format_id: u32,
    timestamp: u64,
    secret: &str,
) -> String {
    let params = format!("format_id{}intentstreamtrack_id{}", format_id, track_id);
    generate_signature("trackgetFileUrl", &params, timestamp, secret)
}

/// Generic request signature for any endpoint.
///
/// `method` is the endpoint path with slashes removed, e.g. "/album/get" → "albumget".
/// `kv_pairs` are (key, value) pairs sorted alphabetically by key.
/// The params string is built as key1value1key2value2... (same as mobile app interceptor).
pub(crate) fn sign_request(
    method: &str,
    kv_pairs: &[(&str, &str)],
    timestamp: u64,
    secret: &str,
) -> String {
    let mut sorted = kv_pairs.to_vec();
    sorted.sort_by_key(|(k, _)| *k);
    let params: String = sorted.iter().map(|(k, v)| format!("{}{}", k, v)).collect();
    generate_signature(method, &params, timestamp, secret)
}

/// Get current Unix timestamp
pub(crate) fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
}

/// Generate CMAF signature for session/start endpoint.
///
/// `seed` comes from the live webplayer bundle via
/// `QobuzClient::cmaf_seed` — it is deliberately NOT a constant here, so
/// nothing in this repository ships the value and a rotation upstream is
/// picked up by the next bundle refresh rather than by a release.
pub(crate) fn sign_session_start(timestamp: u64, seed: &str) -> String {
    let mut args = std::collections::BTreeMap::new();
    args.insert("profile", "qbz-1".to_string());
    qbz_cmaf::compute_request_sig("sessionstart", &args, &timestamp.to_string(), seed)
}

/// Generate CMAF signature for file/url endpoint. `seed` as above.
pub(crate) fn sign_file_url(track_id: u64, format_id: u32, timestamp: u64, seed: &str) -> String {
    let mut args = std::collections::BTreeMap::new();
    args.insert("format_id", format_id.to_string());
    args.insert("intent", "stream".to_string());
    args.insert("track_id", track_id.to_string());
    qbz_cmaf::compute_request_sig("fileurl", &args, &timestamp.to_string(), seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_signature() {
        let sig = generate_signature("test", "params", 1234567890, "secret");
        assert_eq!(sig.len(), 32); // MD5 hex is 32 chars
    }

    #[test]
    fn test_sign_get_file_url() {
        let sig = sign_get_file_url(123456, 27, 1234567890, "testsecret");
        assert_eq!(sig.len(), 32);
    }
}
