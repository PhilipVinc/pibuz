//! Core API types for QBZ
//!
//! This module contains all shared data types used across the application:
//! - Media types: Track, Album, Artist, Playlist
//! - Quality/streaming types
//! - Search and favorites types
//! - Image and metadata types

use serde::{Deserialize, Serialize};

// ============ Dynamic-suggest (DailyQ/WeeklyQ) ============

/// A seed track resolved for the `/dynamic/suggest` `track_to_analysed`
/// payload (DailyQ / WeeklyQ). Field names match the Qobuz wire shape
/// exactly; `0` marks an unknown id (mirrors Tauri's `?? 0`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackToAnalyse {
    pub track_id: u64,
    pub artist_id: u64,
    pub genre_id: u64,
    pub label_id: u64,
}

// ============ Quality Types ============

/// Audio quality format IDs (matches Qobuz API format IDs)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u32)]
#[derive(Default)]
pub enum Quality {
    Mp3 = 5,
    #[default]
    Lossless = 6, // 16-bit/44.1kHz (CD Quality)
    HiRes = 7,       // 24-bit/≤96kHz
    UltraHiRes = 27, // 24-bit/>96kHz
}

impl Quality {
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            5 => Some(Quality::Mp3),
            6 => Some(Quality::Lossless),
            7 => Some(Quality::HiRes),
            27 => Some(Quality::UltraHiRes),
            _ => None,
        }
    }

    pub fn id(&self) -> u32 {
        *self as u32
    }

    pub fn label(&self) -> &'static str {
        match self {
            Quality::Mp3 => "MP3 320kbps",
            Quality::Lossless => "FLAC 16-bit/44.1kHz",
            Quality::HiRes => "FLAC 24-bit/≤96kHz",
            Quality::UltraHiRes => "FLAC 24-bit/>96kHz",
        }
    }

    /// Quality levels in descending order for fallback
    pub fn fallback_order() -> &'static [Quality] {
        &[
            Quality::UltraHiRes,
            Quality::HiRes,
            Quality::Lossless,
            Quality::Mp3,
        ]
    }
}

// ============ User Session ============

/// User credentials and session info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSession {
    pub user_auth_token: String,
    pub user_id: u64,
    pub email: String,
    pub display_name: String,
    pub subscription_label: String,
    #[serde(default)]
    pub subscription_valid_until: Option<String>,
    /// Account territory (ISO 3166-1 alpha-2, e.g. "FR") from the login
    /// response. `serde(default)` keeps pre-v10 persisted sessions loadable.
    #[serde(default)]
    pub country_code: Option<String>,
    /// Account language (ISO 639-1, e.g. "fr") from the login response —
    /// the default target for lyrics translation ("Auto").
    #[serde(default)]
    pub language_code: Option<String>,
}

// ============ Stream Types ============

/// Stream URL response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamUrl {
    pub url: String,
    pub format_id: u32,
    pub mime_type: String,
    pub sampling_rate: f64,
    pub bit_depth: Option<u32>,
    pub track_id: u64,
    pub restrictions: Vec<StreamRestriction>,
}

