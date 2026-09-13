//! QBZ Core Orchestrator
//!
//! The main orchestrator that connects all QBZ subsystems and provides
//! a unified API for frontends.

use std::sync::Arc;
use tokio::sync::RwLock;

use qbz_models::{
    Album, CoreEvent, DiscoverAlbum, FrontendAdapter, Playlist, Quality, QueueState, QueueTrack,
    RepeatMode, StreamUrl, Track, TrackToAnalyse, TracksContainer, UserSession,
};
use qbz_player::{PlaybackState, Player, QueueManager};
use qbz_qobuz::QobuzClient;

use crate::error::CoreError;

/// Set of blacklisted artist ids. Built per call from the live blacklist store
/// (`qbz-app`); empty only under fail-open (no session bound / feature off).
pub type BlacklistFilter = std::collections::HashSet<u64>;

/// Set of blocked album ids (Qobuz album ids are alphanumeric `String`s, so a
/// separate, parallel axis from the `u64` artist filter). An album is hidden by
/// its OWN id regardless of artist — the surgical fix for Qobuz same-name
/// artist merges. Empty under fail-open.
pub type AlbumBlacklistFilter = std::collections::HashSet<String>;

/// D-FEAT: returns true if the album should be hidden by the blacklist.
///
/// Extends the historical Tauri rule (which blocked only the PRIMARY
/// `album.artist`) to also block when ANY contributor in `album.artists[]`
/// (featured artists included) is blacklisted. Centralizing this here keeps
/// every call site (search, discovery, queue-build) on ONE consistent rule.
///
/// Fail-open: an empty filter never blocks; an album with no matching id is
/// kept.
pub fn album_blacklisted(
    album: &Album,
    bl: &BlacklistFilter,
    album_bl: &AlbumBlacklistFilter,
) -> bool {
    if bl.is_empty() && album_bl.is_empty() {
        return false;
    }
    // Album axis (orthogonal): the album's OWN id being blocked hides it
    // regardless of artist.
    if album_bl.contains(&album.id) {
        return true;
    }
    if bl.contains(&album.artist.id) {
        return true;
    }
    album
        .artists
        .as_ref()
        .is_some_and(|v| v.iter().any(|a| bl.contains(&a.id)))
}

/// D-FEAT: returns true if the track should be hidden by the blacklist.
///
/// Blocks on the track's structured `performer` OR `composer` id. Extends the
/// historical Tauri rule (performer only) to also cover the composer.
///
/// Fail-open: an empty filter never blocks; a track with neither a performer
/// nor a composer id is kept (no id to match against).
///
/// D-FEAT limitation: the model exposes no structured per-track *featured
/// performer id* — only `performer`, `composer`, and a free-text `performers`
/// string. We deliberately do NOT name-match the free-text string; this rule
/// is strictly id-based.
pub fn track_blacklisted(
    track: &Track,
    bl: &BlacklistFilter,
    album_bl: &AlbumBlacklistFilter,
) -> bool {
    if bl.is_empty() && album_bl.is_empty() {
        return false;
    }
    // Album axis: a track of a blocked album is hidden too.
    if track
        .album
        .as_ref()
        .is_some_and(|a| album_bl.contains(&a.id))
    {
        return true;
    }
    track.performer.as_ref().is_some_and(|a| bl.contains(&a.id))
        || track.composer.as_ref().is_some_and(|a| bl.contains(&a.id))
}

/// D-FEAT: returns true if a discover-shaped album should be hidden.
///
/// Discover albums expose only a flat `artists[]` vec (no separate primary
/// `artist`), so any matching contributor id — primary or featured — blocks
/// the album. Fail-open: an empty filter never blocks.
pub fn discover_album_blacklisted(
    album: &DiscoverAlbum,
    bl: &BlacklistFilter,
    album_bl: &AlbumBlacklistFilter,
) -> bool {
    if bl.is_empty() && album_bl.is_empty() {
        return false;
    }
    if album_bl.contains(&album.id) {
        return true;
    }
    album.artists.iter().any(|a| bl.contains(&a.id))
}

