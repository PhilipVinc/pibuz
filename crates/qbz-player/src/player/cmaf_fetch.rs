//! Fetching one CMAF segment, the way a feeder that outlives its URL has to.
//!
//! `Player::cmaf_stream_segments` fetched each segment with one bare GET: no
//! status check (a 4xx error page went on to fail in the decryptor, loudly but
//! misleadingly), no retry (a WiFi blip at a seek was the end of the track),
//! and no way to renew the signed URL template. That last one matters because
//! a CMAF feeder, like the remote one, stays up after completion to serve
//! seeks and keeps its template for as long as a pause lasts — on the Pi the
//! remote path's URL was dead two hours after it was resolved, and the resume
//! that met it wedged the audio thread for 18 hours.
//!
//! So this answers the three cases differently:
//! - EXPIRED (401/403/410): re-run the CMAF setup through a [`CmafRefresher`]
//!   and carry on — but only if the segment table it returns is IDENTICAL,
//!   because that table is the byte index every offset in the buffer is built
//!   on (`qbz_cmaf::map`). Another file's segments at those offsets would decode
//!   as noise rather than fail.
//! - TRANSIENT (no connection, a dropped body, 5xx): retry after
//!   [`TRANSIENT_RETRY_DELAYS`], then give up with the reason.
//! - anything else non-2xx: fail at once, naming the status.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Re-runs the CMAF setup for the track a feeder is serving, for a fresh
/// signed URL template and content key.
pub type CmafRefresher = Arc<
    dyn Fn() -> Pin<
            Box<dyn Future<Output = Result<qbz_qobuz::cmaf::CmafStreamingInfo, String>> + Send>,
        > + Send
        + Sync,
>;

/// Where a CMAF feeder fetches from, and what a refresh must still agree with.
pub(crate) struct CmafSource {
    pub url_template: String,
    pub content_key: [u8; 16],
    pub segment_table: Vec<qbz_cmaf::SegmentTableEntry>,
}

/// Wait before each retry of a segment fetch that failed in transit; its
/// length is how many retries there are. Short in tests, which only count.
pub(crate) const TRANSIENT_RETRY_DELAYS: [Duration; 3] = if cfg!(test) {
    [Duration::from_millis(10); 3]
} else {
    [
        Duration::from_millis(500),
        Duration::from_millis(1500),
        Duration::from_secs(4),
    ]
};

fn means_the_url_expired(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403 | 410)
}

fn is_transient_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

