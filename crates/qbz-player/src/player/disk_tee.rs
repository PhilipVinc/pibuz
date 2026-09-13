//! Staging a track into the L2 cache as it streams past.
//!
//! Every windowed feeder needs this. A bounded buffer holds a window, not the
//! track, so by the time the last byte arrives the first is long gone and
//! there is nothing left to hand the cache: the route that used to exist —
//! reading the finished track back out of the buffer — only ever answered for
//! a buffer still holding one whole run from byte 0, and no feeder leaves one
//! behind any more. Writing the bytes out as they go past is what keeps a
//! streamed track replayable without ever holding it twice.

use std::sync::Arc;

use qbz_cache::PlaybackCache;

/// Writes the contiguous prefix of a streamed track into the L2 cache.
///
/// The cast path used to read no cache and write none, so the daemon filled a
/// cache its primary route never touched and a `previous` tap re-downloaded a
/// track already on the card. The read half is `Player::play_cached_if_present`;
/// this is the write half, shared by the two feeders that have one — the remote
/// body pump and the CMAF segment assembler.
///
/// Deliberately gives up rather than getting clever. It only ever appends at
/// `next`, so the moment a seek makes the byte stream non-contiguous the tee is
/// abandoned and the `.part` discarded: a partial file that looks whole is
/// worse than no file, and `commit_write` is only reached when the byte count
/// matches the size the track was declared to be.
pub struct DiskTee {
    cache: Arc<PlaybackCache>,
    track_id: u64,
    file: std::io::BufWriter<std::fs::File>,
    next: u64,
    expected: u64,
    live: bool,
}

impl DiskTee {
    /// Open a `.part` for `track_id`, or `None` when there is no cache to
    /// stage into, the size is unknown, or the file cannot be created.
    pub fn open(cache: Option<Arc<PlaybackCache>>, track_id: u64, expected: u64) -> Option<Self> {
        let cache = cache?;
        if expected == 0 {
            return None;
        }
        let part = cache.begin_write(track_id, expected)?;
        let file = std::fs::File::create(&part)
            .map_err(|e| log::debug!("[CACHE] cannot stage track {track_id}: {e}"))
            .ok()?;
        Some(Self {
            cache,
            track_id,
            // 256 KiB so a 64 KiB chunk is not a syscall each. The write lands
            // in the page cache either way; this is about syscall count on a
            // tokio worker, not about durability.
            file: std::io::BufWriter::with_capacity(256 * 1024, file),
            next: 0,
            expected,
            live: true,
        })
    }

    /// Append `chunk`, which must begin exactly where the last one ended.
    pub fn write_at(&mut self, offset: u64, chunk: &[u8]) {
        if !self.live {
            return;
        }
        if offset != self.next {
            self.abandon("the byte stream is no longer contiguous");
            return;
        }
        use std::io::Write as _;
        if let Err(e) = self.file.write_all(chunk) {
            self.abandon(&format!("write failed: {e}"));
            return;
        }
        self.next += chunk.len() as u64;
    }

    /// Stop staging and discard the `.part`.
    pub fn abandon(&mut self, why: &str) {
        if !self.live {
            return;
        }
        self.live = false;
        log::debug!("[CACHE] not staging track {} to disk: {why}", self.track_id);
        self.cache.abort_write(self.track_id);
    }

    /// Publish only a whole file, and only after it is on the card: the rename
    /// in `commit_write` is the atomicity guarantee, so an unsynced `.part`
    /// would make it a lie.
    pub fn finish(mut self) {
        if !self.live {
            return;
        }
        if self.next != self.expected {
            self.abandon(&format!(
                "{} of {} bytes — incomplete",
                self.next, self.expected
            ));
            return;
        }
        use std::io::Write as _;
        if let Err(e) = self.file.flush() {
            self.abandon(&format!("flush failed: {e}"));
            return;
        }
        match self.file.get_ref().sync_all() {
            Ok(()) => {
                if self.cache.commit_write(self.track_id).is_some() {
                    log::info!(
                        "[CACHE] Track {} staged to the disk cache ({} bytes)",
                        self.track_id,
                        self.next
                    );
                }
                self.live = false;
            }
            Err(e) => self.abandon(&format!("sync failed: {e}")),
        }
    }
}

impl Drop for DiskTee {
    fn drop(&mut self) {
        // A feeder aborted on track change must not leave a `.part` behind;
        // `rebuild_state` sweeps them at startup, but not before then.
        self.abandon("the feeder stopped before the track finished");
    }
}
