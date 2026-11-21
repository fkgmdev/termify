//! Thin async wrapper around `rspotify` for the browse/search and playlist
//! editing operations the TUI needs. Converts Spotify model types into the
//! UI's simple `Track` and `Playlist` types.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rspotify::{AuthCodePkceSpotify, ClientError};
use rspotify::clients::{BaseClient, OAuthClient};
use rspotify::model::{
    FullPlaylist, FullTrack, Image as SpotifyImage, PlayableId, PlayableItem, PlaylistId,
    PlaylistItem, PublicUser, SearchResult, SearchType, SimplifiedPlaylist, TrackId,
};
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

/// Spotify often omits `display_name` for older playlist items; the stable ID
/// is still a useful attribution.
fn user_name(user: &PublicUser) -> String {
    user.display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| user.id.id().to_string())
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
        added_by: None,
    })
}

fn track_from_playable(item: &PlayableItem) -> Option<Track> {
    match item {
        PlayableItem::Track(track) => track_from_full(track),
        _ => None, // episodes and unknown items are not playable via librespot
    }
}

/// Playlist items carry the user who added them; attach that to the track so
/// the UI can show per-song attribution.
fn track_from_playlist_item(item: &PlaylistItem) -> Option<Track> {
    let mut track = track_from_playable(item.item.as_ref()?)?;
    track.added_by = item.added_by.as_ref().map(user_name);
    Some(track)
}

fn playlist_from_simplified(playlist: &SimplifiedPlaylist) -> Playlist {
    Playlist {
        id: playlist.id.id().to_string(),
        name: playlist.name.clone(),
        description: String::new(), // not present on simplified playlists
        track_count: Some(playlist.items.total),
        owner: Some(user_name(&playlist.owner)),
        collaborative: playlist.collaborative,
        is_public: playlist.public,
        contributors: Vec::new(), // filled in when the playlist is opened
        snapshot_id: playlist.snapshot_id.clone(),
    }
}

fn playlist_from_full(playlist: &FullPlaylist, contributors: Vec<String>) -> Playlist {
    Playlist {
        id: playlist.id.id().to_string(),
        name: playlist.name.clone(),
        description: playlist.description.clone().unwrap_or_default(),
        track_count: Some(playlist.items.total),
        owner: Some(user_name(&playlist.owner)),
        collaborative: playlist.collaborative,
        is_public: playlist.public,
        contributors,
        snapshot_id: playlist.snapshot_id.clone(),
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
        .iter()
        .map(playlist_from_simplified)
        .collect())
}

/// Fetch full details and the tracks of a playlist by its raw ID or URI.
/// Contributors are derived from the `added_by` field of the playlist items:
/// Spotify's Web API no longer exposes a separate collaborator list.
pub async fn playlist_details(
    client: &AuthCodePkceSpotify,
    playlist_id: &str,
) -> Result<(Playlist, Vec<Track>)> {
    let id = PlaylistId::from_id_or_uri(playlist_id)
        .with_context(|| format!("invalid playlist ID/URI: {playlist_id}"))?;
    let full = client
        .playlist(id, None, None)
        .await
        .map_err(map_spotify_error)
        .context("fetching playlist failed")?;

    let mut contributors: Vec<String> = Vec::new();
    let mut tracks = Vec::new();
    for item in &full.items.items {
        if let Some(user) = &item.added_by {
            let name = user_name(user);
            if !contributors.contains(&name) {
                contributors.push(name);
            }
        }
        if let Some(track) = track_from_playlist_item(item) {
            tracks.push(track);
        }
    }
    Ok((playlist_from_full(&full, contributors), tracks))
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

/// Create a private (non-collaborative) playlist and return its details.
pub async fn create_playlist(client: &AuthCodePkceSpotify, name: &str) -> Result<Playlist> {
    let user = client.me().await.map_err(map_spotify_error)?;
    let full = client
        .user_playlist_create(user.id, name, Some(false), Some(false), None)
        .await
        .map_err(map_spotify_error)
        .context("creating playlist failed")?;
    Ok(playlist_from_full(&full, Vec::new()))
}

/// Rename a playlist. Only the fields we pass are changed by Spotify.
pub async fn rename_playlist(
    client: &AuthCodePkceSpotify,
    playlist_id: &str,
    name: &str,
) -> Result<()> {
    let id = PlaylistId::from_id_or_uri(playlist_id)
        .with_context(|| format!("invalid playlist ID/URI: {playlist_id}"))?;
    client
        .playlist_change_detail(id, Some(name), None, None, None)
        .await
        .map_err(map_spotify_error)
        .context("renaming playlist failed")?;
    Ok(())
}

fn playable_track_id(uri: &str) -> Result<PlayableId<'_>> {
    let id = TrackId::from_uri(uri)
        .with_context(|| format!("invalid track URI: {uri}"))?;
    Ok(PlayableId::Track(id))
}

/// Append a track to a playlist.
pub async fn add_playlist_track(
    client: &AuthCodePkceSpotify,
    playlist_id: &str,
    track_uri: &str,
) -> Result<()> {
    let playlist = PlaylistId::from_id_or_uri(playlist_id)
        .with_context(|| format!("invalid playlist ID/URI: {playlist_id}"))?;
    let track = playable_track_id(track_uri)?;
    client
        .playlist_add_items(playlist, [track], None)
        .await
        .map_err(map_spotify_error)
        .context("adding track to playlist failed")?;
    Ok(())
}

/// Remove every occurrence of a track from a playlist.
pub async fn remove_playlist_track(
    client: &AuthCodePkceSpotify,
    playlist_id: &str,
    track_uri: &str,
) -> Result<()> {
    let playlist = PlaylistId::from_id_or_uri(playlist_id)
        .with_context(|| format!("invalid playlist ID/URI: {playlist_id}"))?;
    let track = playable_track_id(track_uri)?;
    client
        .playlist_remove_all_occurrences_of_items(playlist, [track], None)
        .await
        .map_err(map_spotify_error)
        .context("removing track from playlist failed")?;
    Ok(())
}

/// Compute Spotify's `range_start`/`insert_before` pair for swapping a track
/// with its neighbour. `insert_before` is an index in the *original* list, so
/// moving down needs one extra position after the removal.
pub fn reorder_positions(from: usize, to: usize) -> (i32, i32) {
    let insert_before = if to > from { to + 1 } else { to };
    (from as i32, insert_before as i32)
}

/// Move one playlist track from `from` to `to` (both zero-based final indices).
pub async fn move_playlist_track(
    client: &AuthCodePkceSpotify,
    playlist_id: &str,
    snapshot_id: &str,
    from: usize,
    to: usize,
) -> Result<()> {
    let playlist = PlaylistId::from_id_or_uri(playlist_id)
        .with_context(|| format!("invalid playlist ID/URI: {playlist_id}"))?;
    let (range_start, insert_before) = reorder_positions(from, to);
    client
        .playlist_reorder_items(
            playlist,
            Some(range_start),
            Some(insert_before),
            Some(1),
            Some(snapshot_id),
        )
        .await
        .map_err(map_spotify_error)
        .context("reordering playlist track failed")?;
    Ok(())
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

    #[test]
    fn playlist_reorder_translates_to_spotify_positions() {
        // Original [A, B, C, D]. B moves down: range 1, insert before D (3).
        assert_eq!(reorder_positions(1, 2), (1, 3));
        // C moves up: range 2, insert before B (1).
        assert_eq!(reorder_positions(2, 1), (2, 1));
        assert_eq!(reorder_positions(0, 0), (0, 0));
    }
}
