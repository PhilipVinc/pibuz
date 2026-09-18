//! CMAF streaming pipeline for Qobuz.
//!
//! Qobuz's modern mobile client uses CMAF (Common Media Application Format)
//! segmented streaming over Akamai CDN, with AES-CTR per-frame encryption.
//! This is the pipeline that the v9.7.0.3 Android app uses and the one we
//! need to match if we want to stay compatible as Qobuz deprecates the
//! legacy `/track/getFileUrl` nginx path.
//!
//! # Pipeline shape
//!
//! 1. `/file/url` returns `{ url_template, key (wrapped), n_segments, ... }`
//! 2. `/session/start` returns `{ session_id, infos }` — the `infos` string
//!    is the HKDF salt needed to derive the per-session AES key
//! 3. Session key = `HKDF(CMAF_SEED, infos)`
//! 4. Content key = unwrap(session_key, key) — this is the per-track AES key
//! 5. Fetch init segment (s=0) → parse FLAC header + segment table
//! 6. For each s=1..n_segments: fetch → parse crypto boxes → decrypt frames
//!    in place → emit decrypted FLAC frames to the consumer
//!
//! # Why live in `qbz-qobuz` and not `qbz-cmaf`
//!
//! `qbz-cmaf` is pure parsing + crypto primitives (no I/O, no Qobuz client).
//! This module is the Qobuz-specific orchestration: it calls `/file/url`,
//! `/session/start`, owns the Akamai HTTP client, and returns ready-to-play
//! or ready-to-store bundles.
//!
//! # Why two variants
//!
//! - [`download_full`] — returns the fully decrypted FLAC as `Arc<[u8]>`,
//!   assembled directly in the allocation the cache and the audio thread go on
//!   to share (see [`ArcSlab`]). Used by the playback pipeline for in-memory
//!   cache writes and eager downloads.
//! - [`download_raw`] — returns a [`CmafRawBundle`] of **encrypted** segments
//!   plus key material. Used by the offline cache so we can persist
//!   bit-identical bytes to what Qobuz delivered, and decrypt only at
//!   playback time. This is the security-sensitive path.

use std::sync::Arc;

use qbz_models::{Quality, StreamQualityInfo};

use crate::client::QobuzClient;
use crate::error::Result;

/// Concurrency cap for the full-download path. 3 segments in flight is the
/// empirically-determined sweet spot — Akamai CDN rate-limits with 1s windows
/// past ~5 parallel requests per client IP.
pub const CMAF_PREFETCH_CONCURRENCY: usize = 3;

/// Progress callback shape for the download helpers. Each call reports
/// "k of n segments complete" with the bytes received for that segment,
/// so the caller can emit UI progress events without knowing the CMAF
/// internals. Callbacks must be `Send + Sync` because segments are
/// fetched in parallel.
pub type CmafProgressCallback = std::sync::Arc<dyn Fn(CmafProgressUpdate) + Send + Sync>;

/// A single progress tick. `segments_completed` is cumulative (1..=n),
/// `n_segments` is the total including the init segment if you count it.
#[derive(Debug, Clone, Copy)]
pub struct CmafProgressUpdate {
    pub segments_completed: u32,
    pub n_segments: u32,
    pub bytes_this_segment: u64,
}

/// Info gathered from the CMAF init segment, enough to start streaming
/// playback. The caller is expected to fetch audio segments 1..n_segments
/// and feed them through [`qbz_cmaf::parse_segment_crypto`] +
/// [`qbz_cmaf::decrypt_frame`].
pub struct CmafStreamingInfo {
    pub url_template: String,
    pub n_segments: u8,
    pub content_key: [u8; 16],
    pub flac_header: Vec<u8>,
    pub segment_table: Vec<qbz_cmaf::SegmentTableEntry>,
    pub format_id: u32,
    pub sampling_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    /// How long the init segment fetch took (ms), for speed estimation.
    pub init_fetch_ms: u64,
}

/// Raw (encrypted) CMAF bundle suitable for offline storage.
///
/// Everything in this struct is **bit-identical** to what Qobuz's CDN
/// returned. In particular:
///
/// - `init_bytes` is the raw init segment (unencrypted mp4 box with the
///   FLAC header inside — cheap to store).
/// - `segments` are the raw encrypted segment mp4 files, one per
///   `s=1..=n_segments`. These are useless without `content_key` and
///   without running them through the CMAF decrypt pipeline.
/// - `content_key` is the 16-byte AES key unwrapped from the session key;
///   it must be stored **encrypted at rest** on the caller's side.
/// - `infos` is the original `session/start` infos string. With the
///   `CMAF_SEED` constant this is enough to re-derive `session_key` and
///   re-unwrap the content key if we ever need to audit or migrate.
///
/// The intent is that an attacker who copies the user's offline directory
/// out without also extracting the OS-keyring wrapped `content_key` gets
/// nothing usable — the segments are encrypted, the `infos` is just a
/// salt, and the seed alone isn't enough.
pub struct CmafRawBundle {
    pub init_bytes: Vec<u8>,
    pub segments: Vec<Vec<u8>>,
    pub content_key: [u8; 16],
    pub infos: String,
    pub format_id: u32,
    pub sampling_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub n_segments: u8,
}

/// Prepare CMAF streaming: fetch init segment only, derive keys, return info.
/// Does NOT download audio segments -- the caller streams those in background.
pub async fn setup_streaming(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
) -> std::result::Result<CmafStreamingInfo, String> {
    let file_url = client
        .get_file_url(track_id, quality)
        .await
        .map_err(|e| format!("get_file_url failed: {}", e))?;

    let url_template = file_url
        .url_template
        .as_ref()
        .ok_or("No url_template in file/url response")?
        .clone();
    let key_str = file_url.key.as_ref().ok_or("No key in file/url response")?;

    let (_session_id, infos) = client
        .ensure_cmaf_session()
        .await
        .map_err(|e| format!("ensure_cmaf_session failed: {}", e))?;

    let session_key = qbz_cmaf::derive_session_key(crate::auth::CMAF_SEED, &infos)
        .map_err(|e| format!("Session key derivation failed: {}", e))?;
    let content_key = qbz_cmaf::unwrap_content_key(&session_key, key_str)
        .map_err(|e| format!("Content key unwrap failed: {}", e))?;

    // Fetch only the init segment (s=0) -- typically small, <500ms
    let http = build_cdn_client()?;
    let init_url = url_template.replace("$SEGMENT$", "0");
    let init_start = std::time::Instant::now();

    log::info!("[CMAF] Fetching init segment for track {}", track_id);
    let init_data = fetch_bytes_with_retry(http, &init_url, "CMAF init")
        .await
        .map_err(|e| format!("Failed to fetch init segment: {}", e))?;

    let init_fetch_ms = init_start.elapsed().as_millis() as u64;

    let init_info = qbz_cmaf::parse_init_segment(&init_data)
        .map_err(|e| format!("Failed to parse init segment: {}", e))?;

    log::info!(
        "[CMAF] Init for track {}: FLAC header {}B, segment_table={} entries, API n_segments={}, fetched in {}ms",
        track_id,
        init_info.flac_header.len(),
        init_info.segment_table.len(),
        file_url.n_segments,
        init_fetch_ms
    );
    if init_info.segment_table.len() != file_url.n_segments as usize {
        log::warn!(
            "[CMAF] MISMATCH for track {}: segment_table has {} entries but API says n_segments={}",
            track_id,
            init_info.segment_table.len(),
            file_url.n_segments
        );
    }

    let format_id = file_url.format_id.unwrap_or(quality.id());

    Ok(CmafStreamingInfo {
        url_template,
        n_segments: file_url.n_segments,
        content_key,
        flac_header: init_info.flac_header,
        segment_table: init_info.segment_table,
        format_id,
        sampling_rate: file_url.sampling_rate,
        bit_depth: file_url.bits_depth.or(file_url.bit_depth),
        init_fetch_ms,
    })
}

