//! SoundCloud resources retain their own identifiers and attribution.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    #[serde(default)]
    pub urn: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub permalink_url: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Playable,
    Preview,
    Blocked,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    #[serde(default)]
    pub urn: String,
    #[serde(default)]
    pub title: String,
    /// Milliseconds, as returned by the API.
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub permalink_url: String,
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub metadata_artist: Option<String>,
    #[serde(default)]
    pub streamable: bool,
    #[serde(default)]
    pub access: Access,
    #[serde(default)]
    pub user_favorite: bool,
}

impl Track {
    pub fn creator(&self) -> &str {
        self.metadata_artist
            .as_deref()
            .filter(|artist| !artist.trim().is_empty())
            .unwrap_or(&self.user.username)
    }

    pub fn is_playable(&self) -> bool {
        self.streamable && self.access == Access::Playable
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    #[serde(default)]
    pub urn: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub sharing: String,
    #[serde(default)]
    pub permalink_url: String,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub track_count: usize,
    /// A playlist response can include an incomplete track list. Fetch
    /// `/playlists/{urn}/tracks` before a structural edit.
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct Page<T> {
    #[serde(default)]
    pub collection: Vec<T>,
    #[serde(default)]
    pub next_href: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub user: User,
    pub liked_tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
    pub liked_playlists: Vec<Playlist>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamUrls {
    #[serde(default)]
    pub hls_aac_160_url: Option<String>,
    #[serde(default)]
    pub hls_aac_96_url: Option<String>,
    #[serde(default)]
    pub preview_mp3_128_url: Option<String>,
}

impl StreamUrls {
    /// A preview is never returned as a substitute for a complete song.
    pub fn preferred(&self) -> Option<&str> {
        self.hls_aac_160_url
            .as_deref()
            .filter(|url| !url.is_empty())
            .or_else(|| self.hls_aac_96_url.as_deref().filter(|url| !url.is_empty()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Resolved {
    Track(Track),
    Playlist(Playlist),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_and_unknown_access_are_never_advertised_as_full_songs() {
        let mut track = Track {
            streamable: true,
            access: Access::Preview,
            ..Default::default()
        };
        assert!(!track.is_playable());
        track.access = Access::Playable;
        assert!(track.is_playable());
        let streams = StreamUrls {
            preview_mp3_128_url: Some("https://example.test/preview".into()),
            ..Default::default()
        };
        assert!(streams.preferred().is_none());
    }

    #[test]
    fn missing_artist_falls_back_to_uploader_without_losing_uploader_credit() {
        let track = Track {
            metadata_artist: Some(" ".into()),
            user: User {
                username: "Uploader".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(track.creator(), "Uploader");
    }
}
