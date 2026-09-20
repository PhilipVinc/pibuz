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
use qbz_cmaf::SealWriter;

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
    /// Sealed, like everything else in the L2 cache: these are decrypted
    /// frames on their way past, and writing them in the clear is precisely
    /// what this daemon does not do. The seal is a keystream over bytes that
    /// are already being copied into a buffer, so the tee stays what it was —
    /// a few percent of one core on the way to the card.
    file: SealWriter<std::io::BufWriter<std::fs::File>>,
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
        // 256 KiB so a 64 KiB chunk is not a syscall each. The write lands
        // in the page cache either way; this is about syscall count on a
        // tokio worker, not about durability.
        let file = SealWriter::new(
            cache.key(),
            std::io::BufWriter::with_capacity(256 * 1024, file),
        )
        .map_err(|e| log::debug!("[CACHE] cannot seal track {track_id}: {e}"))
        .ok()?;
        Some(Self {
            cache,
            track_id,
            file,
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
        match self.file.get_ref().get_ref().sync_all() {
            Ok(()) => {
                if self.cache.commit_write(self.track_id).is_some() {
                    log::info!(
                        "[CACHE] Track {} staged to the disk cache ({} bytes of audio, sealed)",
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
        // the startup sweep clears them, but not before then.
        self.abandon("the feeder stopped before the track finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache() -> (Arc<PlaybackCache>, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qbz-tee-{}-{n}", std::process::id()));
        let cache = PlaybackCache::with_path(dir.clone(), 8 * 1024 * 1024).expect("cache");
        (Arc::new(cache), dir)
    }

    /// The seam this type sits on, end to end: bytes go past the tee, the
    /// cache publishes the file, and the file reads back as the audio.
    ///
    /// Each half is tested elsewhere — the cipher in `qbz_cmaf::vault`, the
    /// index in `qbz_cache` — and neither notices if the tee writes the
    /// plaintext into a sealed file's body, or seals twice. This does.
    #[test]
    fn a_staged_track_comes_back_as_the_audio_that_went_in() {
        let (cache, dir) = temp_cache();
        let track: Vec<u8> = b"fLaC"
            .iter()
            .copied()
            .chain((0..20_000u32).map(|i| (i % 251) as u8))
            .collect();

        let mut tee = DiskTee::open(Some(cache.clone()), 7, track.len() as u64).expect("tee");
        // Chunked the way a feeder hands them over — a segment at a time, not
        // a whole track.
        let mut at = 0u64;
        for chunk in track.chunks(4_096) {
            tee.write_at(at, chunk);
            at += chunk.len() as u64;
        }
        tee.finish();

        let file = cache.file_if_present(7).expect("the track was published");
        assert_eq!(file.read_all().expect("read back"), track);

        // And what is on the card is not the track.
        let on_card = std::fs::read(file.path()).expect("the file itself");
        assert!(
            !on_card.windows(4).any(|w| w == b"fLaC"),
            "the tee wrote audio in the clear"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A feeder that seeks stops staging, and publishes nothing. A partial
    /// file that looks whole is worse than no file — and now it would also be
    /// a file sealed around a hole.
    #[test]
    fn a_non_contiguous_write_publishes_nothing() {
        let (cache, dir) = temp_cache();
        let mut tee = DiskTee::open(Some(cache.clone()), 9, 8_192).expect("tee");
        tee.write_at(0, &[1u8; 4_096]);
        tee.write_at(6_000, &[2u8; 2_192]);
        tee.finish();

        assert!(cache.file_if_present(9).is_none());
        assert!(!dir.join("9.audio").exists());
        assert!(!dir.join("9.part").exists(), "the .part was left behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Short of the declared size is the same refusal, for the same reason:
    /// `commit_write` sizes the entry from the file, so a short one would be
    /// indexed as complete and decode as garbage forever after.
    #[test]
    fn a_short_track_publishes_nothing() {
        let (cache, dir) = temp_cache();
        let mut tee = DiskTee::open(Some(cache.clone()), 11, 8_192).expect("tee");
        tee.write_at(0, &[3u8; 4_096]);
        tee.finish();

        assert!(cache.file_if_present(11).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
