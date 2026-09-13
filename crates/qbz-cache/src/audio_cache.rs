//! L1 Memory Cache - In-memory LRU cache for audio data
//!
//! Fast access cache with configurable size limit and LRU eviction.
//! Evicted tracks can optionally spill to L2 disk cache.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::PlaybackCache;

/// Audio bytes for one track, shared rather than copied.
///
/// A Hi-Res FLAC is 60-170 MB, and the same track is simultaneously held by
/// the cache, the audio thread's resume/seek copy, and the decoder's cursor.
/// With `Vec<u8>` each of those was a separate allocation — three to five
/// resident copies of one track, which is what put a Pi 3B+ (1 GB) into swap
/// during normal playback. `Arc<[u8]>` makes every hand-off a refcount bump.
pub type TrackBytes = Arc<[u8]>;

/// Cached audio data for a track
#[derive(Clone)]
pub struct CachedTrack {
    pub track_id: u64,
    pub data: TrackBytes,
    pub size_bytes: usize,
}

/// Internal cache state - all in one struct to avoid deadlocks
struct CacheState {
    /// Cached tracks keyed by track ID
    tracks: HashMap<u64, CachedTrack>,
    /// Order of access for LRU eviction (most recent at back)
    access_order: Vec<u64>,
    /// Current cache size in bytes
    current_size: usize,
    /// Track IDs currently being fetched
    fetching: HashSet<u64>,
    /// Track IDs whose last prefetch failed, with when it failed. Lets the
    /// prefetch scheduler back off a track that is currently un-fetchable
    /// (e.g. the account is being 403'd) instead of re-hammering it every
    /// queue tick and feeding a request storm (issue #637).
    failed: HashMap<u64, Instant>,
}

/// Audio cache manager with LRU eviction and optional disk spillover
///
/// Provides fast in-memory caching with automatic eviction when the
/// size limit is reached. Evicted tracks are written to the L2 disk
/// cache (if configured) for later retrieval.
pub struct AudioCache {
    state: Mutex<CacheState>,
    /// Maximum cache size in bytes
    max_size_bytes: usize,
    /// Optional disk-based L2 cache for evicted tracks
    playback_cache: Option<Arc<PlaybackCache>>,
}

impl Default for AudioCache {
    fn default() -> Self {
        Self::new(400 * 1024 * 1024) // 400MB for ~4-5 Hi-Res tracks
    }
}

impl AudioCache {
    /// Create a new cache with specified max size in bytes
    pub fn new(max_size_bytes: usize) -> Self {
        Self {
            state: Mutex::new(CacheState {
                tracks: HashMap::new(),
                access_order: Vec::new(),
                current_size: 0,
                fetching: HashSet::new(),
                failed: HashMap::new(),
            }),
            max_size_bytes,
            playback_cache: None,
        }
    }

    /// Create cache with disk spillover enabled
    pub fn with_playback_cache(max_size_bytes: usize, playback_cache: Arc<PlaybackCache>) -> Self {
        Self {
            state: Mutex::new(CacheState {
                tracks: HashMap::new(),
                access_order: Vec::new(),
                current_size: 0,
                fetching: HashSet::new(),
                failed: HashMap::new(),
            }),
            max_size_bytes,
            playback_cache: Some(playback_cache),
        }
    }

    /// Get the playback cache reference
    pub fn get_playback_cache(&self) -> Option<&Arc<PlaybackCache>> {
        self.playback_cache.as_ref()
    }

    /// How many bytes this cache may hold, as configured for this host.
    ///
    /// For a caller deciding whether to keep a track in memory or hand it over
    /// as a file: the useful question is not how big the track is, it is whether
    /// two of them — the one playing and the one prefetched — can coexist here.
    pub fn budget_bytes(&self) -> usize {
        self.max_size_bytes
    }

    /// Get a track from cache if available.
    ///
    /// The clone is a refcount bump (see [`TrackBytes`]), not a copy of the
    /// audio; callers may hold the result for as long as they need it.
    pub fn get(&self, track_id: u64) -> Option<CachedTrack> {
        let mut state = self.state.lock().unwrap();

        let track = state.tracks.get(&track_id).cloned();

        if track.is_some() {
            // Update access order (move to back = most recently used)
            state.access_order.retain(|&id| id != track_id);
            state.access_order.push(track_id);
            log::debug!("Cache hit for track {}", track_id);
        } else {
            log::debug!("Cache miss for track {}", track_id);
        }

        track
    }

    /// Check if a track is in cache without updating access order
    pub fn contains(&self, track_id: u64) -> bool {
        self.state.lock().unwrap().tracks.contains_key(&track_id)
    }

    /// Check if a track is currently being fetched
    pub fn is_fetching(&self, track_id: u64) -> bool {
        self.state.lock().unwrap().fetching.contains(&track_id)
    }

    /// Mark a track as being fetched
    pub fn mark_fetching(&self, track_id: u64) {
        self.state.lock().unwrap().fetching.insert(track_id);
    }