impl StreamUrl {
    /// Check if the stream has restrictions that prevent playback
    pub fn has_restrictions(&self) -> bool {
        self.restrictions.iter().any(|r| {
            r.code == "FormatRestrictedByFormatAvailability"
                || r.code == "SampleRestrictedByRightHolders"
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRestriction {
    pub code: String,
}

// ============ External streaming (Cast / DLNA) ============

/// Resolved audio quality actually delivered for an external stream, in the
/// kHz convention used across the catalog and [`StreamUrl`]. Surfaced so the
/// UI can show the REAL quality of a cast stream, which can fall back below
/// the requested tier (HiRes -> Lossless -> Mp3) without the user knowing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamQualityInfo {
    /// Qobuz format id: 5=MP3, 6=Lossless, 7=HiRes, 27=UltraHiRes.
    pub format_id: u32,
    /// Sampling rate in kHz (e.g. 96.0, 192.0), when known.
    pub sampling_rate_khz: Option<f64>,
    /// Bit depth (16 / 24), when known.
    pub bit_depth: Option<u32>,
}

impl StreamQualityInfo {
    /// Build from a raw sampling-rate value whose unit may be kHz or Hz
    /// depending on the Qobuz endpoint (`get_stream_url` reports kHz as f64,
    /// `file/url` reports an integer that has been observed as kHz). Normalize
    /// to kHz robustly: any real audio rate is < 1000 kHz and >= 8000 Hz, so a
    /// value >= 1000 is Hz and gets divided. Zero/negative -> unknown.
    pub fn from_raw(format_id: u32, raw_rate: Option<f64>, bit_depth: Option<u32>) -> Self {
        let sampling_rate_khz = raw_rate.and_then(|r| {
            if r <= 0.0 {
                None
            } else if r >= 1000.0 {
                Some(r / 1000.0)
            } else {
                Some(r)
            }
        });
        Self {
            format_id,
            sampling_rate_khz,
            bit_depth,
        }
    }

    /// The `Quality` tier this format id maps to, if recognized.
    pub fn quality(&self) -> Option<Quality> {
        Quality::from_id(self.format_id)
    }

    /// Coarse tier label like "FLAC 24-bit/>96kHz" (from the format id).
    pub fn tier_label(&self) -> &'static str {
        self.quality().map(|q| q.label()).unwrap_or("Unknown")
    }
}

// ============ CMAF Stream Types ============

/// Response from POST /api.json/0.2/session/start
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartResponse {
    pub session_id: String,
    pub expires_at: u64,
    #[serde(default)]
    pub infos: Option<String>,
}

/// Response from GET /api.json/0.2/file/url (CMAF segmented streaming)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackFileUrl {
    #[serde(default)]
    pub url_template: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub n_segments: u8,
    #[serde(default)]
    pub key_id: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub sampling_rate: Option<u32>,
    #[serde(default)]
    pub bit_depth: Option<u32>,
    #[serde(default)]
    pub bits_depth: Option<u32>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub n_samples: Option<u64>,
    #[serde(default)]
    pub format_id: Option<u32>,
    #[serde(default)]
    pub track_id: Option<u64>,
    #[serde(default)]
    pub restrictions: Vec<StreamRestriction>,
}

// ============ Image Types ============

/// Image set with multiple resolutions
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageSet {
    pub small: Option<String>,
    pub thumbnail: Option<String>,
    pub large: Option<String>,
    pub extralarge: Option<String>,
    pub mega: Option<String>,
    pub back: Option<String>,
}

impl ImageSet {
    pub fn best(&self) -> Option<&String> {
        self.mega
            .as_ref()
            .or(self.extralarge.as_ref())
            .or(self.large.as_ref())
            .or(self.thumbnail.as_ref())
            .or(self.small.as_ref())
    }
}

// ============ Core Media Types ============

/// Track model
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Track {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub title: String,
    /// Subtitle/edition info from Qobuz (e.g. "Player's Ball Mix",
    /// "Nine Inch Noize Version", "Remastered 2024"). Frontend renders
    /// it parenthesized after the title so remix and reissue albums are
    /// distinguishable from originals (issue #360).
    pub version: Option<String>,
    /// Classical "work" the track belongs to (e.g. "Symphony No. 9 in D minor,
    /// Op. 125"). Qobuz returns it on the track object (always present in the
    /// envelope, `null` for non-classical catalog). Drives the per-work section
    /// headers on the album view, mirroring the official Qobuz player (PR #536).
    pub work: Option<String>,
    pub isrc: Option<String>,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub track_number: u32,
    pub media_number: Option<u32>,
    pub performer: Option<Artist>,
    pub album: Option<AlbumSummary>,
    #[serde(default)]
    pub hires: bool,
    #[serde(default)]
    pub hires_streamable: bool,
    pub maximum_sampling_rate: Option<f64>,
    pub maximum_bit_depth: Option<u32>,
    #[serde(default)]
    pub streamable: bool,
    #[serde(default)]
    pub parental_warning: bool,
    /// Playlist-specific: ID within the playlist (for removal)
    pub playlist_track_id: Option<u64>,
    /// Performers/credits string (format: "Name, Role - Name, Role")
    pub performers: Option<String>,
    /// Composer information
    pub composer: Option<Artist>,
    /// Copyright information
    pub copyright: Option<String>,
}

/// Album summary (embedded in track responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumSummary {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub image: ImageSet,
    /// Label (if returned in track response)
    pub label: Option<Label>,
    /// Genre (when returned, e.g. on favorites track album objects).
    pub genre: Option<Genre>,
}

