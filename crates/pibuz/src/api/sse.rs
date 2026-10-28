// crates/pibuz/src/api/sse.rs — the `GET /api/events` Server-Sent Events stream.
// A live push feed of CoreEvents so a client (a plasmoid, a bar
// applet, `pibuz watch`) reacts to playback/queue/library changes without
// polling.
//
// Concurrency: the control-plane serve loop is single-threaded, so an open SSE
// stream would starve every other request. `serve()` therefore moves this onto
// its OWN thread (Request is Send); `stream` blocks there until the client
// disconnects (a write fails) or the bus closes. The bus is a
// tokio broadcast, drained here with `blocking_recv()` from the plain thread.
//
// Wire format: one SSE frame per emitted event —
//   event: <CoreEvent type>\n
//   data: {"type":"…","data":{…}}\n\n
// The `data` line is the CoreEvent's own `#[serde(tag="type",content="data")]`
// JSON (single-line). A leading `: …` comment primes the stream; a dropped-lag
// notice is sent as a comment rather than silently losing ordering.
//
// Delivery: every frame is written and FLUSHED on its own. tiny_http's
// `respond` cannot do that for a body of unknown length: it wraps it in a
// chunked encoder that holds 8 KB before sending anything, over a 1 KB
// `BufWriter`, and flushes only when the body ends. A `PositionUpdated` is
// ~100 bytes every 2 s, so clients got the stream in bursts tens of seconds
// apart and `curl -N` saw not even the headers (#6). So this takes the raw
// connection (`into_writer`) and does the HTTP itself: a status line, the
// headers, then one chunk per frame (bare frames for HTTP/1.0, which has no
// chunked encoding and ends the body by closing).
use std::io::Write;

use tiny_http::Request;
use tokio::sync::broadcast;

use qbz_models::CoreEvent;

/// Stream CoreEvents to one client until it disconnects (a write fails) or the
/// bus closes. Runs on a dedicated thread.
pub(crate) fn stream(req: Request, rx: broadcast::Receiver<CoreEvent>) {
    let version = req.http_version();
    let chunked = (version.0, version.1) >= (1, 1);
    let mut out = req.into_writer();
    let _ = pump(&mut out, Frames::new(rx), chunked);
}

/// Write the response head, then every frame as it comes, each flushed; on
/// the bus closing, end the body. Errors are the client going away.
fn pump(out: &mut dyn Write, mut frames: Frames, chunked: bool) -> std::io::Result<()> {
    let (status, framing) = if chunked {
        ("HTTP/1.1 200 OK", "Transfer-Encoding: chunked")
    } else {
        ("HTTP/1.0 200 OK", "Connection: close")
    };
    write!(
        out,
        "{status}\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         X-Accel-Buffering: no\r\n\
         {framing}\r\n\r\n"
    )?;
    while let Some(frame) = frames.next_frame() {
        if chunked {
            write!(out, "{:X}\r\n", frame.len())?;
            out.write_all(&frame)?;
            out.write_all(b"\r\n")?;
        } else {
            out.write_all(&frame)?;
        }
        out.flush()?;
    }
    if chunked {
        out.write_all(b"0\r\n\r\n")?;
    }
    out.flush()
}

/// The frames to send, in order, blocking on the CoreEvent bus for each.
struct Frames {
    rx: broadcast::Receiver<CoreEvent>,
    primed: bool,
}

impl Frames {
    fn new(rx: broadcast::Receiver<CoreEvent>) -> Self {
        Frames { rx, primed: false }
    }