    /// Unmark a track as being fetched
    pub fn unmark_fetching(&self, track_id: u64) {
        self.state.lock().unwrap().fetching.remove(&track_id);
    }

    /// Record that a prefetch for this track failed (starts a back-off window).
    pub fn mark_failed(&self, track_id: u64) {
        self.state
            .lock()
            .unwrap()
            .failed
            .insert(track_id, Instant::now());
    }

    /// True if the track failed to prefetch within `cooldown` — the scheduler
    /// uses this to skip re-hammering a currently un-fetchable track (issue
    /// #637). Expired entries are cleaned up on read.
    pub fn recently_failed(&self, track_id: u64, cooldown: Duration) -> bool {
        let mut state = self.state.lock().unwrap();
        match state.failed.get(&track_id) {
            Some(when) if when.elapsed() < cooldown => true,
            Some(_) => {
                state.failed.remove(&track_id);
                false
            }
            None => false,
        }
    }

    /// Clear a track's failure marker (e.g. once it is successfully cached).
    pub fn clear_failed(&self, track_id: u64) {
        self.state.lock().unwrap().failed.remove(&track_id);
    }

    /// Insert a track into cache, evicting old entries to disk if needed.
    ///
    /// Takes anything convertible into [`TrackBytes`]; passing an existing
    /// `TrackBytes` shares the buffer the caller is already playing from,
    /// while a `Vec<u8>` straight off the network is converted once here.
    pub fn insert(&self, track_id: u64, data: impl Into<TrackBytes>) {
        let data: TrackBytes = data.into();
        let size = data.len();

        // Don't cache if track is larger than max cache size
        if size > self.max_size_bytes {
            log::warn!(
                "Track {} ({} bytes) too large for cache (max {} bytes)",
                track_id,
                size,
                self.max_size_bytes
            );
            return;
        }

        // Collect tracks to evict (to avoid holding lock while writing to disk)
        let mut tracks_to_spill: Vec<CachedTrack> = Vec::new();

        {
            let mut state = self.state.lock().unwrap();

            // Evict old entries to make room
            while state.current_size + size > self.max_size_bytes && !state.access_order.is_empty()
            {
                let oldest_id = state.access_order.remove(0);
                if let Some(track) = state.tracks.remove(&oldest_id) {
                    state.current_size = state.current_size.saturating_sub(track.size_bytes);
                    log::debug!(
                        "Evicting track {} ({} bytes) from memory cache",
                        oldest_id,
                        track.size_bytes
                    );
                    tracks_to_spill.push(track);
                }
            }
        }

        // Spill evicted tracks to disk cache (outside of lock)
        if let Some(playback_cache) = &self.playback_cache {
            for track in tracks_to_spill {
                playback_cache.insert(track.track_id, &track.data);
            }
        }

        let mut state = self.state.lock().unwrap();

        // If track already exists, update size tracking
        if let Some(existing) = state.tracks.get(&track_id) {
            state.current_size = state.current_size.saturating_sub(existing.size_bytes);
        }

        let cached = CachedTrack {
            track_id,
            data,
            size_bytes: size,
        };

        state.tracks.insert(track_id, cached);
        state.current_size += size;

        // Update access order
        state.access_order.retain(|&id| id != track_id);
        state.access_order.push(track_id);

        log::info!(
            "Cached track {} ({} bytes). Cache size: {}/{} bytes",
            track_id,
            size,
            state.current_size,
            self.max_size_bytes
        );
    }

    /// Drop one track from L1, spilling it to L2 on the way out.
    ///
    /// LRU alone keeps a finished track resident until something else needs the
    /// room. On a 1 GB Pi that means the track just played (~100 MB for Hi-Res)
    /// sits beside the one now playing AND the gapless prefetch — the shape that
    /// had qbzd at 465 MB RSS on a 905 MB box, swapping to the SD card. Releasing
    /// at the transition is quality-neutral: the bytes land in the disk cache, so
    /// a back-skip re-reads them from L2 instead of the network.
    ///
    /// Returns whether the track was resident. Safe to call for a track that
    /// was never cached, or twice.
    pub fn release(&self, track_id: u64) -> bool {
        let released = {
            let mut state = self.state.lock().unwrap();
            match state.tracks.remove(&track_id) {
                Some(track) => {
                    state.current_size = state.current_size.saturating_sub(track.size_bytes);
                    state.access_order.retain(|&id| id != track_id);
                    Some(track)
                }
                None => None,
            }
        };

        let Some(track) = released else {
            return false;
        };

        log::info!(
            "Released track {} ({} bytes) from the memory cache",
            track_id,
            track.size_bytes
        );

        // Spill outside the lock, exactly as LRU eviction does.
        if let Some(playback_cache) = &self.playback_cache {
            playback_cache.insert(track.track_id, &track.data);
        }
        true
    }

    /// Clear all cached data (both L1 memory and L2 disk caches)
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.tracks.clear();
        state.access_order.clear();
        state.current_size = 0;
        state.fetching.clear();
        state.failed.clear();
        log::info!("L1 memory cache cleared");