/// Where a downloaded track's bytes should go, decided by the caller at the one
/// moment the real size is known.
pub enum TrackDestination {
    /// Assemble in memory and hand the buffer back.
    Memory,
    /// Write straight to this path as the download proceeds. The caller owns
    /// publishing it (rename into place, index it) once this returns.
    File(std::path::PathBuf),
    /// Do not download this track at all.
    ///
    /// The size is the whole point: a caller that has nowhere to put 200 MB —
    /// a small board with no disk cache — can only find that out here, once the
    /// segment table has been read and before a single audio byte is fetched.
    /// Declining is a normal outcome, not an error, so it comes back as
    /// [`DownloadedTrack::Refused`] rather than as a failure the caller would
    /// log, retry and back off.
    Refuse,
}

/// What [`download_full_sized`] produced.
pub enum DownloadedTrack {
    Memory(std::sync::Arc<[u8]>),
    File(std::path::PathBuf),
    /// The caller's `choose` returned [`TrackDestination::Refuse`]. Nothing was
    /// fetched beyond the init segment and nothing was written.
    Refused,
}

/// Somewhere decrypted frames get appended.
///
/// A trait rather than a bare `&mut Vec<u8>` so a track can be assembled
/// DIRECTLY into the allocation it will finally be shared from. See [`ArcSlab`]
/// for why that is worth a trait.
pub trait TrackSink {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Append `src`, or fail if there is no room. Fallible because a fixed-size
    /// sink has a real end: a segment table that disagrees with the bytes that
    /// arrive must surface as an error, never as a silently truncated track.
    fn append(&mut self, src: &[u8]) -> std::result::Result<(), String>;
    /// The bytes from `from` to the current end, for decrypting a frame in
    /// place immediately after it was appended.
    fn tail_from(&mut self, from: usize) -> &mut [u8];
}

impl TrackSink for Vec<u8> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn append(&mut self, src: &[u8]) -> std::result::Result<(), String> {
        self.extend_from_slice(src);
        Ok(())
    }
    fn tail_from(&mut self, from: usize) -> &mut [u8] {
        &mut self[from..]
    }
}

/// A track assembled straight into the `Arc` it will be shared from.
///
/// # Why this exists
///
/// `TrackBytes` is `Arc<[u8]>` so that the cache, the audio thread and the
/// decoder share ONE copy of a track instead of holding three. The catch is at
/// the seam: an `Arc` keeps its refcount immediately BEFORE the bytes, and a
/// `Vec`'s allocation has no room for that header, so `Arc::from(vec)` cannot
/// adopt the buffer — it allocates again and copies. Building a 60-170 MB track
/// into a `Vec` and then handing it to the cache therefore holds BOTH copies for
/// the length of the copy: 240 MB+ at Hi-Res, on boards with 512-905 MB of RAM,
/// while the track now playing is also resident.
///
/// The size is known before any audio is fetched — the init segment's table
/// gives the exact decrypted byte count — so there is no reason to grow into a
/// `Vec` first. Fill the final allocation directly and the second copy never
/// exists.
pub struct ArcSlab {
    bytes: std::sync::Arc<[u8]>,
    len: usize,
}

impl ArcSlab {
    /// A slab of exactly `capacity` bytes, ready to be filled.
    pub fn with_capacity(capacity: usize) -> Self {
        // ZEROED rather than uninitialised on purpose: every byte is then a
        // valid `u8` the moment the allocation exists, so `assume_init` is
        // sound however much of the slab the download goes on to write, and a
        // short download is caught by `into_arc` rather than by UB. For a
        // track-sized block the zeroing is close to free — the allocator serves
        // it from fresh zero pages it never has to touch.
        let bytes = unsafe { std::sync::Arc::new_zeroed_slice(capacity).assume_init() };
        Self { bytes, len: 0 }
    }

    fn writable(&mut self) -> &mut [u8] {
        std::sync::Arc::get_mut(&mut self.bytes)
            .expect("the slab is never shared while it is being filled")
    }