    /// The next frame to send, blocking for a bus event. `None` = bus closed.
    fn next_frame(&mut self) -> Option<Vec<u8>> {
        if !self.primed {
            self.primed = true;
            return Some(b": pibuz event stream\n\n".to_vec());
        }
        loop {
            match self.rx.blocking_recv() {
                Ok(ev) => {
                    if let Some(frame) = format_event(&ev) {
                        return Some(frame.into_bytes());
                    }
                    // Not an emitted event — keep waiting.
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Some(format!(": lagged {n} event(s)\n\n").into_bytes());
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Render one CoreEvent as an SSE frame. Every event is pushed; `None` only
/// when it fails to serialize.
fn format_event(ev: &CoreEvent) -> Option<String> {
    let value = serde_json::to_value(ev).ok()?;
    let typ = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("event")
        .to_string();
    let data = serde_json::to_string(&value).ok()?;
    Some(format!("event: {typ}\ndata: {data}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qbz_models::PlaybackState;
    use std::io::Read;

    #[test]
    fn playback_event_becomes_a_typed_sse_frame() {
        let frame = format_event(&CoreEvent::PlaybackStateChanged {
            state: PlaybackState::Playing,
        })
        .expect("playback event is emitted");
        assert!(frame.starts_with("event: PlaybackStateChanged\n"));
        assert!(frame.contains("data: {"));
        assert!(frame.ends_with("\n\n"));
        // The data line carries the tagged CoreEvent JSON.
        assert!(frame.contains("\"type\":\"PlaybackStateChanged\""));
    }

    #[test]
    fn volume_and_queue_events_are_emitted() {
        assert!(format_event(&CoreEvent::VolumeChanged { volume: 0.5 }).is_some());
        assert!(format_event(&CoreEvent::ShuffleChanged { enabled: true }).is_some());
    }

    /// Serve one `/api/events` request on a loopback tiny_http server, the
    /// way `api::serve` does, and return a raw client connected to it.
    fn serve_one(
        rx: broadcast::Receiver<CoreEvent>,
        http_version: &str,
    ) -> (std::net::TcpStream, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind loopback");
        let addr = server.server_addr().to_ip().expect("an IP listener");
        let thread = std::thread::spawn(move || {
            let req = server.recv().expect("one request");
            stream(req, rx);
        });
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .expect("read timeout");
        write!(
            client,
            "GET /api/events HTTP/{http_version}\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n"
        )
        .expect("send request");
        (client, thread)
    }

    /// Read until `want` has arrived, or give up at the deadline. Never a bare
    /// blocking read: a regression here is bytes that never come.
    fn read_until(client: &mut std::net::TcpStream, seen: &mut Vec<u8>, want: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut buf = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            if String::from_utf8_lossy(seen).contains(want) {
                return true;
            }
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(seen).contains(want)
    }

    /// #6, the side note: `/api/events` reached a client in bursts, tens of
    /// seconds apart, and `curl -N` saw nothing at all in 4 s. tiny_http's own
    /// chunked writer holds 8 KB before it sends, and a `PositionUpdated` is
    /// ~100 bytes every 2 s. ONE small event must arrive on its own.
    #[test]
    fn a_single_event_reaches_the_client_without_waiting_for_more() {
        let (tx, rx) = broadcast::channel(16);
        let (mut client, thread) = serve_one(rx, "1.1");
        let mut seen = Vec::new();
        assert!(
            read_until(&mut client, &mut seen, ": pibuz event stream"),
            "the stream opens at once; got {:?}",
            String::from_utf8_lossy(&seen)
        );
        tx.send(CoreEvent::VolumeChanged { volume: 0.5 })
            .expect("a subscriber");
        assert!(
            read_until(&mut client, &mut seen, "event: VolumeChanged"),
            "one event, delivered alone; got {:?}",
            String::from_utf8_lossy(&seen)
        );
        let text = String::from_utf8_lossy(&seen).into_owned();
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.contains("Transfer-Encoding: chunked"), "{text}");
        assert!(text.contains("Content-Type: text/event-stream"), "{text}");

        // The bus closing ends the stream with the terminating chunk, so a
        // client on a kept-alive connection knows the body is over.
        drop(tx);
        assert!(
            read_until(&mut client, &mut seen, "\r\n0\r\n\r\n"),
            "{:?}",
            String::from_utf8_lossy(&seen)
        );
        thread.join().expect("the stream thread ends");
    }

    /// HTTP/1.0 has no chunked encoding: the frames go out bare and the
    /// connection's close ends the body.
    #[test]
    fn an_http_1_0_client_gets_bare_frames() {
        let (tx, rx) = broadcast::channel(16);
        let (mut client, thread) = serve_one(rx, "1.0");
        let mut seen = Vec::new();
        tx.send(CoreEvent::VolumeChanged { volume: 0.5 })
            .expect("a subscriber");
        assert!(
            read_until(&mut client, &mut seen, "event: VolumeChanged"),
            "{:?}",
            String::from_utf8_lossy(&seen)
        );
        let text = String::from_utf8_lossy(&seen).into_owned();
        assert!(!text.contains("Transfer-Encoding"), "{text}");
        assert!(
            text.contains("\r\n\r\n: pibuz event stream\n\nevent: VolumeChanged\n"),
            "{text}"
        );
        drop(tx);
        thread.join().expect("the stream thread ends");
    }
}