        // Also clear L2 disk cache if present
        if let Some(ref playback_cache) = self.playback_cache {
            playback_cache.clear();
            log::info!("L2 playback cache cleared");
        }
    }

    /// Cache statistics, read by this crate's own tests to assert eviction and
    /// sizing. No production caller — the tests ARE the reason it exists, and
    /// deleting it would mean deleting the coverage with it.
    pub fn stats(&self) -> CacheStats {
        let state = self.state.lock().unwrap();
        CacheStats {
            cached_tracks: state.tracks.len(),
            current_size_bytes: state.current_size,
            max_size_bytes: self.max_size_bytes,
            fetching_count: state.fetching.len(),
        }
    }
}

/// Cache statistics
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheStats {
    pub cached_tracks: usize,
    pub current_size_bytes: usize,
    pub max_size_bytes: usize,
    pub fetching_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transition release: the finished track's bytes leave L1 immediately
    /// and stop counting against the budget, while the track now playing stays.
    #[test]
    fn release_frees_one_track_and_leaves_the_rest() {
        let cache = AudioCache::new(10 * 1024 * 1024);
        cache.insert(1, vec![0u8; 1024]);
        cache.insert(2, vec![0u8; 2048]);
        assert_eq!(cache.stats().current_size_bytes, 3072);

        assert!(cache.release(1), "track 1 was resident");
        assert!(!cache.contains(1));
        assert!(cache.contains(2), "the playing track is untouched");
        assert_eq!(cache.stats().current_size_bytes, 2048);
        assert_eq!(cache.stats().cached_tracks, 1);
    }

    /// Releasing a track that was never cached — or releasing twice — is a
    /// no-op, so the driver can emit the action unconditionally.
    #[test]
    fn release_is_a_no_op_for_an_absent_track() {
        let cache = AudioCache::new(10 * 1024 * 1024);
        cache.insert(1, vec![0u8; 1024]);

        assert!(!cache.release(99));
        assert!(cache.release(1));
        assert!(!cache.release(1));
        assert_eq!(cache.stats().current_size_bytes, 0);
    }
}

#[cfg(test)]
mod residency_tests {
    use super::*;

    /// Inserting a downloaded track COPIES it, so for an instant both copies of
    /// a Hi-Res FLAC are resident.
    ///
    /// `TrackBytes` is `Arc<[u8]>`, which carries a refcount header ahead of the
    /// bytes, so it cannot adopt a `Vec`'s allocation — `Arc::from(vec)` always
    /// allocates and copies. Every prefetch arrives as a `Vec<u8>` (the CMAF and
    /// legacy download paths both return one) and is handed straight to
    /// `insert`, so completing a prefetch briefly holds 2x the track: 240 MB+
    /// for a Hi-Res track, on boards with 512-905 MB of RAM, while the track
    /// now playing is also resident.
    ///
    /// This is asserted as a POINTER inequality rather than measured with a
    /// counting allocator on purpose. The fact is discrete and the test is
    /// exact; a peak-bytes measurement of the same thing would be at the mercy
    /// of the allocator's `realloc` behaviour and of whatever else the test
    /// binary is doing on another thread.
    ///
    /// Nothing here is a bug to be fixed by this test — it is a cost to be
    /// known, and pinned so that a future change that removes it (building the
    /// download into an `Arc` buffer directly) is visibly a change.
    #[test]
    fn inserting_a_downloaded_track_copies_it_rather_than_adopting_the_buffer() {
        let cache = AudioCache::new(64 * 1024 * 1024);

        let downloaded = vec![7u8; 4 * 1024 * 1024];
        let downloaded_ptr = downloaded.as_ptr();
        cache.insert(42, downloaded);

        let cached = cache.get(42).expect("just inserted").data;
        assert_ne!(
            cached.as_ptr(),
            downloaded_ptr,
            "if these ever match, Arc has learned to adopt a Vec's allocation \
             and the transient double is gone — update the comment above"
        );
        assert_eq!(cached.len(), 4 * 1024 * 1024);
        assert_eq!(cached[0], 7);
    }

    /// A track already held as `TrackBytes` is inserted by refcount bump, with
    /// no copy at all.
    ///
    /// This is the other half of the story and the reason the type exists: the
    /// gapless path hands the SAME `Arc` to the cache, the audio thread and the
    /// decoder, and only the first of those ever paid for a copy. A change that
    /// made `insert` take bytes by value would silently reintroduce three
    /// resident copies of every track — the shape that put a 1 GB Pi into swap.
    #[test]
    fn inserting_bytes_already_shared_costs_no_copy() {
        let cache = AudioCache::new(64 * 1024 * 1024);

        let shared: TrackBytes = Arc::from(vec![3u8; 1024 * 1024]);
        let shared_ptr = shared.as_ptr();
        cache.insert(99, shared.clone());

        let cached = cache.get(99).expect("just inserted").data;
        assert_eq!(
            cached.as_ptr(),
            shared_ptr,
            "handing the cache an Arc it can share must not copy the track"
        );
    }
}
