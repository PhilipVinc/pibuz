//! API endpoint definitions.
//!
//! What remains is the renderer's surface: log in, resolve a track / album /
//! playlist, open a CMAF session and get a file URL. The catalog paths — search,
//! discover, awards, labels, radio, favorites, playlist editing, lyrics — went
//! with the methods that called them when the account path was removed.

pub const BASE_URL: &str = "https://www.qobuz.com/api.json/0.2";

/// Endpoint paths
pub mod paths {
    // User
    pub const USER_LOGIN: &str = "/user/login";

    // Track
    pub const TRACK_GET: &str = "/track/get";
    pub const TRACK_GET_LIST: &str = "/track/getList";
    pub const TRACK_GET_FILE_URL: &str = "/track/getFileUrl";

    // Album
    pub const ALBUM_GET: &str = "/album/get";
    pub const DYNAMIC_SUGGEST: &str = "/dynamic/suggest";

    // Artist
    pub const ARTIST_GET: &str = "/artist/get";

    // Playlist
    pub const PLAYLIST_GET: &str = "/playlist/get";

    // Session (CMAF streaming)
    pub const SESSION_START: &str = "/session/start";

    // File (CMAF streaming)
    pub const FILE_URL: &str = "/file/url";
}

/// Build full URL for an endpoint
pub fn build_url(endpoint: &str) -> String {
    format!("{}{}", BASE_URL, endpoint)
}