/// Album model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: Artist,
    #[serde(default)]
    pub image: ImageSet,
    pub release_date_original: Option<String>,
    /// Date the album becomes available for streaming (ISO YYYY-MM-DD).
    /// When in the future, the album is upcoming and cannot be fetched
    /// via `get_album` yet — Release Watch uses this to gate clicks.
    pub release_date_stream: Option<String>,
    /// Whether the album is currently streamable. False for upcoming
    /// releases, regional restrictions, or label takedowns.
    #[serde(default)]
    pub streamable: Option<bool>,
    pub label: Option<Label>,
    pub genre: Option<Genre>,
    pub tracks_count: Option<u32>,
    pub duration: Option<u32>,
    #[serde(default)]
    pub hires: bool,
    #[serde(default)]
    pub hires_streamable: bool,
    pub maximum_sampling_rate: Option<f64>,
    pub maximum_bit_depth: Option<u32>,
    /// V2 nested quality block. The modern album shape returned by
    /// `/label/getAlbums` (DiscographyAlbumDto) and `/discover`-style items
    /// nests quality here; preferred over the flat `maximum_*` fields.
    #[serde(default)]
    pub audio_info: Option<DiscoverAudioInfo>,
    /// V2 nested release dates (`{original, download, stream}`); preferred
    /// over the flat `release_date_original` when present.
    #[serde(default)]
    pub dates: Option<DiscoverAlbumDates>,
    /// The V2 wire spells the album track count `track_count` (no trailing
    /// `s`); the flat shape uses `tracks_count`.
    #[serde(default)]
    pub track_count: Option<u32>,
    /// Explicit release type when provided ("album" | "ep" | "single" |
    /// "live" | "compilation" | ...).
    #[serde(default)]
    pub release_type: Option<String>,
    #[serde(default)]
    pub tracks: Option<TracksContainer>,
    /// Universal Product Code for the album
    pub upc: Option<String>,
    /// Editorial description/review of the album
    pub description: Option<String>,
    /// Album goodies (booklets, liner notes PDFs)
    #[serde(default)]
    pub goodies: Option<Vec<Goody>>,
    /// Editorial awards (Qobuzissime, Album of the Week, press accolades).
    #[serde(default)]
    pub awards: Option<Vec<AlbumAward>>,
    /// Parental advisory / explicit content marker.
    #[serde(default)]
    pub parental_warning: Option<bool>,
    /// Full artist contributor list including roles. The primary artist is
    /// duplicated here as `roles: ["main-artist"]`; non-main entries are
    /// the album's featured artists.
    #[serde(default)]
    pub artists: Option<Vec<AlbumArtist>>,
    /// Release variant label ("2009 Remaster", "Hi-Res", "Deluxe Edition", …).
    /// Qobuz keeps this out of `title`; the web player appends it in parens so
    /// re-editions of the same album are distinguishable. Surfaced the same way
    /// on every album title (see `format_album_title`).
    #[serde(default)]
    pub version: Option<String>,
    /// Album-level composer credit (single Artist). The official web player
    /// renders this — NOT the per-track `composer` — as the "… • X
    /// (composer)" tail of the header credit line, and suppresses it when the
    /// name is the "Various Composers" placeholder. See `album::build_credits`.
    #[serde(default)]
    pub composer: Option<Artist>,
}

/// Album artist contributor entry (main artist + featured artists).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumArtist {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub roles: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracksContainer {
    pub items: Vec<Track>,
    pub total: u32,
}

/// Artist model
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Artist {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub image: Option<ImageSet>,
    #[serde(default)]
    pub albums_count: Option<u32>,
    /// Biography (available when fetching full artist details)
    #[serde(default)]
    pub biography: Option<ArtistBiography>,
    /// Albums (available when fetching with extra=albums)
    #[serde(default)]
    pub albums: Option<ArtistAlbums>,
    /// Tracks where this artist appears (extra=tracks_appears_on)
    #[serde(default)]
    pub tracks_appears_on: Option<TracksContainer>,
    /// Curated playlists for this artist (extra=playlists)
    #[serde(default)]
    pub playlists: Option<Vec<Playlist>>,
}

/// Playlist model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub owner: PlaylistOwner,
    pub images: Option<Vec<String>>,
    #[serde(default)]
    pub tracks_count: u32,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub is_public: bool,
    #[serde(default)]
    pub tracks: Option<TracksContainer>,
    pub genres: Option<Vec<PlaylistGenre>>,
    pub images150: Option<Vec<String>>,
    pub images300: Option<Vec<String>>,
    pub slug: Option<String>,
    pub users_count: Option<u32>,
}

// ============ Metadata Types ============

/// Label model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub id: u64,
    pub name: String,
}

// ============ Label Page Types (/label/page) ============

// ============ Award Page Types (/award/page) ============

