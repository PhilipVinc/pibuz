//! API endpoint definitions.
//!
//! The renderer's surface: log in, resolve a track / album / playlist, open a
//! CMAF session and get a file URL.

pub(crate) const BASE_URL: &str = "https://www.qobuz.com/api.json/0.2";

/// Endpoint paths
pub(crate) mod paths {
    // User

    // Track
    pub(crate) const TRACK_GET: &str = "/track/get";
    pub(crate) const TRACK_GET_LIST: &str = "/track/getList";
    pub(crate) const TRACK_GET_FILE_URL: &str = "/track/getFileUrl";

    // Album
    pub(crate) const ALBUM_GET: &str = "/album/get";

    // Artist
    pub(crate) const ARTIST_GET: &str = "/artist/get";

    // Playlist
    pub(crate) const PLAYLIST_GET: &str = "/playlist/get";

    // Session (CMAF streaming)
    pub(crate) const SESSION_START: &str = "/session/start";

    // File (CMAF streaming)
    pub(crate) const FILE_URL: &str = "/file/url";
}

/// Build full URL for an endpoint
pub fn build_url(endpoint: &str) -> String {
    format!("{}{}", BASE_URL, endpoint)
}