    /// The bytes written so far. The slab is sized from the segment table, so
    /// in production this is the whole thing by the time anyone asks; it is
    /// separate from [`Self::into_arc`] for tests that fill a generous slab and
    /// only care about what came out.
    pub fn filled(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The finished track, or an error when the bytes that arrived disagree
    /// with the segment table that sized the slab.
    ///
    /// Refusing a short track is the same discipline the straight-to-disk path
    /// applies: whatever comes out of here is treated as a complete track for
    /// ever after, and a short one decodes as garbage on every later play.
    pub fn into_arc(self) -> std::result::Result<std::sync::Arc<[u8]>, String> {
        if self.len != self.bytes.len() {
            return Err(format!(
                "assembled {} bytes, segment table said {} — refusing a short track",
                self.len,
                self.bytes.len()
            ));
        }
        Ok(self.bytes)
    }
}

impl TrackSink for ArcSlab {
    fn len(&self) -> usize {
        self.len
    }
    fn append(&mut self, src: &[u8]) -> std::result::Result<(), String> {
        let start = self.len;
        let end = start + src.len();
        if end > self.bytes.len() {
            return Err(format!(
                "segment data overruns the slab: {end} bytes into a {}-byte track",
                self.bytes.len()
            ));
        }
        self.writable()[start..end].copy_from_slice(src);
        self.len = end;
        Ok(())
    }
    fn tail_from(&mut self, from: usize) -> &mut [u8] {
        let end = self.len;
        &mut self.writable()[from..end]
    }
}

/// Download a track, letting the caller choose in-memory or straight-to-disk
/// ONCE THE SIZE IS KNOWN — before a single audio segment is fetched.
///
/// `choose` is called with the exact decrypted byte count, which the init
/// segment's table gives up front. That is the whole point: a caller can only
/// make a sensible decision about a 195 MB track if it learns the size BEFORE
/// the bytes exist, and until now nothing exposed it.
///
/// The old shape — always assemble the whole track in memory, then let the
/// caller write it out if it turned out to be too big — cost, on a Pi:
///   * a 195 MB `Vec` that `Arc::from` then COPIED to share, and
///   * a single blocking, fsync'd write of the whole track afterwards, 11-15 s
///     with the card pinned, which underran ALSA and was audible (twice,
///     measured, mid-write).
///     Writing as the segments decrypt spreads the same bytes across the ~40 s the
///     download already takes, so neither the copy nor the burst exists.
pub async fn download_full_sized(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
    on_progress: Option<CmafProgressCallback>,
    choose: impl FnOnce(usize) -> TrackDestination,
) -> std::result::Result<DownloadedTrack, String> {
    let setup = setup_streaming(client, track_id, quality).await?;
    let http = build_cdn_client()?;

    let total_size: usize = setup.flac_header.len()
        + setup
            .segment_table
            .iter()
            .map(|s| s.byte_len as usize)
            .sum::<usize>();

    match choose(total_size) {
        TrackDestination::Refuse => {
            log::info!(
                "[CMAF] Track {} declined at {:.2} MB — the caller has nowhere to put it",
                track_id,
                total_size as f64 / (1024.0 * 1024.0),
            );
            Ok(DownloadedTrack::Refused)
        }
        TrackDestination::Memory => {
            let mut output = ArcSlab::with_capacity(total_size);
            output.append(&setup.flac_header)?;
            fetch_decrypt_in_order(
                http,
                &setup.url_template,
                setup.n_segments,
                "CMAF-FULL",
                on_progress,
                &setup.content_key,
                &mut Sink::Memory(&mut output),
            )
            .await?;
            log::info!(
                "[CMAF-FULL] Track {} complete in memory: {:.2} MB FLAC, expected {:.2} MB",
                track_id,
                output.len() as f64 / (1024.0 * 1024.0),
                total_size as f64 / (1024.0 * 1024.0),
            );
            Ok(DownloadedTrack::Memory(output.into_arc()?))
        }
        TrackDestination::File(path) => {
            // The writer lives on a BLOCKING thread with a bounded channel in
            // front of it, for two reasons.
            //
            // It must not run here: this is an async task, and a write plus its
            // writeback is hundreds of milliseconds of a tokio worker — the same
            // workers the qconnect socket and the report loop run on.
            //
            // And the channel is the throttle. Two segments deep, so when the
            // card falls behind the download AWAITS instead of racing ahead and
            // piling up dirty pages. That is what keeps a big track from ever
            // saturating the card: the network paces the writes, and if the card
            // is slower than the network, the card paces the download.
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
            let writer_path = path.clone();
            let header = setup.flac_header.clone();
            let writer =
                tokio::task::spawn_blocking(move || write_track_to_disk(&writer_path, header, rx));

            let fetch = fetch_decrypt_in_order(
                http,
                &setup.url_template,
                setup.n_segments,
                "CMAF-DISK",
                on_progress,
                &setup.content_key,
                &mut Sink::File { tx },
            )
            .await;

            // Dropping the sink closed the channel, so the writer finishes and
            // reports what it managed to write. Join it either way: on a fetch
            // error it still has to clean up.
            let written = match writer.await {
                Ok(Ok(written)) => written,
                Ok(Err(e)) => {
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("disk writer task failed: {e}"));
                }
            };
            if let Err(e) = fetch {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }

            // The caller is about to RENAME this into the cache under the real
            // name, and whatever sits under that name is treated as a complete
            // track forever after. A short file would decode as garbage on every
            // later play -- precisely the corruption the temp-and-rename pattern
            // exists to prevent -- so refuse rather than publish one.
            if written != total_size {
                let _ = std::fs::remove_file(&path);
                return Err(format!(
                    "CMAF-DISK track {track_id}: wrote {written} bytes, segment table said \
                     {total_size} -- refusing to publish a short file"
                ));
            }

            log::info!(
                "[CMAF-DISK] Track {} written straight to disk: {:.2} MB FLAC, expected {:.2} MB",
                track_id,
                written as f64 / (1024.0 * 1024.0),
                total_size as f64 / (1024.0 * 1024.0),
            );
            Ok(DownloadedTrack::File(path))
        }
    }
}

/// Write a track to `path` from `rx`, pacing the card.
///
/// Runs on a blocking thread. Per chunk:
///
/// 1. write and flush it into the page cache,
/// 2. `sync_file_range(WRITE)` on it — asks the kernel to START writing it out
///    and returns IMMEDIATELY, so the card is fed steadily and nothing here ever
///    waits on it,
/// 3. for the PREVIOUS chunk, `sync_file_range(WAIT_BEFORE|WRITE|WAIT_AFTER)`
///    then `posix_fadvise(DONTNEED)`. The wait is bounded to one chunk and by
///    then step 2 has usually finished it, so it rarely blocks — and dropping
///    the pages afterwards stops a write-once track from evicting the PLAYING
///    track's cache, which on this host is being read off the same card.
///
/// The alternative — let dirty pages accumulate and fsync at the end — was
/// measured putting three audible ALSA underruns into the final 1.4 s of a
/// 37.6 s write, the card monopolised while the decoder needed it.
///
/// Non-Linux has neither syscall; there the flush alone is the whole of it.
fn write_track_to_disk(
    path: &std::path::Path,
    header: Vec<u8>,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> std::result::Result<usize, String> {
    use std::io::Write;

    let file = std::fs::File::create(path)
        .map_err(|e| format!("create {} for streaming download: {e}", path.display()))?;
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);

    writer
        .write_all(&header)
        .map_err(|e| format!("write FLAC header: {e}"))?;
    let mut written = header.len();
    // The range handed to the kernel last time, still on its way out.
    let mut in_flight: Option<(usize, usize)> = None;

    while let Some(chunk) = rx.blocking_recv() {
        let start = written;
        writer
            .write_all(&chunk)
            .map_err(|e| format!("write to {}: {e}", path.display()))?;
        written += chunk.len();
        writer
            .flush()
            .map_err(|e| format!("flush {}: {e}", path.display()))?;

        start_writeback(&writer, start, written - start);
        if let Some((off, len)) = in_flight.replace((start, written - start)) {
            finish_writeback(&writer, off, len);
        }
    }

    if let Some((off, len)) = in_flight {
        finish_writeback(&writer, off, len);
    }
    let file = writer
        .into_inner()
        .map_err(|e| format!("flush {}: {e}", path.display()))?;
    // Cheap by now: the ranges above are already out, so this only settles the
    // tail. `sync_data`, not `sync_all` — the length is the only metadata that
    // matters and the rename publishes it.
    file.sync_data()
        .map_err(|e| format!("sync {}: {e}", path.display()))?;
    Ok(written)
}

/// Ask the kernel to begin writing `[offset, offset+len)` out. Returns at once.
#[cfg(target_os = "linux")]
fn start_writeback(writer: &std::io::BufWriter<std::fs::File>, offset: usize, len: usize) {
    use std::os::unix::io::AsRawFd;
    if len == 0 {
        return;
    }
    // Best effort throughout: a kernel or filesystem that refuses these leaves
    // the write correct, just less considerate.
    unsafe {
        libc::sync_file_range(
            writer.get_ref().as_raw_fd(),
            offset as libc::off64_t,
            len as libc::off64_t,
            libc::SYNC_FILE_RANGE_WRITE,
        );
    }
}