// ============ Label Sub-resource Types (v9.7.0.3 API) ============
//
// The label page (/label/page) returns an aggregated snapshot; the
// getAlbums / getPlaylists / getTopArtists / getNextReleases /
// getAwardedReleases endpoints return paginated lists for each
// sub-resource. Per Qobuz convention these use the V2 list envelope
// { has_more, items: [...] }. Deserialized shapes are best-effort: if
// the server wraps items in e.g. { albums: { items: ... } }, the
// Optional fallbacks still keep the call non-fatal.

// ============ Search Types ============

/// Search results container
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub albums: Option<SearchResultsPage<Album>>,
    pub tracks: Option<SearchResultsPage<Track>>,
    pub artists: Option<SearchResultsPage<Artist>>,
    pub playlists: Option<SearchResultsPage<Playlist>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResultsPage<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    // `/album/suggest` returns a page with only `{limit, items}` (no `total`
    // or `offset`); without defaults the whole response failed to deserialize
    // and the album "Suggestions" carousel silently never showed. Defaulting
    // the pagination scalars to 0 is harmless — only `items` is consumed there.
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub limit: u32,
}

// ============ Purchases API Models ============
//
// The wire shapes returned by the Qobuz `/purchase/*` endpoints. The lenient
// deserializers live in `crate::purchase_serde` (see that module's docs for the
// per-field coercion rules).

/// Response from `/purchase/getUserPurchases` (commands #1/#3/#4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseResponse {
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_page")]
    pub albums: SearchResultsPage<PurchaseAlbum>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_page")]
    pub tracks: SearchResultsPage<PurchaseTrack>,
}

/// Response from `/purchase/getUserPurchasesIds` (command #2). Items are OPAQUE
/// JSON — the UI reads only `.total` from each page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseIdsResponse {
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_page")]
    pub albums: SearchResultsPage<serde_json::Value>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_page")]
    pub tracks: SearchResultsPage<serde_json::Value>,
}

/// A purchased album. `downloadable` defaults TRUE; `downloaded` is NOT from
/// Qobuz — it is server-computed from the local registry. `purchased_at` is
/// unix epoch seconds. Nested `tracks` is populated only on the album-detail /
/// by-type-albums paths.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PurchaseAlbum {
    #[serde(
        default,
        deserialize_with = "crate::purchase_serde::deserialize_string_id"
    )]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: Artist,
    #[serde(default)]
    pub image: ImageSet,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub release_date_original: Option<String>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub label: Option<Label>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub genre: Option<Genre>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub tracks_count: Option<u32>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub duration: Option<u32>,
    #[serde(default)]
    pub hires: bool,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub maximum_sampling_rate: Option<f64>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub maximum_bit_depth: Option<u32>,
    #[serde(default = "crate::purchase_serde::serde_true")]
    pub downloadable: bool,
    #[serde(default)]
    pub downloaded: bool,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub purchased_at: Option<i64>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub tracks: Option<SearchResultsPage<PurchaseTrack>>,
}

/// A purchased track. NOTE: there is intentionally **no `version` field** — the
/// purchases path never carries the track subtitle/edition (see source-of-truth
/// §4.6). `streamable` defaults TRUE; `downloaded`/`downloaded_format_ids` are
/// server-computed from the local registry; `media_number` is the disc number
/// used for disc-grouping. `purchased_at` is unix epoch seconds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PurchaseTrack {
    #[serde(
        default,
        deserialize_with = "crate::purchase_serde::deserialize_u64_id"
    )]
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub track_number: u32,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub media_number: Option<u32>,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub performer: Artist,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub album: Option<AlbumSummary>,
    #[serde(default)]
    pub hires: bool,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub maximum_sampling_rate: Option<f64>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub maximum_bit_depth: Option<u32>,
    #[serde(default = "crate::purchase_serde::serde_true")]
    pub streamable: bool,
    #[serde(default)]
    pub downloaded: bool,
    #[serde(default)]
    pub downloaded_format_ids: Vec<u32>,
    #[serde(default, deserialize_with = "crate::purchase_serde::lenient_option")]
    pub purchased_at: Option<i64>,
}

/// Response from the `/radio/*` endpoints — a generated track list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioResponse {
    #[serde(rename = "type", default)]
    pub radio_type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::purchase_serde::lenient_page_flexible"
    )]
    pub tracks: SearchResultsPage<Track>,
}

/// Favorites container
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favorites {
    pub albums: Option<SearchResultsPage<Album>>,
    pub tracks: Option<SearchResultsPage<Track>>,
    pub artists: Option<SearchResultsPage<Artist>>,
}

// ============ Discover API Types ============

