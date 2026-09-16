//! URL redaction for anything that reaches the log.
//!
//! A Qobuz stream URL is a bearer credential: the CDN authorizes on a signed
//! token carried in the query string (and, for some edges, in the path), so a
//! copy of the URL plays the track for anyone holding it until it expires.
//!
//! `reqwest::Error`'s own `Display` embeds the full request URL, which means the
//! ordinary act of reporting a failed fetch — `format!("{err}")`, a `log::warn!`,
//! an error string handed back to the daemon API — writes that credential to
//! `pibuz.log`. That is the file people attach to bug reports (vicrodh/qbz#780
//! item 10; the desktop's Plex/Jellyfin/Subsonic clients strip the URL for this
//! exact reason and the fix never reached the Qobuz client).
//!
//! Everything here is text-level on purpose: it runs over a message that may
//! already be a chain of several errors from several layers, so it cannot
//! assume one `reqwest::Error` with one `url()` to remove.

/// Rewrite every URL in `text` down to `scheme://host`, dropping the path,
/// query, fragment and any userinfo.
///
/// The host survives because it is the diagnostic that matters — which CDN
/// answered, whether DNS went somewhere unexpected — and carries no secret. A
/// URL that was only a host is left exactly as it was, so nothing is marked
/// redacted that wasn't.
pub fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(at) = next_scheme(rest) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // A URL ends at the first character that cannot appear in one
        // unescaped. `)` matters most: reqwest writes `for url (https://...)`.
        let mut end = tail
            .find(|c: char| {
                c.is_whitespace() || matches!(c, ')' | '(' | '"' | '\'' | '<' | '>' | ',' | ';')
            })
            .unwrap_or(tail.len());
        // Sentence punctuation that happens to follow a URL is not part of it.
        // `:` matters here specifically: `describe_reqwest` joins an error chain
        // with ": ", so the cause separator sits flush against the URL and was
        // being swallowed with it. A port keeps its colon — that one is interior,
        // never trailing.
        while end > 0
            && matches!(
                tail.as_bytes()[end - 1],
                b'.' | b',' | b':' | b';' | b'!' | b'?'
            )
        {
            end -= 1;
        }
        out.push_str(&redact_one(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Chain-expand a `reqwest::Error` into its causes and redact the result.
///
/// The chain is what makes a network failure diagnosable — reqwest's own
/// message is often just "error sending request", with the real reason (TLS,
/// DNS, a hyper limit) one or two `source()` hops down — so it is kept, and
/// only the URLs in it are cut.
pub fn describe_reqwest(err: &reqwest::Error) -> String {
    use std::error::Error as _;

    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    redact_urls(&out)
}

/// Byte offset of the next `http://` or `https://`, whichever comes first.
fn next_scheme(text: &str) -> Option<usize> {
    let http = text.find("http://");
    let https = text.find("https://");
    match (http, https) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `scheme://user:pass@host:port/path?token=…` -> `scheme://host:port/<redacted>`.
fn redact_one(url: &str) -> String {
    let Some((scheme, after)) = url.split_once("://") else {
        return url.to_string();
    };
    // The authority runs to the first `/`, `?` or `#`; the rest is what hides
    // the token.
    let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..authority_end];
    // Credentials, if any, sit before the last `@`.
    let host = authority.rsplit('@').next().unwrap_or(authority);

    let had_credentials = host.len() != authority.len();
    if authority_end == after.len() && !had_credentials {
        // Nothing but a host was there — leave it alone rather than claim a
        // redaction that removed nothing.
        return url.to_string();
    }
    format!("{scheme}://{host}/<redacted>")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape reqwest actually produces, and the one that leaked.
    #[test]
    fn a_signed_stream_url_does_not_survive_the_message() {
        let msg = "error sending request for url (https://streaming-qobuz.akamaized.net/\
                   file.flac?eid=1&hmac=SECRETTOKEN&exp=99)";
        let out = redact_urls(msg);

        assert!(!out.contains("SECRETTOKEN"), "{out}");
        assert!(!out.contains("hmac"), "{out}");
        assert!(!out.contains("file.flac"), "{out}");
        // The host and the surrounding prose still read as a diagnosis.
        assert_eq!(
            out,
            "error sending request for url \
             (https://streaming-qobuz.akamaized.net/<redacted>)"
        );
    }

    #[test]
    fn credentials_in_the_authority_go_too() {
        assert_eq!(
            redact_urls("proxy https://user:hunter2@proxy.local:8080 refused"),
            "proxy https://proxy.local:8080/<redacted> refused"
        );
    }

    #[test]
    fn every_url_in_a_chain_is_cut_not_just_the_first() {
        let out =
            redact_urls("redirect http://a.example/p?t=1 -> https://b.example/q?t=2: too many");
        assert!(!out.contains("t=1"), "{out}");
        assert!(!out.contains("t=2"), "{out}");
        assert_eq!(
            out,
            "redirect http://a.example/<redacted> -> https://b.example/<redacted>: too many"
        );
    }

    /// Redaction must not become noise on messages that carry no URL, and a
    /// bare host is not a secret — claiming a redaction there would just make
    /// the log harder to read.
    #[test]
    fn text_without_a_secret_is_untouched() {
        for msg in [
            "connection closed before message completed",
            "probe range request failed with status 503",
            "reached https://qobuz.com",
        ] {
            assert_eq!(redact_urls(msg), msg);
        }
    }

    /// `remote_stream::is_header_flood_error` classifies by matching hyper's
    /// wording in the expanded chain. Redaction runs BEFORE that match, so it
    /// must leave the wording alone or the Akamai small-object path stops being
    /// recognised.
    /// The tests above assert against reqwest's message shape as we believe it
    /// to be. This one asserts against the shape reqwest ACTUALLY produces, so
    /// a future version that reformats its `Display` cannot quietly slip the
    /// URL past the parser. Offline: port 1 on loopback refuses immediately,
    /// no DNS and no network.
    #[tokio::test]
    async fn a_real_reqwest_error_loses_its_url() {
        // A test binary has no `main` to install the process-level rustls
        // provider the workspace's reqwest needs; `Client::new` panics without
        // one. Ignore an Err — another test in this binary may have won the race.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/track.flac?hmac=SECRETTOKEN")
            .send()
            .await
            .expect_err("connection to port 1 must fail");

        // Precondition: reqwest really does put the URL in its own message.
        // If this ever stops holding the test below proves nothing, so it is
        // asserted rather than assumed.
        assert!(
            err.to_string().contains("SECRETTOKEN"),
            "reqwest no longer embeds the URL — re-check whether this module is \
             still needed: {err}"
        );

        let described = describe_reqwest(&err);
        assert!(!described.contains("SECRETTOKEN"), "{described}");
        assert!(!described.contains("track.flac"), "{described}");
        assert!(described.contains("127.0.0.1:1"), "{described}");

        // And through the error type every caller actually prints.
        let api = crate::error::ApiError::from(err);
        let shown = api.to_string();
        assert!(!shown.contains("SECRETTOKEN"), "{shown}");
        assert!(shown.starts_with("Network error: "), "{shown}");
    }

    #[test]
    fn the_phrases_error_classification_depends_on_survive() {
        let msg = "error sending request for url (https://cdn.example/f?t=1): \
                   message head is too large";
        let out = redact_urls(msg);
        assert!(out.contains("message head is too large"), "{out}");
        assert!(!out.contains("t=1"), "{out}");
    }
}