/// Wait for `[offset, offset+len)` to be out, then drop it from the page cache.
#[cfg(target_os = "linux")]
fn finish_writeback(writer: &std::io::BufWriter<std::fs::File>, offset: usize, len: usize) {
    use std::os::unix::io::AsRawFd;
    if len == 0 {
        return;
    }
    let fd = writer.get_ref().as_raw_fd();
    unsafe {
        libc::sync_file_range(
            fd,
            offset as libc::off64_t,
            len as libc::off64_t,
            libc::SYNC_FILE_RANGE_WAIT_BEFORE
                | libc::SYNC_FILE_RANGE_WRITE
                | libc::SYNC_FILE_RANGE_WAIT_AFTER,
        );
        // Only meaningful once the pages are clean, which is why it follows the
        // wait: the kernel will not drop dirty pages.
        libc::posix_fadvise(
            fd,
            offset as libc::off_t,
            len as libc::off_t,
            libc::POSIX_FADV_DONTNEED,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn start_writeback(_writer: &std::io::BufWriter<std::fs::File>, _offset: usize, _len: usize) {}

#[cfg(not(target_os = "linux"))]
fn finish_writeback(_writer: &std::io::BufWriter<std::fs::File>, _offset: usize, _len: usize) {}

/// Where [`fetch_decrypt_in_order`] puts each decrypted segment.
enum Sink<'a> {
    Memory(&'a mut ArcSlab),
    /// Hands segments to the blocking writer. Bounded, so a card slower than
    /// the network throttles the download rather than piling up dirty pages.
    File {
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    },
}

impl Sink<'_> {
    /// Async because the file variant AWAITS the writer's channel — that await
    /// is the backpressure, and doing it any other way would either block a
    /// tokio worker or let the download outrun the disk.
    async fn append_segment(
        &mut self,
        seg_data: &[u8],
        seg_number: usize,
        content_key: &[u8; 16],
    ) -> std::result::Result<(), String> {
        match self {
            // Decrypts straight into the output; no intermediate copy.
            Sink::Memory(out) => decrypt_segment_into(seg_data, seg_number, content_key, *out),
            Sink::File { tx } => {
                // One buffer per segment, handed to the writer and freed there.
                // A shared scratch would have to be copied out to send anyway.
                let mut chunk = Vec::new();
                decrypt_segment_into(seg_data, seg_number, content_key, &mut chunk)?;
                tx.send(chunk)
                    .await
                    .map_err(|_| format!("disk writer stopped before segment {seg_number}"))
            }
        }
    }
}

/// Download a track's complete CMAF stream and return decrypted FLAC bytes.
///
/// Segments are fetched concurrently with a semaphore cap, decrypted, and
/// concatenated.
///
/// Prefer [`download_full_sized`] on any path that runs on a player: this
/// returns the WHOLE track as an `Arc<[u8]>` with no say in the matter, which
/// is ~210 MB at 24/192 and an OOM on a 512 MB board. `download_full_sized`
/// hands the caller the size before the bytes exist so it can send an oversized
/// track to disk, or decline it.
pub async fn download_full(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
) -> std::result::Result<Arc<[u8]>, String> {
    download_full_with_progress(client, track_id, quality, None).await
}

/// Same as [`download_full`] but with a progress callback fired once per
/// completed segment.
pub async fn download_full_with_progress(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
    on_progress: Option<CmafProgressCallback>,
) -> std::result::Result<Arc<[u8]>, String> {
    download_full_with_quality_progress(client, track_id, quality, on_progress)
        .await
        .map(|(bytes, _quality)| bytes)
}

/// Like [`download_full`] but also returns the quality actually resolved from
/// the CMAF init segment (`format_id` / `sampling_rate` / `bit_depth`). Used
/// by the external-stream (Cast / DLNA) path, which must surface the real
/// delivered quality. The CMAF path always yields decrypted FLAC, so the
/// caller's content type is `audio/flac`.
pub async fn download_full_with_quality(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
) -> std::result::Result<(Arc<[u8]>, StreamQualityInfo), String> {
    download_full_with_quality_progress(client, track_id, quality, None).await
}

/// [`download_full_with_quality`] + a per-segment progress callback.
pub async fn download_full_with_quality_progress(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
    on_progress: Option<CmafProgressCallback>,
) -> std::result::Result<(Arc<[u8]>, StreamQualityInfo), String> {
    let setup = setup_streaming(client, track_id, quality).await?;
    let http = build_cdn_client()?;

    let total_size: usize = setup.flac_header.len()
        + setup
            .segment_table
            .iter()
            .map(|s| s.byte_len as usize)
            .sum::<usize>();

    // Pre-sized so the decrypted track never reallocates, and filled segment by
    // segment so the encrypted copy is never resident as a whole.
    let mut output = ArcSlab::with_capacity(total_size);
    output.append(&setup.flac_header)?;
    fetch_decrypt_in_order(
        http,
        &setup.url_template,
        setup.n_segments,
        "CMAF-FULL",
        on_progress,
        &setup.content_key,
        &mut Sink::Memory(&mut output),
    )
    .await?;

    log::info!(
        "[CMAF-FULL] Track {} complete: {:.2} MB FLAC, expected {:.2} MB",
        track_id,
        output.len() as f64 / (1024.0 * 1024.0),
        total_size as f64 / (1024.0 * 1024.0),
    );

    // `from_raw` normalizes the rate unit (kHz vs Hz) defensively.
    let quality_info = StreamQualityInfo::from_raw(
        setup.format_id,
        setup.sampling_rate.map(|v| v as f64),
        setup.bit_depth,
    );
    Ok((output.into_arc()?, quality_info))
}

/// Download a track's complete CMAF stream and return it as a raw (still
/// encrypted) bundle suitable for offline storage.
///
/// The caller is responsible for:
/// 1. Persisting `init_bytes` + `segments` to disk as bit-identical blobs
/// 2. Wrapping `content_key` with a device-bound key before storing it
/// 3. Storing `infos` (either wrapped or as plaintext — it's only a salt,
///    useless without `CMAF_SEED` + `content_key`)
///
/// At playback time, the caller feeds `init_bytes` through
/// [`qbz_cmaf::parse_init_segment`] to recover the FLAC header + segment
/// table, then decrypts each segment with the unwrapped content key.
pub async fn download_raw(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
) -> std::result::Result<CmafRawBundle, String> {
    download_raw_with_progress(client, track_id, quality, None).await
}

/// Same as [`download_raw`] but with a progress callback fired once per
/// completed audio segment. The init segment doesn't count toward progress
/// — it's downloaded up front and is typically tiny (<1% of total bytes).
pub async fn download_raw_with_progress(
    client: &QobuzClient,
    track_id: u64,
    quality: Quality,
    on_progress: Option<CmafProgressCallback>,
) -> std::result::Result<CmafRawBundle, String> {
    let file_url = client
        .get_file_url(track_id, quality)
        .await
        .map_err(|e| format!("get_file_url failed: {}", e))?;

    let url_template = file_url
        .url_template
        .as_ref()
        .ok_or("No url_template in file/url response")?
        .clone();
    let key_str = file_url.key.as_ref().ok_or("No key in file/url response")?;

    let (_session_id, infos) = client
        .ensure_cmaf_session()
        .await
        .map_err(|e| format!("ensure_cmaf_session failed: {}", e))?;

    let session_key = qbz_cmaf::derive_session_key(crate::auth::CMAF_SEED, &infos)
        .map_err(|e| format!("Session key derivation failed: {}", e))?;
    let content_key = qbz_cmaf::unwrap_content_key(&session_key, key_str)
        .map_err(|e| format!("Content key unwrap failed: {}", e))?;

    let http = build_cdn_client()?;

    // Init segment — used for FLAC header + segment table at playback
    let init_url = url_template.replace("$SEGMENT$", "0");
    log::info!("[CMAF-RAW] Fetching init for track {}", track_id);
    let init_bytes = http
        .get(&init_url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch init segment: {}", e))?
        .bytes()
        .await
        .map_err(|e| format!("Failed to read init segment: {}", e))?
        .to_vec();

    // Audio segments — encrypted, stored as-is
    let segments = fetch_all_segments(
        http,
        &url_template,
        file_url.n_segments,
        "CMAF-RAW",
        on_progress,
    )
    .await?;

    log::info!(
        "[CMAF-RAW] Track {} bundle: init={}B, {} encrypted segments, total raw size={} bytes",
        track_id,
        init_bytes.len(),
        segments.len(),
        init_bytes.len() + segments.iter().map(|s| s.len()).sum::<usize>(),
    );

    Ok(CmafRawBundle {
        init_bytes,
        segments,
        content_key,
        infos,
        format_id: file_url.format_id.unwrap_or(quality.id()),
        sampling_rate: file_url.sampling_rate,
        bit_depth: file_url.bits_depth.or(file_url.bit_depth),
        n_segments: file_url.n_segments,
    })
}

/// The shared Akamai CDN client (`crate::cdn`).
///
/// This used to BUILD a client, and every caller below built its own — four
/// sites, so `download_full` alone made two per track (one here, one inside
/// `setup_streaming`). They all share one now: warm connection pool, warm TLS
/// session cache, roots parsed once. See `cdn` for the timeout contract.
///
/// Uses the workspace reqwest feature set (rustls-tls): smaller binary, no
/// system SSL dependency. If Akamai ever surfaces a cert issue, adding the
/// `native-tls` feature to qbz-qobuz is the escape hatch.
fn build_cdn_client() -> std::result::Result<&'static reqwest::Client, String> {
    crate::cdn::client()
}

/// Fetch a CDN URL into bytes, retrying transient failures (network blips,
/// 5xx, 429) with exponential backoff. A terminal status (404/403) fails
/// immediately. Without this a single transient segment failure aborted the
/// whole track download and the frontend skipped it — issue #467.
async fn fetch_bytes_with_retry(
    http: &reqwest::Client,
    url: &str,
    log_tag: &str,
) -> std::result::Result<Vec<u8>, String> {
    use crate::retry::{
        classify_reqwest, classify_status, retry_transient, FetchError, DEFAULT_MAX_ATTEMPTS,
    };
    retry_transient(
        DEFAULT_MAX_ATTEMPTS,
        log_tag,
        FetchError::is_transient,
        |_attempt| async move {
            let response = http
                .get(url)
                .header("User-Agent", "Mozilla/5.0")
                .send()
                .await
                .map_err(|e| classify_reqwest(&e, "fetch"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(classify_status(status, "fetch"));
            }
            response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| classify_reqwest(&e, "read"))
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Fetch segments 1..=n_segments concurrently with a semaphore cap and a
/// cooldown per slot to stay under CDN rate limits.
///
/// If `on_progress` is `Some`, it's invoked once per completed segment
/// (not per HTTP chunk — the cooldown happens on the worker, not here).
/// Callbacks fire in completion order, not segment order.
async fn fetch_all_segments(
    http: &reqwest::Client,
    url_template: &str,
    n_segments: u8,
    log_tag: &str,
    on_progress: Option<CmafProgressCallback>,
) -> std::result::Result<Vec<Vec<u8>>, String> {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(CMAF_PREFETCH_CONCURRENCY));
    let seg_indices: Vec<u8> = (1..=n_segments).collect();
    let mut handles = Vec::with_capacity(seg_indices.len());

    let completed_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

    for seg_idx in seg_indices {
        let sem = semaphore.clone();
        let http = http.clone();
        let seg_url = url_template.replace("$SEGMENT$", &seg_idx.to_string());
        let log_tag = log_tag.to_string();
        let progress = on_progress.clone();
        let counter = completed_count.clone();

        handles.push(tokio::spawn(async move {
            let permit = sem
                .acquire_owned()
                .await
                .map_err(|e| format!("semaphore: {}", e))?;
            let seg_data =
                fetch_bytes_with_retry(&http, &seg_url, &format!("{} seg {}", log_tag, seg_idx))
                    .await
                    .map_err(|e| format!("[{}] seg {} fetch: {}", log_tag, seg_idx, e))?;
            let bytes_this_segment = seg_data.len() as u64;
            if let Some(cb) = progress {
                let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                cb(CmafProgressUpdate {
                    segments_completed: done,
                    n_segments: n_segments as u32,
                    bytes_this_segment,
                });
            }
            // Cooldown before releasing the slot — keeps requests spaced out
            // to stay under CDN rate limits (most use 1s windows)
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            drop(permit);
            Ok::<(u8, Vec<u8>), String>((seg_idx, seg_data))
        }));
    }

    // Collect results in arrival order, then re-sort by segment index
    let mut segments: Vec<(u8, Vec<u8>)> = Vec::with_capacity(handles.len());
    for handle in handles {
        let (idx, data) = handle
            .await
            .map_err(|e| format!("[{}] task panic: {}", log_tag, e))?
            .map_err(|e| format!("[{}] download failed: {}", log_tag, e))?;
        segments.push((idx, data));
    }
    segments.sort_by_key(|(idx, _)| *idx);
    Ok(segments.into_iter().map(|(_, data)| data).collect())
}

/// Fetch every segment and decrypt each ONE AT A TIME into `output`, holding
/// only a small window of encrypted bytes at once.
///
/// This exists because [`fetch_all_segments`] returns `Vec<Vec<u8>>` — the
/// whole track, encrypted — and the full-download path then decrypted it into
/// a second whole-track buffer. Both were live at the same instant, so
/// prefetching a track cost TWICE its size in memory, and half of that was an
/// encrypted copy about to be thrown away. On a 1 GB player, prefetching a
/// 69 MB track was measured spiking RSS by 111 MB at exactly the moment the
/// download completed; on a 512 MB board the same doubling is what made
/// prefetching a Hi-Res track impossible at all.
///
/// Segments arrive concurrently but are consumed IN ORDER, decrypted, and
/// dropped, so the peak is `output` plus at most `PIPELINE_DEPTH` segments.
/// A CMAF segment is a fixed ~10 s of audio — under 1 MB at CD quality, a few
/// MB at Hi-Res — so the window is single-digit megabytes against a whole
/// second track.
///
/// [`fetch_all_segments`] stays: `download_raw` genuinely needs every segment
/// at once, because it stores them encrypted for offline playback.
async fn fetch_decrypt_in_order(
    http: &reqwest::Client,
    url_template: &str,
    n_segments: u8,
    log_tag: &str,
    on_progress: Option<CmafProgressCallback>,
    content_key: &[u8; 16],
    sink: &mut Sink<'_>,
) -> std::result::Result<(), String> {
    use futures_util::StreamExt;

    // Deeper than the fetch concurrency ON PURPOSE. `buffered` yields in order,
    // so a slow segment at the head would otherwise stall the whole pipeline:
    // with a window equal to the concurrency, nothing new could start until the
    // head landed. Twice the depth keeps requests always available to the
    // semaphore while still bounding what is held to a handful of segments.
    let pipeline_depth = CMAF_PREFETCH_CONCURRENCY * 2;

    let semaphore = Arc::new(tokio::sync::Semaphore::new(CMAF_PREFETCH_CONCURRENCY));
    let completed_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

    let mut stream = futures_util::stream::iter(1..=n_segments)
        .map(|seg_idx| {
            let sem = semaphore.clone();
            let http = http.clone();
            let seg_url = url_template.replace("$SEGMENT$", &seg_idx.to_string());
            let log_tag = log_tag.to_string();
            let progress = on_progress.clone();
            let counter = completed_count.clone();
            async move {
                let permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| format!("semaphore: {}", e))?;
                let seg_data = fetch_bytes_with_retry(
                    &http,
                    &seg_url,
                    &format!("{} seg {}", log_tag, seg_idx),
                )
                .await
                .map_err(|e| format!("[{}] seg {} fetch: {}", log_tag, seg_idx, e))?;
                let bytes_this_segment = seg_data.len() as u64;
                if let Some(cb) = progress {
                    let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    cb(CmafProgressUpdate {
                        segments_completed: done,
                        n_segments: n_segments as u32,
                        bytes_this_segment,
                    });
                }
                // Cooldown before releasing the slot — keeps requests spaced
                // out to stay under CDN rate limits (most use 1s windows).
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                drop(permit);
                Ok::<(u8, Vec<u8>), String>((seg_idx, seg_data))
            }
        })
        .buffered(pipeline_depth);

    let mut expected: u8 = 1;
    while let Some(result) = stream.next().await {
        let (seg_idx, seg_data) = result?;
        // `buffered` is ordered; assert it rather than trust it, because
        // decrypting out of order would produce silent audio corruption
        // rather than an error.
        if seg_idx != expected {
            return Err(format!(
                "[{}] segments arrived out of order: expected {}, got {}",
                log_tag, expected, seg_idx
            ));
        }
        sink.append_segment(&seg_data, seg_idx as usize, content_key)
            .await?;
        expected = expected.saturating_add(1);
        // seg_data drops here — this is the whole point.
    }
    Ok(())
}

/// Decrypt a sequence of encrypted CMAF segments in order and append the
/// decrypted frames to `output`.
///
/// This is the common decryption logic shared between the full-download
/// path (decrypt-then-return) and the offline playback path (decrypt-from-
/// disk-then-feed-player).
///
/// Hot-path note: the previous implementation allocated a `Vec<u8>` per
/// frame, copied the encrypted bytes into it, decrypted in place, then
/// copied again into `output` via `extend_from_slice`. For a HiRes FLAC
/// this is tens of thousands of small heap allocations + double copies
/// per track. Now we extend `output` with the encrypted bytes directly
/// and decrypt the just-appended slice in place — one copy instead of
/// three, zero per-frame allocations. Combined with AES-NI codegen
/// (enabled via `target-cpu=x86-64-v3` in `.cargo/config.toml`) this
/// is the difference between a 20-second offline-cache gap on track
/// transitions and a sub-second one.
pub fn decrypt_segments_into(
    segments: &[Vec<u8>],
    content_key: &[u8; 16],
    output: &mut impl TrackSink,
) -> std::result::Result<(), String> {
    for (seg_idx, seg_data) in segments.iter().enumerate() {
        // seg_idx is 0-based here but the original segment number is idx+1
        decrypt_segment_into(seg_data, seg_idx + 1, content_key, output)?;
    }
    Ok(())
}

/// Decrypt ONE encrypted CMAF segment and append its frames to `output`.
///
/// The body of [`decrypt_segments_into`], split out so a caller that consumes
/// segments as they arrive can decrypt each one and drop it, instead of holding
/// the whole track twice (see [`fetch_decrypt_in_order`]). `seg_number` is
/// 1-based and only used in error messages.
pub fn decrypt_segment_into(
    seg_data: &[u8],
    seg_number: usize,
    content_key: &[u8; 16],
    output: &mut impl TrackSink,
) -> std::result::Result<(), String> {
    let crypto = qbz_cmaf::parse_segment_crypto(seg_data)
        .map_err(|e| format!("CMAF seg {} parse: {}", seg_number, e))?;

    let mut data_pos = crypto.data_offset;
    for entry in &crypto.entries {
        let frame_end = data_pos + entry.size as usize;
        if frame_end > seg_data.len() {
            return Err(format!("CMAF seg {} frame overflow", seg_number));
        }
        let output_start = output.len();
        output.append(&seg_data[data_pos..frame_end])?;
        if entry.flags != 0 {
            qbz_cmaf::decrypt_frame(content_key, &entry.iv, output.tail_from(output_start));
        }
        data_pos = frame_end;
    }
    if data_pos < crypto.mdat_end && crypto.mdat_end <= seg_data.len() {
        output.append(&seg_data[data_pos..crypto.mdat_end])?;
    }
    Ok(())
}

// Silence "unused imports" if we end up not using everything at some point;
// the Result alias is kept for future variants that want to surface ApiError.
#[allow(dead_code)]
fn _type_assertions() {
    let _: fn() -> Result<()> = || Ok(());
}

#[cfg(test)]
mod segment_assembly_tests {
    use super::{decrypt_segment_into, decrypt_segments_into};

    /// Bytes of the QBZ segment UUID box (`qbz_cmaf::parser::QBZ_SEGMENT_UUID`,
    /// which is private there). If the parser ever stops recognising this, the
    /// fixture builder below returns segments that fail to parse and these
    /// tests fail loudly rather than silently passing on empty input.
    const SEGMENT_UUID: [u8; 16] = [
        0x3b, 0x42, 0x12, 0x92, 0x56, 0xf3, 0x5f, 0x75, 0x92, 0x36, 0x63, 0xb6, 0x9a, 0x1f, 0x52,
        0xb2,
    ];

    /// Build one syntactically valid encrypted CMAF segment: a `uuid` box
    /// carrying the frame table, followed by an `mdat` box carrying the audio.
    /// `frames` is (payload, encrypted); a tail of `trailing` unencrypted bytes
    /// after the last frame exercises the mdat-remainder path.
    fn segment(frames: &[(Vec<u8>, bool)], trailing: usize) -> Vec<u8> {
        const IV_SIZE: usize = 8;
        let mut uuid_payload = Vec::new();
        uuid_payload.extend_from_slice(&[0u8; 4]); // version/padding
        let data_offset_pos = uuid_payload.len();
        uuid_payload.extend_from_slice(&[0u8; 4]); // data_offset_raw, patched below
        uuid_payload.push(IV_SIZE as u8);
        let n = frames.len();
        uuid_payload.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        for (i, (payload, encrypted)) in frames.iter().enumerate() {
            uuid_payload.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            uuid_payload.extend_from_slice(&[0u8; 2]); // skip
            uuid_payload.extend_from_slice(&(if *encrypted { 1u16 } else { 0u16 }).to_be_bytes());
            uuid_payload.extend_from_slice(&[i as u8 + 1; IV_SIZE]);
        }

        let uuid_box_len = 8 + 16 + uuid_payload.len();
        // data_offset is measured from the START of the uuid box, and the audio
        // begins at the mdat payload, which follows the whole uuid box.
        let data_offset_raw = (uuid_box_len + 8) as u32;
        uuid_payload[data_offset_pos..data_offset_pos + 4]
            .copy_from_slice(&data_offset_raw.to_be_bytes());

        let mut mdat_payload: Vec<u8> = Vec::new();
        for (payload, _) in frames {
            mdat_payload.extend_from_slice(payload);
        }
        mdat_payload.extend(std::iter::repeat_n(0xABu8, trailing));

        let mut out = Vec::new();
        out.extend_from_slice(&(uuid_box_len as u32).to_be_bytes());
        out.extend_from_slice(b"uuid");
        out.extend_from_slice(&SEGMENT_UUID);
        out.extend_from_slice(&uuid_payload);
        out.extend_from_slice(&((mdat_payload.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(&mdat_payload);
        out
    }

    fn fixture() -> Vec<Vec<u8>> {
        vec![
            segment(&[(vec![1u8; 64], true), (vec![2u8; 48], false)], 0),
            segment(&[(vec![3u8; 96], true)], 7),
            segment(
                &[
                    (vec![4u8; 32], false),
                    (vec![5u8; 32], true),
                    (vec![6u8; 16], true),
                ],
                3,
            ),
        ]
    }

    /// The property the streaming prefetch rests on: decrypting segments ONE AT
    /// A TIME, dropping each as you go, must produce byte-for-byte what
    /// decrypting the whole batch produced. Anything else would be silent audio
    /// corruption, not an error.
    #[test]
    fn one_at_a_time_matches_the_whole_batch() {
        let key = [7u8; 16];
        let segments = fixture();

        let mut batch = Vec::new();
        decrypt_segments_into(&segments, &key, &mut batch).expect("batch");

        let mut streamed = Vec::new();
        for (i, seg) in segments.iter().enumerate() {
            decrypt_segment_into(seg, i + 1, &key, &mut streamed).expect("one at a time");
        }

        assert!(
            !batch.is_empty(),
            "fixture produced no audio — parser drift?"
        );
        assert_eq!(batch, streamed);
    }

    /// Guards the fixture itself: if the builder stopped producing parseable
    /// segments, the test above would compare two empty buffers and pass.
    #[test]
    fn the_fixture_actually_carries_every_frame() {
        let key = [7u8; 16];
        let mut out = Vec::new();
        decrypt_segments_into(&fixture(), &key, &mut out).expect("decrypt");
        // 64+48 + 96+7 + 32+32+16+3 = 298 bytes of frames and trailing audio.
        assert_eq!(out.len(), 298);
    }

    /// The property this whole refactor rests on: a track written STRAIGHT TO
    /// DISK as its segments decrypt must be byte-for-byte what assembling it in
    /// memory produced. If these ever diverge, big tracks silently decode as
    /// something other than what small ones do.
    ///
    /// Goes through the real machinery — the bounded channel and
    /// `write_track_to_disk`, writeback calls and all — not a stand-in.
    #[tokio::test]
    async fn the_disk_sink_and_the_memory_sink_agree() {
        use super::{write_track_to_disk, Sink};

        let key = [7u8; 16];
        let segments = fixture();

        // Sized generously: this test is about the two sinks AGREEING, not about
        // the segment table, so it fills what it fills and compares that.
        let mut in_memory = super::ArcSlab::with_capacity(64 * 1024);
        {
            let mut sink = Sink::Memory(&mut in_memory);
            for (i, seg) in segments.iter().enumerate() {
                sink.append_segment(seg, i + 1, &key)
                    .await
                    .expect("memory sink");
            }
        }

        let dir = std::env::temp_dir().join(format!("qbz-sink-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("track.part");

        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let writer_path = path.clone();
        // No FLAC header: the comparison is over segment payloads alone.
        let writer =
            tokio::task::spawn_blocking(move || write_track_to_disk(&writer_path, Vec::new(), rx));
        {
            let mut sink = Sink::File { tx };
            for (i, seg) in segments.iter().enumerate() {
                sink.append_segment(seg, i + 1, &key)
                    .await
                    .expect("file sink");
            }
            // Dropping the sink closes the channel, which is how the writer
            // learns the track is finished.
        }
        let written = writer.await.expect("writer joined").expect("writer ok");

        let on_disk = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(!in_memory.filled().is_empty(), "fixture produced no audio");
        assert_eq!(
            in_memory.filled(),
            on_disk,
            "disk and memory assembly diverged"
        );
        assert_eq!(
            written,
            on_disk.len(),
            "byte count must match what was written"
        );
    }

    /// Order is load-bearing: the same segments assembled out of order must NOT
    /// match. This is why `fetch_decrypt_in_order` asserts the index rather
    /// than trusting `buffered` to stay ordered.
    #[test]
    fn order_changes_the_output() {
        let key = [7u8; 16];
        let segments = fixture();
        let mut forward = Vec::new();
        let mut reversed = Vec::new();
        for (i, seg) in segments.iter().enumerate() {
            decrypt_segment_into(seg, i + 1, &key, &mut forward).expect("fwd");
        }
        for (i, seg) in segments.iter().rev().enumerate() {
            decrypt_segment_into(seg, i + 1, &key, &mut reversed).expect("rev");
        }
        assert_ne!(forward, reversed);
    }

    /// Bytes of the QBZ init UUID box (`qbz_cmaf::parser::QBZ_INIT_UUID`,
    /// private there for the same reason as its segment twin).
    const INIT_UUID: [u8; 16] = [
        0xc7, 0xc7, 0x5d, 0xf0, 0xfd, 0xd9, 0x51, 0xe9, 0x8f, 0xc2, 0x29, 0x71, 0xe4, 0xac, 0xf8,
        0xd2,
    ];

    /// Build an init segment whose table declares `entries` as
    /// `(byte_len, sample_count)`, wrapped exactly as the parser expects.
    fn init_segment(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut flac = Vec::new();
        flac.extend_from_slice(b"fLaC");
        flac.extend_from_slice(&[0x00, 0x00, 0x00, 0x22]); // STREAMINFO, 34 bytes
        flac.extend_from_slice(&[0x11u8; 34]);

        let mut payload = Vec::new();
        payload.extend_from_slice(&[0u8; 4]); // version/padding
        payload.extend_from_slice(&1u32.to_be_bytes()); // track_id
        payload.extend_from_slice(&2u32.to_be_bytes()); // file_id
        payload.extend_from_slice(&96_000u32.to_be_bytes()); // sample_rate
        payload.push(24); // bits_per_sample
        payload.extend_from_slice(&[2, 0, 0]); // channels + padding
        payload.extend_from_slice(&[0u8; 6]); // total_samples_count
        payload.extend_from_slice(&(flac.len() as u16).to_be_bytes());
        payload.extend_from_slice(&flac);
        payload.push(4); // key_id_len
        payload.extend_from_slice(&[0xEEu8; 4]); // key_id
        payload.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for (byte_len, sample_count) in entries {
            payload.extend_from_slice(&byte_len.to_be_bytes());
            payload.extend_from_slice(&sample_count.to_be_bytes());
        }

        let mut out = Vec::new();
        out.extend_from_slice(&((8 + 16 + payload.len()) as u32).to_be_bytes());
        out.extend_from_slice(b"uuid");
        out.extend_from_slice(&INIT_UUID);
        out.extend_from_slice(&payload);
        out
    }

    /// A reproducible pseudo-random source. No dev-dependency, and the seed is
    /// in every failure message, so a broken case can be replayed.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }
        fn in_range(&mut self, lo: usize, hi: usize) -> usize {
            lo + (self.next() as usize) % (hi - lo + 1)
        }
    }

    /// Generate one synthetic track: the segments, and the `(byte_len,
    /// sample_count)` table an honest packager would have written for them.
    ///
    /// `byte_len` here is the segment's `mdat` payload size, which is the claim
    /// under test — see the round trip below.
    fn synthetic_track(seed: u64, n_segments: usize) -> (Vec<Vec<u8>>, Vec<(u32, u32)>) {
        let mut rng = Lcg(seed);
        let mut segments = Vec::with_capacity(n_segments);
        let mut table = Vec::with_capacity(n_segments);
        for _ in 0..n_segments {
            let n_frames = rng.in_range(1, 6);
            let frames: Vec<(Vec<u8>, bool)> = (0..n_frames)
                .map(|_| {
                    let len = rng.in_range(1, 300);
                    let fill = (rng.next() & 0xFF) as u8;
                    (vec![fill; len], rng.next().is_multiple_of(2))
                })
                .collect();
            // A tail beyond the last frame entry is unencrypted audio the
            // assembler must still emit; zero is the common case.
            let trailing = if rng.next().is_multiple_of(3) {
                rng.in_range(1, 40)
            } else {
                0
            };
            let mdat_payload: usize = frames.iter().map(|(f, _)| f.len()).sum::<usize>() + trailing;
            segments.push(segment(&frames, trailing));
            table.push((mdat_payload as u32, (n_frames * 4096) as u32));
        }
        (segments, table)
    }

    /// THE CLAIM the range-seekable CMAF path rests on, pinned as an invariant
    /// of our own code: a segment assembles to EXACTLY the byte count its table
    /// entry declares, so prefix sums over the table are assembled-stream
    /// offsets rather than an estimate of them.
    ///
    /// Proving it here and not only on hardware matters because the table is a
    /// remote input: this test says what our assembler does with it, so a
    /// change on either side — a parser tweak, a new box in the segment, a
    /// Qobuz-side redefinition caught by the feeder's run-time check — has a
    /// fixed reference to fail against.
    #[test]
    fn a_segment_assembles_to_exactly_its_declared_byte_len() {
        for seed in 0..64u64 {
            let (segments, table) = synthetic_track(seed, 7);
            let key = [0x5Au8; 16];
            for (i, (seg, (byte_len, _))) in segments.iter().zip(&table).enumerate() {
                let mut out = Vec::new();
                decrypt_segment_into(seg, i + 1, &key, &mut out).expect("decrypt");
                assert_eq!(
                    out.len() as u32,
                    *byte_len,
                    "seed {seed} segment {} assembled to {} bytes, table said {byte_len}",
                    i + 1,
                    out.len()
                );
            }
        }
    }

    /// The whole round trip, through the real parser: build an init segment,
    /// parse it back, and check that [`qbz_cmaf::SegmentMap`] places every
    /// segment at the offset its bytes actually occupy in the assembled FLAC.
    ///
    /// This is the map the feeder seeks with. If it is off by so much as a
    /// byte, a seek serves audio attributed to the wrong offsets — which
    /// decodes as noise rather than failing.
    #[test]
    fn the_segment_map_places_every_segment_where_its_bytes_land() {
        for seed in 100..132u64 {
            let n = 9;
            let (segments, table) = synthetic_track(seed, n);
            let init = init_segment(&table);
            let info = qbz_cmaf::parse_init_segment(&init).expect("parse init");
            assert_eq!(info.flac_header.len(), 42);
            assert_eq!(info.segment_table.len(), n);

            let map = qbz_cmaf::SegmentMap::new(info.flac_header.len(), &info.segment_table);
            let key = [0x5Au8; 16];

            let mut assembled = info.flac_header.clone();
            for (i, seg) in segments.iter().enumerate() {
                let start = assembled.len() as u64;
                assert_eq!(
                    map.segment_start(i + 1),
                    Some(start),
                    "seed {seed}: segment {} starts at {start}, map said {:?}",
                    i + 1,
                    map.segment_start(i + 1)
                );
                decrypt_segment_into(seg, i + 1, &key, &mut assembled).expect("decrypt");
                assert_eq!(
                    map.segment_len(i + 1),
                    Some(assembled.len() as u64 - start),
                    "seed {seed}: segment {} length disagrees with the table",
                    i + 1
                );
            }

            assert_eq!(
                map.total_len(),
                assembled.len() as u64,
                "seed {seed}: the table's total is not the assembled size"
            );

            // And the reverse direction: every assembled byte resolves to the
            // segment that carries it, which is what a seek asks of the map.
            for offset in (map.header_len()..map.total_len()).step_by(7) {
                let seg = map.segment_at(offset).expect("audio offset");
                let resume = map.resume_at(offset);
                assert_eq!(resume.first_segment, seg);
                assert!(!resume.with_header);
                assert!(resume.body_offset <= offset);
                assert_eq!(map.segment_start(seg), Some(resume.body_offset));
            }
        }
    }
}

#[cfg(test)]
mod arc_slab_tests {
    use super::{ArcSlab, TrackSink};

    /// The whole point: what comes out of the slab IS the allocation that was
    /// filled, so handing it on costs a refcount bump and not a copy of the
    /// track.
    #[test]
    fn the_track_leaves_in_the_allocation_it_was_assembled_in() {
        let mut slab = ArcSlab::with_capacity(6);
        slab.append(b"abc").unwrap();
        let filled_ptr = slab.filled().as_ptr();
        slab.append(b"def").unwrap();

        let bytes = slab.into_arc().expect("filled exactly");
        assert_eq!(&bytes[..], b"abcdef");
        assert_eq!(
            bytes.as_ptr(),
            filled_ptr,
            "into_arc must hand back the same allocation, not a copy of it"
        );

        // And sharing it is free, which is the reason for `Arc<[u8]>` at all.
        let shared = bytes.clone();
        assert_eq!(shared.as_ptr(), bytes.as_ptr());
    }

    /// A frame decrypted in place lands in the slab, not in a scratch copy.
    #[test]
    fn a_frame_can_be_rewritten_in_place_after_it_is_appended() {
        let mut slab = ArcSlab::with_capacity(8);
        slab.append(b"head").unwrap();
        let frame_start = slab.len();
        slab.append(b"ENCR").unwrap();
        slab.tail_from(frame_start).copy_from_slice(b"clea");

        assert_eq!(&slab.into_arc().unwrap()[..], b"headclea");
    }

    /// A segment table that promises more than arrives must be an error, never
    /// a short track: whatever leaves here is treated as a complete track for
    /// ever after, and a short one decodes as garbage on every later play.
    #[test]
    fn a_short_track_is_refused_rather_than_published() {
        let mut slab = ArcSlab::with_capacity(16);
        slab.append(b"only this").unwrap();
        let err = slab
            .into_arc()
            .expect_err("a short track must not be published");
        assert!(err.contains("refusing a short track"), "{err}");
    }

    /// And bytes beyond the promised size are refused too, rather than
    /// silently truncated.
    #[test]
    fn data_that_overruns_the_segment_table_is_refused() {
        let mut slab = ArcSlab::with_capacity(4);
        slab.append(b"fits").unwrap();
        let err = slab
            .append(b"more")
            .expect_err("overrun must not be swallowed");
        assert!(err.contains("overruns the slab"), "{err}");
    }

    /// The slab starts zeroed, which is what makes `assume_init` sound however
    /// much of it the download writes — the safety argument, pinned.
    #[test]
    fn the_slab_starts_zeroed() {
        let slab = ArcSlab::with_capacity(32);
        assert_eq!(slab.len(), 0);
        assert!(slab.filled().is_empty());
        let bytes = ArcSlab::with_capacity(32).into_arc().unwrap_err();
        assert!(bytes.contains("assembled 0 bytes"), "{bytes}");
    }
}