/// Core orchestrator for QBZ
///
/// This is the main entry point for any frontend (Tauri, Slint, Iced, CLI, etc.)
/// It provides a unified API and emits events through the FrontendAdapter.
pub struct QbzCore<A: FrontendAdapter> {
    /// Frontend adapter for event emission
    adapter: Arc<A>,
    /// Qobuz API client
    client: Arc<RwLock<Option<QobuzClient>>>,
    /// Queue manager
    queue: Arc<RwLock<QueueManager>>,
    /// Audio player
    player: Arc<Player>,
    /// Whether the core is initialized
    initialized: Arc<RwLock<bool>>,
    /// D8 guard: true when the current queue was built from an OFFLINE-ONLY
    /// local playlist — such a queue must never be pushed to the Qobuz
    /// Connect cloud. Cleared by every queue REPLACEMENT (`set_queue` /
    /// `set_queue_with_order` / `clear_queue`); append-style ops preserve it.
    /// Set explicitly by the frontend's local-playlist play path right after
    /// its `set_queue`.
    queue_offline_only: Arc<std::sync::atomic::AtomicBool>,
}

impl<A: FrontendAdapter + Send + Sync + 'static> QbzCore<A> {
    /// Create a new QbzCore instance with the given frontend adapter and player
    ///
    /// The Player must be created by the frontend with appropriate audio settings.
    /// QbzCore orchestrates playback through this player.
    pub fn new(adapter: A, player: Player) -> Self {
        Self {
            adapter: Arc::new(adapter),
            client: Arc::new(RwLock::new(None)),
            queue: Arc::new(RwLock::new(QueueManager::new())),
            player: Arc::new(player),
            initialized: Arc::new(RwLock::new(false)),
            queue_offline_only: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Mark (or unmark) the current queue as built from an OFFLINE-ONLY local
    /// playlist (D8). Call right after the `set_queue` that loaded it.
    ///
    /// Every caller in this tree passes `false`; the local/offline library
    /// that set it `true` went with the desktop app. Kept because the
    /// resetting calls are real and `queue_is_offline_only` is still read by
    /// qconnect/publish.rs — collapsing the pair is a separate change.
    pub fn set_queue_offline_only(&self, on: bool) {
        self.queue_offline_only
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// True when the current queue originates from an offline-only local
    /// playlist — QConnect must skip its cloud queue push.
    pub fn queue_is_offline_only(&self) -> bool {
        self.queue_offline_only
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Initialize the core
    ///
    /// This should be called once at startup to set up all subsystems.
    /// Best-effort: if bundle token extraction fails (e.g. no network), the
    /// core still finishes initialization so queue manager and player remain
    /// usable for offline/local playback. API calls will then return
    /// `CoreError::NotInitialized` until the client is rebuilt with tokens.
    pub async fn init(&self) -> Result<(), CoreError> {
        let mut initialized = self.initialized.write().await;
        if *initialized {
            return Ok(());
        }

        let client = QobuzClient::new().map_err(|e| CoreError::Internal(e.to_string()))?;

        match client.init().await {
            Ok(_) => {
                *self.client.write().await = Some(client);
                log::info!("QbzCore initialized with bundle tokens");
            }
            Err(e) => {
                log::warn!(
                    "QbzCore: bundle token extraction failed ({}). Starting in offline-tolerant mode; API calls will be unavailable until next online start.",
                    e
                );
            }
        }

        *initialized = true;
        Ok(())
    }

    /// Whether the Qobuz API client is initialized (bundle tokens extracted).
    /// Returns false when the core is running in offline-tolerant mode after
    /// a failed bundle extraction at startup.
    pub async fn is_api_initialized(&self) -> bool {
        self.client.read().await.is_some()
    }

    /// Best-effort attempt to rebuild the Qobuz API client. Useful when the
    /// initial `init()` ran offline and the host has since regained network.
    /// No-op when the client is already initialized.
    pub async fn try_init_api(&self) -> Result<(), CoreError> {
        {
            let guard = self.client.read().await;
            if guard.is_some() {
                return Ok(());
            }
        }

        let client = QobuzClient::new().map_err(|e| CoreError::Internal(e.to_string()))?;
        client
            .init()
            .await
            .map_err(|e| CoreError::Internal(format!("Failed to extract bundle tokens: {}", e)))?;
        *self.client.write().await = Some(client);
        log::info!("QbzCore: API client initialized lazily");
        Ok(())
    }

    /// Check if a user session exists
    pub async fn has_session(&self) -> bool {
        let client = self.client.read().await;
        if let Some(c) = client.as_ref() {
            c.is_logged_in().await
        } else {
            false
        }
    }

    /// Login with email and password
    pub async fn login(&self, email: &str, password: &str) -> Result<UserSession, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;

        match client.login(email, password).await {
            Ok(session) => {
                self.emit(CoreEvent::LoggedIn {
                    session: session.clone(),
                })
                .await;
                Ok(session)
            }
            Err(e) => {
                self.emit(CoreEvent::Error {
                    code: "AUTH_FAILED".to_string(),
                    message: e.to_string(),
                    recoverable: true,
                })
                .await;
                Err(CoreError::AuthFailed(e.to_string()))
            }
        }
    }

    // ==================== Queue Operations ====================

    /// Get current queue state
    pub async fn get_queue_state(&self) -> QueueState {
        let queue = self.queue.read().await;
        queue.get_state()
    }

    /// Get all queue tracks and current index (for session persistence)
    pub async fn get_all_queue_tracks(&self) -> (Vec<QueueTrack>, Option<usize>) {
        let queue = self.queue.read().await;
        queue.get_all_tracks()
    }

    /// Get the full queue state without the upcoming/history caps that
    /// `get_queue_state` applies. Used by clients that paginate the
    /// upcoming list and need the complete play history (Queue sidebar).
    pub async fn get_queue_state_full(&self) -> QueueState {
        let queue = self.queue.read().await;
        queue.get_state_full()
    }

    /// Set repeat mode
    pub async fn set_repeat_mode(&self, mode: RepeatMode) {
        let queue = self.queue.write().await;
        queue.set_repeat(mode);
        self.emit(CoreEvent::RepeatModeChanged { mode }).await;
    }

    /// Set shuffle
    pub async fn set_shuffle(&self, enabled: bool) {
        let queue = self.queue.write().await;
        queue.set_shuffle(enabled);
        self.emit(CoreEvent::ShuffleChanged { enabled }).await;
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Set shuffle mode using an authoritative order.
    pub async fn set_shuffle_with_order(&self, enabled: bool, shuffle_order: Option<Vec<usize>>) {
        let queue = self.queue.write().await;
        queue.set_shuffle_with_order(enabled, shuffle_order);
        self.emit(CoreEvent::ShuffleChanged { enabled }).await;
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Toggle shuffle and return new state
    pub async fn toggle_shuffle(&self) -> bool {
        let queue = self.queue.write().await;
        let was_enabled = queue.is_shuffle();
        let new_enabled = !was_enabled;
        queue.set_shuffle(new_enabled);
        self.emit(CoreEvent::ShuffleChanged {
            enabled: new_enabled,
        })
        .await;
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
        new_enabled
    }

    /// Clear the queue. `keep_current=true` preserves the now-playing track
    /// (historical behavior); `false` wipes everything including the current
    /// slot — use when nothing is actively playing and the user wants a full
    /// reset.
    pub async fn clear_queue(&self, keep_current: bool) {
        self.set_queue_offline_only(false);
        let queue = self.queue.write().await;
        queue.clear(keep_current);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Add a track to the end of the queue
    pub async fn add_track(&self, track: QueueTrack) {
        let queue = self.queue.write().await;
        queue.add_track(track);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Add multiple tracks to the queue
    pub async fn add_tracks(&self, tracks: Vec<QueueTrack>) {
        let queue = self.queue.write().await;
        queue.add_tracks(tracks);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Add a track to play next (after current)
    pub async fn add_track_next(&self, track: QueueTrack) {
        let queue = self.queue.write().await;
        queue.add_track_next(track);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Set the entire queue (replaces existing)
    pub async fn set_queue(&self, tracks: Vec<QueueTrack>, start_index: Option<usize>) {
        // Any queue replacement drops the offline-only-playlist stamp; the
        // local-playlist play path re-sets it right after when it applies.
        self.set_queue_offline_only(false);
        let queue = self.queue.write().await;
        queue.set_queue(tracks, start_index);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Replace queue contents and playback order atomically.
    pub async fn set_queue_with_order(
        &self,
        tracks: Vec<QueueTrack>,
        start_index: Option<usize>,
        shuffle_enabled: bool,
        shuffle_order: Option<Vec<usize>>,
    ) {
        self.set_queue_offline_only(false);
        let queue = self.queue.write().await;
        queue.set_queue_with_order(tracks, start_index, shuffle_enabled, shuffle_order);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
    }

    /// Remove a track by index
    pub async fn remove_track(&self, index: usize) -> Option<QueueTrack> {
        let queue = self.queue.write().await;
        let removed = queue.remove_track(index);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
        removed
    }

    /// Remove a track from the upcoming list by position
    pub async fn remove_upcoming_track(&self, upcoming_index: usize) -> Option<QueueTrack> {
        let queue = self.queue.write().await;
        let removed = queue.remove_upcoming_track(upcoming_index);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
        removed
    }

    /// Remove every upcoming track after `upcoming_index` in the current play
    /// order; the track at `upcoming_index` is kept. Shuffle-aware. Emits a
    /// single `QueueUpdated` when anything was removed. Returns the count.
    pub async fn remove_upcoming_after(&self, upcoming_index: usize) -> usize {
        let queue = self.queue.write().await;
        let removed = queue.remove_upcoming_after(upcoming_index);
        if removed > 0 {
            self.emit(CoreEvent::QueueUpdated {
                state: queue.get_state(),
            })
            .await;
        }
        removed
    }

    /// Move a track from one position to another
    pub async fn move_track(&self, from_index: usize, to_index: usize) -> bool {
        let queue = self.queue.write().await;
        let success = queue.move_track(from_index, to_index);
        if success {
            self.emit(CoreEvent::QueueUpdated {
                state: queue.get_state(),
            })
            .await;
        }
        success
    }

    /// Jump to a specific track by index
    pub async fn play_index(&self, index: usize) -> Option<QueueTrack> {
        let queue = self.queue.write().await;
        let track = queue.play_index(index);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
        track
    }

    /// Jump to a track by its position in the upcoming list (as shown in the
    /// Queue sidebar). Shuffle-aware: resolves through `shuffle_order` when
    /// shuffle is active.
    pub async fn play_upcoming_at(&self, upcoming_index: usize) -> Option<QueueTrack> {
        let queue = self.queue.write().await;
        let track = queue.play_upcoming_at(upcoming_index);
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
        track
    }

    /// Play `track_id` through the player's own L1/L2 → network path.
    pub async fn play_track_resolved(
        &self,
        track_id: u64,
        quality: Quality,
        start_position_secs: u64,
    ) -> Result<(), String> {
        let guard = self.client.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| "No Qobuz client available".to_string())?;
        self.player
            .play_track(client, track_id, quality, start_position_secs)
            .await
    }

    /// Resolve the bytes for a GAPLESS successor, L1/L2 → network. Returns
    /// bytes to hand to `Player::play_next`, or None.
    pub async fn fetch_for_gapless_resolved(
        &self,
        track_id: u64,
        quality: Quality,
    ) -> Option<qbz_player::TrackAudio> {
        let guard = self.client.read().await;
        let client = guard.as_ref()?;
        self.player
            .fetch_for_gapless(client, track_id, quality)
            .await
    }

    /// Advance to next track in queue
    pub async fn next_track(&self) -> Option<QueueTrack> {
        let queue = self.queue.write().await;
        let track = queue.next();
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
        track
    }

    /// Go to previous track in queue
    pub async fn previous_track(&self) -> Option<QueueTrack> {
        let queue = self.queue.write().await;
        let track = queue.previous();
        self.emit(CoreEvent::QueueUpdated {
            state: queue.get_state(),
        })
        .await;
        track
    }

    /// Get multiple upcoming tracks without advancing (for prefetching)
    pub async fn peek_upcoming(&self, count: usize) -> Vec<QueueTrack> {
        let queue = self.queue.read().await;
        queue.peek_upcoming(count)
    }

    /// Set the "stop after this song" marker on a queue track id. Replaces any
    /// previous marker (single marker). Silent no-op if the id is not in the queue.
    /// Intentionally does NOT emit `CoreEvent::QueueUpdated` — the marker is a UI
    /// intent the frontend reflects via its own queue snapshot, and emitting here
    /// risks QConnect echo loops.
    pub async fn set_stop_after(&self, track_id: u64) {
        self.queue.write().await.set_stop_after(track_id);
    }

    /// Clear the "stop after" marker (user cancellation).
    pub async fn clear_stop_after(&self) {
        self.queue.write().await.clear_stop_after();
    }

    /// Read the current "stop after" marker, if any.
    pub async fn get_stop_after(&self) -> Option<u64> {
        self.queue.read().await.get_stop_after()
    }

    /// One-shot consume: if `finished_track_id` matches the marker, clear it and
    /// return true (the auto-advance driver then halts instead of advancing).
    /// Only the natural end-of-track path may call this — never a manual skip.
    pub async fn consume_stop_after_if(&self, finished_track_id: u64) -> bool {
        self.queue
            .write()
            .await
            .consume_stop_after_if(finished_track_id)
    }

    /// Reconcile the queue pointer to the track the audio engine is actually
    /// playing. A gapless hand-off advances inside the player without going
    /// through `next_track`, so the core pointer can lag the live track and
    /// the now-playing card goes stale. This moves the pointer to the track
    /// with `id` and returns it plus whether the pointer moved; a queue
    /// update is emitted only when it did. Frontend-agnostic — the playback
    /// poll loop calls this to keep now-playing in sync (ADR-006).
    pub async fn sync_current_to_id(&self, id: u64) -> Option<(QueueTrack, bool)> {
        let queue = self.queue.write().await;
        let result = queue.sync_current_to_id(id);
        if matches!(result, Some((_, true))) {
            self.emit(CoreEvent::QueueUpdated {
                state: queue.get_state(),
            })
            .await;
        }
        result
    }

    // ==================== Search & Catalog ====================

    /// Get album by ID
    pub async fn get_album(&self, album_id: &str) -> Result<Album, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;

        client.get_album(album_id).await.map_err(CoreError::Api)
    }

    /// Get track by ID
    pub async fn get_track(&self, track_id: u64) -> Result<Track, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;

        client.get_track(track_id).await.map_err(CoreError::Api)
    }

    // ==================== Streaming ====================

    /// Get stream URL for a track with quality fallback
    pub async fn get_stream_url(
        &self,
        track_id: u64,
        quality: Quality,
    ) -> Result<StreamUrl, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;

        client
            .get_stream_url_with_fallback(track_id, quality)
            .await
            .map_err(CoreError::Api)
    }

    // ==================== Playback Operations ====================

    /// Pause playback
    pub fn pause(&self) -> Result<(), CoreError> {
        self.player.pause().map_err(CoreError::Playback)
    }

    /// Resume playback
    pub fn resume(&self) -> Result<(), CoreError> {
        self.player.resume().map_err(CoreError::Playback)
    }

    /// Stop playback
    pub fn stop(&self) -> Result<(), CoreError> {
        self.player.stop().map_err(CoreError::Playback)
    }

    /// Seek to position in seconds
    pub fn seek(&self, position: u64) -> Result<(), CoreError> {
        self.player.seek(position).map_err(CoreError::Playback)
    }

    /// Set volume (0.0 - 1.0)
    pub fn set_volume(&self, volume: f32) -> Result<(), CoreError> {
        self.player.set_volume(volume).map_err(CoreError::Playback)
    }

    /// Get current playback state
    pub fn get_playback_state(&self) -> PlaybackState {
        let state = &self.player.state;
        PlaybackState {
            is_playing: state.is_playing(),
            position: state.current_position(),
            duration: state.duration(),
            track_id: state.current_track_id(),
            volume: state.volume(),
        }
    }

    /// Get the player (for advanced usage)
    pub fn player(&self) -> Arc<Player> {
        Arc::clone(&self.player)
    }

    // ==================== Favorites ====================

    // ==================== Playlists ====================

    /// Get playlist by ID
    pub async fn get_playlist(&self, playlist_id: u64) -> Result<Playlist, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;

        client
            .get_playlist(playlist_id)
            .await
            .map_err(CoreError::Api)
    }

    /// Get tracks batch by IDs
    pub async fn get_tracks_batch(&self, track_ids: &[u64]) -> Result<Vec<Track>, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;

        client
            .get_tracks_batch(track_ids)
            .await
            .map_err(CoreError::Api)
    }

    /// Dynamic mix suggestions with the `track_to_analysed` payload — the
    /// PRIMARY DailyQ/WeeklyQ path (see `QobuzClient::get_dynamic_suggest_full`).
    pub async fn get_dynamic_suggest_full(
        &self,
        listened_track_ids: &[u64],
        tracks_to_analyse: &[TrackToAnalyse],
        limit: u32,
    ) -> Result<Vec<Track>, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;
        client
            .get_dynamic_suggest_full(listened_track_ids, tracks_to_analyse, limit)
            .await
            .map_err(CoreError::Api)
    }

    /// Get an artist's popular/top tracks (`/artist/get?extra=tracks`).
    pub async fn get_artist_tracks(
        &self,
        artist_id: u64,
        limit: u32,
        offset: u32,
    ) -> Result<TracksContainer, CoreError> {
        let client = self.client.read().await;
        let client = client.as_ref().ok_or(CoreError::NotInitialized)?;

        client
            .get_artist_tracks(artist_id, limit, offset)
            .await
            .map_err(CoreError::Api)
    }

    // ==================== Event Emission ====================

    /// Emit an event to the frontend adapter
    async fn emit(&self, event: CoreEvent) {
        self.adapter.on_event(event).await;
    }

    /// Get the Qobuz client (for advanced usage)
    pub fn client(&self) -> Arc<RwLock<Option<QobuzClient>>> {
        Arc::clone(&self.client)
    }
}

/// Normalize an artist name for dedupe: trim, lowercase, collapse
/// whitespace. Used by the discovery pipeline so "Iron  Maiden" and
/// "iron maiden" hash to the same key in the dismiss store.
pub fn normalize_artist_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use qbz_models::Artist;

    // --- D-FEAT featured-aware blacklist helpers ---

    use qbz_models::types::{AlbumArtist, AlbumSummary};
    use qbz_models::{DiscoverAlbumImage, DiscoverArtist};

    /// Empty album-blacklist filter for the artist-axis tests (the album axis
    /// is exercised by the dedicated album-id tests below).
    fn no_albums() -> AlbumBlacklistFilter {
        AlbumBlacklistFilter::new()
    }

    // Album and Track do not derive Default in qbz-models, so test fixtures
    // construct full struct literals. Only the artist-id fields are meaningful
    // to the blacklist helpers; everything else is zero/None filler.
    fn album_with_artists(primary_id: u64, featured_ids: &[u64]) -> Album {
        let artists = std::iter::once(AlbumArtist {
            id: primary_id,
            name: String::new(),
            roles: Some(vec!["main-artist".to_string()]),
        })
        .chain(featured_ids.iter().map(|&id| AlbumArtist {
            id,
            name: String::new(),
            roles: Some(vec!["featured-artist".to_string()]),
        }))
        .collect();
        Album {
            id: String::new(),
            title: String::new(),
            artist: Artist {
                id: primary_id,
                ..Default::default()
            },
            image: Default::default(),
            release_date_original: None,
            release_date_stream: None,
            streamable: None,
            label: None,
            genre: None,
            tracks_count: None,
            duration: None,
            hires: false,
            hires_streamable: false,
            maximum_sampling_rate: None,
            maximum_bit_depth: None,
            audio_info: None,
            dates: None,
            track_count: None,
            release_type: None,
            tracks: None,
            upc: None,
            description: None,
            goodies: None,
            awards: None,
            parental_warning: None,
            artists: Some(artists),
            composer: None,
            version: None,
        }
    }

    fn track_with(performer_id: Option<u64>, composer_id: Option<u64>) -> Track {
        Track {
            id: 0,
            title: String::new(),
            version: None,
            isrc: None,
            duration: 0,
            track_number: 0,
            media_number: None,
            performer: performer_id.map(|id| Artist {
                id,
                ..Default::default()
            }),
            album: None,
            hires: false,
            hires_streamable: false,
            maximum_sampling_rate: None,
            maximum_bit_depth: None,
            streamable: false,
            parental_warning: false,
            playlist_track_id: None,
            performers: None,
            composer: composer_id.map(|id| Artist {
                id,
                ..Default::default()
            }),
            copyright: None,
            work: None,
        }
    }

    #[test]
    fn album_blacklisted_blocks_on_primary_artist() {
        let album = album_with_artists(1, &[]);
        let bl: BlacklistFilter = [1].into_iter().collect();
        assert!(album_blacklisted(&album, &bl, &no_albums()));
    }

    #[test]
    fn album_blacklisted_blocks_on_featured_not_primary() {
        // Primary is 1 (kept), featured 999 is blocked.
        let album = album_with_artists(1, &[999]);
        let bl: BlacklistFilter = [999].into_iter().collect();
        assert!(album_blacklisted(&album, &bl, &no_albums()));
    }

    #[test]
    fn album_blacklisted_keeps_when_no_match() {
        let album = album_with_artists(1, &[2, 3]);
        let bl: BlacklistFilter = [999].into_iter().collect();
        assert!(!album_blacklisted(&album, &bl, &no_albums()));
    }

    #[test]
    fn album_blacklisted_empty_filter_is_false() {
        let album = album_with_artists(1, &[999]);
        let bl: BlacklistFilter = BlacklistFilter::new();
        assert!(!album_blacklisted(&album, &bl, &no_albums()));
    }

    #[test]
    fn track_blacklisted_blocks_on_performer() {
        let track = track_with(Some(5), None);
        let bl: BlacklistFilter = [5].into_iter().collect();
        assert!(track_blacklisted(&track, &bl, &no_albums()));
    }

    #[test]
    fn track_blacklisted_blocks_on_composer() {
        let track = track_with(Some(1), Some(7));
        let bl: BlacklistFilter = [7].into_iter().collect();
        assert!(track_blacklisted(&track, &bl, &no_albums()));
    }

    #[test]
    fn track_blacklisted_keeps_when_no_match() {
        let track = track_with(Some(1), Some(2));
        let bl: BlacklistFilter = [999].into_iter().collect();
        assert!(!track_blacklisted(&track, &bl, &no_albums()));
    }

    #[test]
    fn track_blacklisted_fail_open_when_no_ids() {
        // No performer + no composer => kept (fail-open).
        let track = track_with(None, None);
        let bl: BlacklistFilter = [1, 2, 3].into_iter().collect();
        assert!(!track_blacklisted(&track, &bl, &no_albums()));
    }

    #[test]
    fn track_blacklisted_empty_filter_is_false() {
        let track = track_with(Some(5), Some(7));
        let bl: BlacklistFilter = BlacklistFilter::new();
        assert!(!track_blacklisted(&track, &bl, &no_albums()));
    }

    #[test]
    fn discover_album_blacklisted_blocks_on_any_artist() {
        let album = DiscoverAlbum {
            id: String::new(),
            title: String::new(),
            version: None,
            track_count: None,
            duration: None,
            parental_warning: None,
            image: DiscoverAlbumImage {
                small: None,
                thumbnail: None,
                large: None,
            },
            artists: vec![
                DiscoverArtist {
                    id: 1,
                    name: String::new(),
                    roles: None,
                },
                DiscoverArtist {
                    id: 999,
                    name: String::new(),
                    roles: None,
                },
            ],
            label: None,
            genre: None,
            dates: None,
            audio_info: None,
            awards: None,
        };
        let blocked: BlacklistFilter = [999].into_iter().collect();
        assert!(discover_album_blacklisted(&album, &blocked, &no_albums()));
        let kept: BlacklistFilter = [555].into_iter().collect();
        assert!(!discover_album_blacklisted(&album, &kept, &no_albums()));
    }

    // --- Album-id axis (orthogonal to the artist axis) ---

    fn album_with_id(id: &str, primary_artist: u64) -> Album {
        let mut a = album_with_artists(primary_artist, &[]);
        a.id = id.to_string();
        a
    }

    fn track_with_album(album_id: &str) -> Track {
        let mut t = track_with(Some(1), Some(2));
        t.album = Some(AlbumSummary {
            id: album_id.to_string(),
            title: String::new(),
            image: Default::default(),
            label: None,
            genre: None,
        });
        t
    }

    #[test]
    fn album_blocked_by_own_id_regardless_of_artist() {
        // Artist filter EMPTY; only the album id is blocked. The widened
        // fail-open guard must NOT early-return here.
        let album = album_with_id("blk", 1);
        let abl: AlbumBlacklistFilter = ["blk".to_string()].into_iter().collect();
        assert!(album_blacklisted(&album, &BlacklistFilter::new(), &abl));
        let other: AlbumBlacklistFilter = ["zzz".to_string()].into_iter().collect();
        assert!(!album_blacklisted(&album, &BlacklistFilter::new(), &other));
    }

    #[test]
    fn album_blocked_keeps_sibling_album_of_same_artist() {
        // The merged-artist use case: blocking one album by id leaves the same
        // artist's other releases visible.
        let blocked_album = album_with_id("bad", 1);
        let good_album = album_with_id("good", 1);
        let abl: AlbumBlacklistFilter = ["bad".to_string()].into_iter().collect();
        assert!(album_blacklisted(
            &blocked_album,
            &BlacklistFilter::new(),
            &abl
        ));
        assert!(!album_blacklisted(
            &good_album,
            &BlacklistFilter::new(),
            &abl
        ));
    }

    #[test]
    fn track_blocked_by_album_id() {
        let track = track_with_album("blk");
        let abl: AlbumBlacklistFilter = ["blk".to_string()].into_iter().collect();
        assert!(track_blacklisted(&track, &BlacklistFilter::new(), &abl));
        // No album / different id => kept.
        assert!(!track_blacklisted(
            &track_with(Some(1), None),
            &BlacklistFilter::new(),
            &abl
        ));
    }

    #[test]
    fn discover_album_blocked_by_own_id() {
        let mut album = DiscoverAlbum {
            id: "blk".to_string(),
            title: String::new(),
            version: None,
            track_count: None,
            duration: None,
            parental_warning: None,
            image: DiscoverAlbumImage {
                small: None,
                thumbnail: None,
                large: None,
            },
            artists: vec![DiscoverArtist {
                id: 1,
                name: String::new(),
                roles: None,
            }],
            label: None,
            genre: None,
            dates: None,
            audio_info: None,
            awards: None,
        };
        let abl: AlbumBlacklistFilter = ["blk".to_string()].into_iter().collect();
        assert!(discover_album_blacklisted(
            &album,
            &BlacklistFilter::new(),
            &abl
        ));
        album.id = "other".to_string();
        assert!(!discover_album_blacklisted(
            &album,
            &BlacklistFilter::new(),
            &abl
        ));
    }
}
