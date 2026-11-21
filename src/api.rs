//! Thin async wrapper around `rspotify` for the browse/search operations the
//! TUI needs. Converts Spotify model types into the UI's simple `Track` type.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rspotify::{AuthCodePkceSpotify, ClientError};
use rspotify::clients::{BaseClient, OAuthClient};
use rspotify::model::{FullTrack, Image as SpotifyImage, PlayableItem, PlaylistId, SearchResult, SearchType};
use rspotify::prelude::*;

use crate::ui::{Playlist, Track};

/// Turn rspotify's error into an actionable message for the status bar.
fn map_spotify_error(err: ClientError) -> anyhow::Error {
    if err.to_string().contains("status code 429") {
        anyhow!("Spotify API rate limit exceeded; wait a minute and retry")
    } else {
        anyhow!(err)
    }
}

/// Choose the most useful album-art URL: the 300px variant when Spotify
/// supplies one, otherwise the largest available image.
fn pick_art_url(images: &[SpotifyImage]) -> Option<String> {
    if let Some(image) = images.iter().find(|image| image.width == Some(300)) {
        return Some(image.url.clone());
    }
    images
        .iter()
        .max_by_key(|image| image.width.unwrap_or(0))
        .map(|image| image.url.clone())
}

/// Convert an rspotify `FullTrack` into the UI's `Track`.
pub fn track_from_full(track: &FullTrack) -> Option<Track> {
    let id = track.id.as_ref()?;
    let artist = track
        .artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "Unknown artist".to_string());
    Some(Track {
        title: track.name.clone(),
        artist,
        album: track.album.name.clone(),
        duration: track.duration.to_std().unwrap_or(Duration::ZERO),
        uri: format!("spotify:track:{}", id.id()),
        art_url: pick_art_url(&track.album.images),
    })
}

fn track_from_playable(item: &PlayableItem) -> Option<Track> {
    match item {
        PlayableItem::Track(track) => track_from_full(track),
        _ => None, // episodes and unknown items are not playable via librespot
    }
}

/// Search Spotify tracks by keyword (up to 10 results per Spotify's 2026 limit).
pub async fn search_tracks(client: &AuthCodePkceSpotify, query: &str) -> Result<Vec<Track>> {
    let result = client
        .search(query, SearchType::Track, None, None, Some(10), None)
        .await
        .map_err(map_spotify_error)
        .context("search request failed")?;
    match result {
        SearchResult::Tracks(page) => Ok(page.items.iter().filter_map(track_from_full).collect()),
        _ => Ok(Vec::new()),
    }
}

/// Fetch the current user's playlists (first page, up to 50).
pub async fn current_user_playlists(client: &AuthCodePkceSpotify) -> Result<Vec<Playlist>> {
    let page = client
        .current_user_playlists_manual(Some(50), None)
        .await
        .map_err(map_spotify_error)
        .context("fetching playlists failed")?;
    Ok(page
        .items
        .into_iter()
        .map(|p| Playlist {
            id: p.id.id().to_string(),
            name: p.name,
            description: String::new(),
            track_count: Some(p.items.total),
        })
        .collect())
}

/// Fetch the tracks in a playlist by its raw ID (first page, up to 100).
pub async fn playlist_tracks(client: &AuthCodePkceSpotify, playlist_id: &str) -> Result<Vec<Track>> {
    let id = PlaylistId::from_id_or_uri(playlist_id)
        .with_context(|| format!("invalid playlist ID/URI: {playlist_id}"))?;
    let page = client
        .playlist_items_manual(id, None, None, Some(100), None)
        .await
        .map_err(map_spotify_error)
        .context("fetching playlist tracks failed")?;
    Ok(page
        .items
        .iter()
        .filter_map(|item| item.item.as_ref().and_then(track_from_playable))
        .collect())
}

/// Fetch the current user's liked songs (first page, up to 50).
pub async fn liked_songs(client: &AuthCodePkceSpotify) -> Result<Vec<Track>> {
    let page = client
        .current_user_saved_tracks_manual(None, Some(50), None)
        .await
        .map_err(map_spotify_error)
        .context("fetching liked songs failed")?;
    Ok(page
        .items
        .iter()
        .filter_map(|item| track_from_full(&item.track))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, url: &str) -> SpotifyImage {
        SpotifyImage {
            height: Some(width),
            url: url.to_string(),
            width: Some(width),
        }
    }

    #[test]
    fn prefers_300px_art_and_falls_back_to_largest() {
        let images = vec![
            image(64, "small"),
            image(640, "large"),
            image(300, "preferred"),
        ];
        assert_eq!(pick_art_url(&images).as_deref(), Some("preferred"));

        let images = vec![image(64, "small"), image(640, "large")];
        assert_eq!(pick_art_url(&images).as_deref(), Some("large"));
        assert_eq!(pick_art_url(&[]), None);
    }
}