/// Album from discover endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverAlbum {
    pub id: String,
    pub title: String,
    pub version: Option<String>,
    pub track_count: Option<u32>,
    pub duration: Option<u32>,
    pub parental_warning: Option<bool>,
    pub image: DiscoverAlbumImage,
    pub artists: Vec<DiscoverArtist>,
    pub label: Option<Label>,
    pub genre: Option<Genre>,
    pub dates: Option<DiscoverAlbumDates>,
    pub audio_info: Option<DiscoverAudioInfo>,
    /// Editorial awards attached to the album. Id 88 = Qobuzissime,
    /// id 151 = Qobuz Album of the Week (locale-stable).
    #[serde(default)]
    pub awards: Option<Vec<AlbumAward>>,
}

/// Album image from discover endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverAlbumImage {
    pub small: Option<String>,
    pub thumbnail: Option<String>,
    pub large: Option<String>,
}

/// Artist in discover album
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverArtist {
    pub id: u64,
    pub name: String,
    pub roles: Option<Vec<String>>,
}

// ============ Artist Page Types (/artist/page) ============

fn deserialize_award_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

fn deserialize_award_awarded_at<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

// ============ Artist Story Types (/artist/story) ============

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_session_deserializes_pre_v10_json() {
        // Sessions persisted before the country/language capture must still
        // load: both new fields default to None (feature stays Auto-off).
        let json = r#"{
            "user_auth_token": "token",
            "user_id": 1705826,
            "email": "a@b.c",
            "display_name": "Tester",
            "subscription_label": "Studio",
            "subscription_valid_until": null
        }"#;
        let session: UserSession = serde_json::from_str(json).expect("old session json loads");
        assert_eq!(session.country_code, None);
        assert_eq!(session.language_code, None);
        assert_eq!(session.user_id, 1705826);
    }

    #[test]
    fn user_session_round_trips_country_and_language() {
        let json = r#"{
            "user_auth_token": "token",
            "user_id": 1705826,
            "email": "a@b.c",
            "display_name": "Tester",
            "subscription_label": "Studio",
            "subscription_valid_until": null,
            "country_code": "FR",
            "language_code": "fr"
        }"#;
        let session: UserSession = serde_json::from_str(json).expect("v10 session json loads");
        assert_eq!(session.country_code.as_deref(), Some("FR"));
        assert_eq!(session.language_code.as_deref(), Some("fr"));
        let back: UserSession =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(back.language_code.as_deref(), Some("fr"));
    }
}

/// Award attached to an album. Shape is intentionally lenient because
/// Qobuz uses three different embedded shapes across endpoints:
/// - `/discover/index` — {id: int, name, awarded_at: "YYYY-MM-DD"}
/// - `/album/get`      — LegacyAwardDto {awardId: string, name,
///   publicationId, publicationName, awardSlug,
///   awardedAt: long, …}
/// - `/artist/page`    — PageArtistAward {id: int, name, awarded_at}
///   id is emitted as String downstream so the frontend has a single
///   type to carry into /award/page and /award/getAlbums. The `alias`
///   list covers the LegacyAwardDto field name the web app never sees
///   but the mobile API uses.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlbumAward {
    #[serde(
        default,
        alias = "awardId",
        alias = "award_id",
        deserialize_with = "deserialize_award_id"
    )]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(
        default,
        alias = "awardedAt",
        deserialize_with = "deserialize_award_awarded_at"
    )]
    pub awarded_at: Option<String>,
}

/// Artist albums container
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistAlbums {
    pub items: Vec<Album>,
    pub total: u32,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub limit: u32,
}

/// Artist biography content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistBiography {
    pub summary: Option<String>,
    pub content: Option<String>,
    pub source: Option<String>,
}

/// Album dates from discover
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverAlbumDates {
    pub download: Option<String>,
    pub original: Option<String>,
    pub stream: Option<String>,
}

/// Audio info from discover album
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverAudioInfo {
    pub maximum_sampling_rate: Option<f64>,
    pub maximum_bit_depth: Option<u32>,
    pub maximum_channel_count: Option<u32>,
}

/// Genre model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Genre {
    pub id: u64,
    pub name: String,
    /// Full ancestor id chain (top-level first, self last) as sent by the
    /// discover endpoints. Absent on older cached payloads → None.
    #[serde(default)]
    pub path: Option<Vec<u64>>,
}

/// A downloadable extra bundled with an album (e.g. PDF booklet)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goody {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    /// Original (full-size) URL
    #[serde(default)]
    pub original_url: String,
    /// File format id (e.g. 21 for PDF)
    #[serde(default)]
    pub file_format_id: Option<u32>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistGenre {
    pub id: u64,
    pub name: String,
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlaylistOwner {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
}