/// Fetch the raw (still encrypted) bytes of segment `seg`.
///
/// On an expiry it refreshes `source` in place — template and key — so every
/// later segment uses the new URL too.
pub(crate) async fn fetch_cmaf_segment(
    client: &reqwest::Client,
    source: &mut CmafSource,
    seg: usize,
    refresh: Option<&CmafRefresher>,
    track_id: u64,
) -> Result<bytes::Bytes, String> {
    let mut transient_failures = 0usize;
    let mut refreshed = false;
    loop {
        let url = source.url_template.replace("$SEGMENT$", &seg.to_string());
        let outcome = match client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                response.bytes().await.map_err(|e| format!("read: {e}"))
            }
            Ok(response) => {
                let status = response.status();
                if means_the_url_expired(status) && !refreshed {
                    if let Some(refresh) = refresh {
                        log::warn!(
                            "[CMAF-STREAM] Track {track_id}: segment {seg} got {status} - the signed \
                             URL has expired, re-running the CMAF setup"
                        );
                        let fresh = refresh().await.map_err(|e| {
                            format!(
                                "CMAF segment {seg}: the URL expired ({status}) and renewing it \
                                 failed: {e}"
                            )
                        })?;
                        if fresh.segment_table != source.segment_table {
                            return Err(format!(
                                "CMAF segment {seg}: the renewed stream has a different segment \
                                 table ({} segments, was {}); refusing to splice a different \
                                 file in",
                                fresh.segment_table.len(),
                                source.segment_table.len()
                            ));
                        }
                        source.url_template = fresh.url_template;
                        source.content_key = fresh.content_key;
                        refreshed = true;
                        continue;
                    }
                }
                if !is_transient_status(status) {
                    return Err(format!("CMAF segment {seg} fetch: status {status}"));
                }
                Err(format!("status {status}"))
            }
            Err(e) => Err(format!("fetch: {e}")),
        };
        match outcome {
            Ok(bytes) => return Ok(bytes),
            Err(why) => {
                let Some(delay) = TRANSIENT_RETRY_DELAYS.get(transient_failures) else {
                    return Err(format!(
                        "CMAF segment {seg} failed after {} attempt(s): {why}",
                        transient_failures + 1
                    ));
                };
                transient_failures += 1;
                log::warn!(
                    "[CMAF-STREAM] Track {track_id}: segment {seg} failed ({why}) - retry {} of {} \
                     in {} ms",
                    transient_failures,
                    TRANSIENT_RETRY_DELAYS.len(),
                    delay.as_millis()
                );
                tokio::time::sleep(*delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What the fake edge does with one request.
    #[derive(Clone)]
    enum Reply {
        /// Close without answering — a blip mid-request.
        Hangup,
        Status(u16),
        Body(Vec<u8>),
    }

    /// A loopback edge that answers each request with the next scripted
    /// [`Reply`], falling back to a body naming the path it was asked for.
    #[derive(Clone)]
    struct FakeEdge {
        base: String,
        script: Arc<Mutex<VecDeque<Reply>>>,
        paths: Arc<Mutex<Vec<String>>>,
    }

    impl FakeEdge {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let edge = FakeEdge {
                base: format!("http://{}", listener.local_addr().unwrap()),
                script: Arc::new(Mutex::new(VecDeque::new())),
                paths: Arc::new(Mutex::new(Vec::new())),
            };
            let served = edge.clone();
            tokio::spawn(async move {
                while let Ok((conn, _)) = listener.accept().await {
                    tokio::spawn(served.clone().answer(conn));
                }
            });
            edge
        }

        fn script(&self, replies: &[Reply]) {
            self.script.lock().unwrap().extend(replies.iter().cloned());
        }

        fn requests(&self) -> Vec<String> {
            self.paths.lock().unwrap().clone()
        }

        async fn answer(self, mut conn: tokio::net::TcpStream) {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if conn.read(&mut byte).await.unwrap_or(0) == 0 {
                    return;
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            self.paths.lock().unwrap().push(path.clone());
            let reply = self.script.lock().unwrap().pop_front();
            let (status, body) = match reply.unwrap_or(Reply::Body(path.into_bytes())) {
                Reply::Hangup => return,
                Reply::Status(code) => (code, Vec::new()),
                Reply::Body(body) => (200, body),
            };
            let header = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(header.as_bytes()).await;
            let _ = conn.write_all(&body).await;
        }
    }

    fn table(lens: &[u32]) -> Vec<qbz_cmaf::SegmentTableEntry> {
        lens.iter()
            .map(|&byte_len| qbz_cmaf::SegmentTableEntry {
                byte_len,
                sample_count: 4096,
            })
            .collect()
    }

    fn source(edge: &FakeEdge, version: &str) -> CmafSource {
        CmafSource {
            url_template: format!("{}/{version}/$SEGMENT$", edge.base),
            content_key: [1; 16],
            segment_table: table(&[100, 200, 300]),
        }
    }

    fn renewed(edge: &FakeEdge, version: &str, lens: &[u32]) -> CmafRefresher {
        let template = format!("{}/{version}/$SEGMENT$", edge.base);
        let lens = lens.to_vec();
        Arc::new(move || {
            let (template, lens) = (template.clone(), lens.clone());
            Box::pin(async move {
                Ok(qbz_qobuz::cmaf::CmafStreamingInfo {
                    url_template: template,
                    n_segments: lens.len() as u8,
                    content_key: [2; 16],
                    flac_header: Vec::new(),
                    segment_table: table(&lens),
                    format_id: 7,
                    sampling_rate: Some(96_000),
                    bit_depth: Some(24),
                    init_fetch_ms: 0,
                })
            })
        })
    }

    fn client() -> &'static reqwest::Client {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        qbz_qobuz::cdn::client().unwrap()
    }

    /// A blip and a struggling edge are ridden out; the segment arrives.
    #[tokio::test]
    async fn a_segment_rides_out_a_brief_outage() {
        let edge = FakeEdge::start().await;
        edge.script(&[Reply::Hangup, Reply::Status(503)]);
        let mut src = source(&edge, "v1");
        let bytes = fetch_cmaf_segment(client(), &mut src, 3, None, 1).await;
        assert_eq!(bytes.unwrap().as_ref(), b"/v1/3");
        assert_eq!(edge.requests().len(), 3);
    }

    /// An outage that does not end is given up on after a bounded number of
    /// tries, with the reason.
    #[tokio::test]
    async fn a_lasting_outage_fails_after_bounded_retries() {
        let edge = FakeEdge::start().await;
        edge.script(&vec![Reply::Status(502); 16]);
        let mut src = source(&edge, "v1");
        let err = fetch_cmaf_segment(client(), &mut src, 3, None, 1)
            .await
            .unwrap_err();
        assert!(
            err.contains("after 4 attempt(s)") && err.contains("502"),
            "{err}"
        );
        assert_eq!(edge.requests().len(), 1 + TRANSIENT_RETRY_DELAYS.len());
    }

    /// An error page is not a segment. It used to be handed to the decryptor,
    /// which failed on it with an error that named the decryptor.
    #[tokio::test]
    async fn a_refusal_fails_at_once_naming_the_status() {
        let edge = FakeEdge::start().await;
        edge.script(&[Reply::Status(404)]);
        let mut src = source(&edge, "v1");
        let err = fetch_cmaf_segment(client(), &mut src, 3, None, 1)
            .await
            .unwrap_err();
        assert!(err.contains("404"), "{err}");
        assert_eq!(edge.requests().len(), 1, "a refusal is not retried");
    }

    /// The Pi's failure, on the CMAF path: the signed template expired during
    /// a pause. It is renewed, the segment arrives from the new one, and the
    /// renewal sticks for the segments after it — key included.
    #[tokio::test]
    async fn an_expired_template_is_renewed_and_kept() {
        let edge = FakeEdge::start().await;
        edge.script(&[Reply::Status(410)]);
        let mut src = source(&edge, "v1");
        let refresh = renewed(&edge, "v2", &[100, 200, 300]);
        let bytes = fetch_cmaf_segment(client(), &mut src, 3, Some(&refresh), 1).await;
        assert_eq!(bytes.unwrap().as_ref(), b"/v2/3");
        assert_eq!(
            src.content_key, [2; 16],
            "the renewed key replaces the old one"
        );
        let next = fetch_cmaf_segment(client(), &mut src, 4, Some(&refresh), 1).await;
        assert_eq!(
            next.unwrap().as_ref(),
            b"/v2/4",
            "later segments use the new URL"
        );
    }

    /// A renewal that describes a different file — another format after a
    /// rights change — is refused: its segments at this buffer's offsets would
    /// decode as noise.
    #[tokio::test]
    async fn a_renewal_with_a_different_segment_table_is_refused() {
        let edge = FakeEdge::start().await;
        edge.script(&[Reply::Status(403)]);
        let mut src = source(&edge, "v1");
        let refresh = renewed(&edge, "v2", &[100, 250, 300]);
        let err = fetch_cmaf_segment(client(), &mut src, 3, Some(&refresh), 1)
            .await
            .unwrap_err();
        assert!(err.contains("refusing"), "{err}");
        assert_eq!(src.url_template, format!("{}/v1/$SEGMENT$", edge.base));
    }

    /// Without a refresher an expiry is a refusal like any other: it fails, it
    /// does not spin.
    #[tokio::test]
    async fn an_expiry_with_no_way_to_renew_fails() {
        let edge = FakeEdge::start().await;
        edge.script(&[Reply::Status(410)]);
        let mut src = source(&edge, "v1");
        let err = fetch_cmaf_segment(client(), &mut src, 3, None, 1)
            .await
            .unwrap_err();
        assert!(err.contains("410"), "{err}");
        assert_eq!(edge.requests().len(), 1);
    }
}
