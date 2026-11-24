//! Thin async wrapper around `rspotify` for the browse/search and playlist
//! editing operations the TUI needs. Converts Spotify model types into the
//! UI's simple `Track` and `Playlist` types.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rspotify::clients::{BaseClient, OAuthClient};
use rspotify::model::{
    AlbumId, AlbumType, ArtistId, FullAlbum, FullArtist, FullPlaylist, FullTrack,
    Image as SpotifyImage, PlayableId, PlayableItem, PlaylistId, PlaylistItem, PublicUser,
    SearchResult, SearchType, SimplifiedAlbum, SimplifiedArtist, SimplifiedPlaylist,
    SimplifiedTrack, TrackId,
};
use rspotify::prelude::*;
use rspotify::{AuthCodePkceSpotify, ClientError};

use crate::ui::{Album, Artist, Playlist, Track};

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

/// Spotify frequently returns `added_by` objects with no `display_name`.
fn display_name(user: &PublicUser) -> Option<String> {
    user.display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
}

fn label_for_user(user: &PublicUser, me_id: Option<&str>, fallback: &str) -> String {
    display_name(user).unwrap_or_else(|| {
        if Some(user.id.id()) == me_id {
            "You".to_string()
        } else {
            fallback.to_string()
        }
    })
}

/// Build readable labels for the user IDs found in playlist items. Anonymous
/// IDs become "You" (for the signed-in user) or "Collaborator N" instead of
/// being shown as Spotify's opaque user IDs.
fn build_user_labels(full: &FullPlaylist, me_id: Option<&str>) -> HashMap<String, String> {
    let mut known: Vec<(String, Option<String>)> = Vec::new();
    for item in &full.items.items {
        let Some(user) = &item.added_by else { continue };
        let id = user.id.id().to_string();
        if let Some(entry) = known.iter_mut().find(|(known_id, _)| *known_id == id) {
            if entry.1.is_none() {
                entry.1 = display_name(user);
            }
        } else {
            known.push((id, display_name(user)));
        }
    }

    let mut labels = HashMap::new();
    let mut anonymous = 0usize;
    for (id, name) in known {
        let label = match name {
            Some(name) => name,
            None if Some(id.as_str()) == me_id => "You".to_string(),
            None => {
                anonymous += 1;
                format!("Collaborator {anonymous}")
            }
        };
        labels.insert(id, label);
    }
    labels
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

/// Playlist items carry the user who added them; attach a readable label to
/// the track so the UI can show per-song attribution.
fn track_from_playlist_item(
    item: &PlaylistItem,
    labels: &HashMap<String, String>,
) -> Option<Track> {
    let mut track = track_from_playable(item.item.as_ref()?)?;
    track.added_by = item
        .added_by
        .as_ref()
        .and_then(|user| labels.get(user.id.id()).cloned());
    Some(track)
}

fn playlist_from_simplified(playlist: &SimplifiedPlaylist, me_id: Option<&str>) -> Playlist {
    Playlist {
        id: playlist.id.id().to_string(),
        name: playlist.name.clone(),
        description: String::new(), // not present on simplified playlists
        track_count: Some(playlist.items.total),
        owner: Some(label_for_user(&playlist.owner, me_id, "Unknown user")),
        collaborative: playlist.collaborative,
        is_public: playlist.public,
        contributors: Vec::new(), // filled in when the playlist is opened
        art_url: pick_art_url(&playlist.images),
        snapshot_id: playlist.snapshot_id.clone(),
    }
}

fn playlist_from_full(
    playlist: &FullPlaylist,
    contributors: Vec<String>,
    me_id: Option<&str>,
) -> Playlist {
    Playlist {
        id: playlist.id.id().to_string(),
        name: playlist.name.clone(),
        description: playlist.description.clone().unwrap_or_default(),
        track_count: Some(playlist.items.total),
        owner: Some(label_for_user(&playlist.owner, me_id, "Unknown user")),
        collaborative: playlist.collaborative,
        is_public: playlist.public,
        contributors,
        art_url: pick_art_url(&playlist.images),
        snapshot_id: playlist.snapshot_id.clone(),
    }
}

/// Shared album conversion. Only full albums know the track count; search
/// results leave it as `None`.
fn album_from_parts(
    id: &str,
    name: &str,
    artists: &[SimplifiedArtist],
    album_type: Option<String>,
    release_date: Option<String>,
    images: &[SpotifyImage],
    track_count: Option<u32>,
) -> Album {
    let names: Vec<String> = artists.iter().map(|artist| artist.name.clone()).collect();
    let artist = names
        .first()
        .cloned()
        .unwrap_or_else(|| "Unknown artist".to_string());
    Album {
        id: id.to_string(),
        name: name.to_string(),
        artist,
        artists: names,
        album_type,
        release_date,
        track_count,
        art_url: pick_art_url(images),
    }
}

/// Convert a search-result album. Albums without an ID cannot be opened, so
/// they are dropped.
fn album_from_simplified(album: &SimplifiedAlbum) -> Option<Album> {
    let id = album.id.as_ref()?;
    Some(album_from_parts(
        id.id(),
        &album.name,
        &album.artists,
        album.album_type.clone(),
        album.release_date.clone(),
        &album.images,
        None,
    ))
}

/// `FullAlbum::album_type` is an enum, unlike the simplified model's string;
/// use the same snake-case label Spotify returns in the API.
fn album_from_full(album: &FullAlbum) -> Album {
    let album_type: &'static str = album.album_type.into();
    album_from_parts(
        album.id.id(),
        &album.name,
        &album.artists,
        Some(album_type.to_string()),
        Some(album.release_date.clone()),
        &album.images,
        Some(album.tracks.total),
    )
}

/// Genres are deprecated in rspotify 0.16.1 but still returned for many
/// artists, so keep them on a best-effort basis. Popularity/followers are
/// ignored because Spotify has removed those fields.
#[allow(deprecated)]
fn artist_from_full(artist: &FullArtist) -> Artist {
    Artist {
        id: artist.id.id().to_string(),
        name: artist.name.clone(),
        genres: artist.genres.clone(),
        art_url: pick_art_url(&artist.images),
    }
}

/// Album-track pages carry no album object, so the enclosing album's name and
/// art are passed in by the caller.
fn track_from_simplified(
    track: &SimplifiedTrack,
    album_name: &str,
    album_art_url: Option<String>,
) -> Option<Track> {
    let id = track.id.as_ref()?;
    let artist = track
        .artists
        .first()
        .map(|artist| artist.name.clone())
        .unwrap_or_else(|| "Unknown artist".to_string());
    Some(Track {
        title: track.name.clone(),
        artist,
        album: album_name.to_string(),
        duration: track.duration.to_std().unwrap_or(Duration::ZERO),
        uri: format!("spotify:track:{}", id.id()),
        art_url: album_art_url,
        added_by: None,
    })
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

/// Search Spotify albums by keyword (up to 10 results).
pub async fn search_albums(client: &AuthCodePkceSpotify, query: &str) -> Result<Vec<Album>> {
    let result = client
        .search(query, SearchType::Album, None, None, Some(10), None)
        .await
        .map_err(map_spotify_error)
        .context("album search failed")?;
    match result {
        SearchResult::Albums(page) => Ok(page
            .items
            .iter()
            .filter_map(album_from_simplified)
            .collect()),
        _ => Ok(Vec::new()),
    }
}

/// Search Spotify artists by keyword (up to 10 results).
pub async fn search_artists(client: &AuthCodePkceSpotify, query: &str) -> Result<Vec<Artist>> {
    let result = client
        .search(query, SearchType::Artist, None, None, Some(10), None)
        .await
        .map_err(map_spotify_error)
        .context("artist search failed")?;
    match result {
        SearchResult::Artists(page) => Ok(page.items.iter().map(artist_from_full).collect()),
        _ => Ok(Vec::new()),
    }
}

/// Fetch album metadata and every page of its tracks by raw ID or URI.
pub async fn album_details(
    client: &AuthCodePkceSpotify,
    album_id: &str,
) -> Result<(Album, Vec<Track>)> {
    let id = AlbumId::from_id_or_uri(album_id)
        .with_context(|| format!("invalid album ID/URI: {album_id}"))?;
    let full = client
        .album(id.as_ref(), None)
        .await
        .map_err(map_spotify_error)
        .context("fetching album failed")?;
    let album = album_from_full(&full);

    let mut tracks = Vec::new();
    let total = full.tracks.total;
    let mut offset = 0u32;
    while offset < total {
        // rspotify 0.16 order: (id, market, limit, offset).
        let page = client
            .album_track_manual(id.as_ref(), None, Some(50), Some(offset))
            .await
            .map_err(map_spotify_error)
            .with_context(|| format!("fetching album tracks at offset {offset} failed"))?;
        let count = page.items.len() as u32;
        for track in &page.items {
            if let Some(track) = track_from_simplified(track, &album.name, album.art_url.clone()) {
                tracks.push(track);
            }
        }
        if count == 0 {
            break; // don't spin forever on an empty or short page
        }
        offset += count;
    }
    Ok((album, tracks))
}

/// Fetch an artist and their albums by raw ID or URI. An empty
/// `include_groups` means all album types, which is what the browse view wants.
pub async fn artist_details(
    client: &AuthCodePkceSpotify,
    artist_id: &str,
) -> Result<(Artist, Vec<Album>)> {
    let id = ArtistId::from_id_or_uri(artist_id)
        .with_context(|| format!("invalid artist ID/URI: {artist_id}"))?;
    let full = client
        .artist(id.as_ref())
        .await
        .map_err(map_spotify_error)
        .context("fetching artist failed")?;
    let page = client
        .artist_albums_manual(id.as_ref(), Vec::<AlbumType>::new(), None, Some(50), None)
        .await
        .map_err(map_spotify_error)
        .context("fetching artist albums failed")?;

    let mut albums: Vec<Album> = page
        .items
        .iter()
        .filter_map(album_from_simplified)
        .collect();
    // Spotify returns albums newest-first already, but make it explicit.
    albums.sort_by(|a, b| b.release_date.cmp(&a.release_date));
    Ok((artist_from_full(&full), albums))
}

async fn current_user_id(client: &AuthCodePkceSpotify) -> Option<String> {
    client.me().await.ok().map(|user| user.id.id().to_string())
}

/// Fetch the current user's playlists (first page, up to 50).
pub async fn current_user_playlists(client: &AuthCodePkceSpotify) -> Result<Vec<Playlist>> {
    let me_id = current_user_id(client).await;
    let page = client
        .current_user_playlists_manual(Some(50), None)
        .await
        .map_err(map_spotify_error)
        .context("fetching playlists failed")?;
    Ok(page
        .items
        .iter()
        .map(|playlist| playlist_from_simplified(playlist, me_id.as_deref()))
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

    let me_id = current_user_id(client).await;
    let labels = build_user_labels(&full, me_id.as_deref());
    let mut contributors: Vec<String> = Vec::new();
    let mut tracks = Vec::new();
    for item in &full.items.items {
        if let Some(user) = &item.added_by {
            if let Some(label) = labels.get(user.id.id()) {
                if !contributors.contains(label) {
                    contributors.push(label.clone());
                }
            }
        }
        if let Some(track) = track_from_playlist_item(item, &labels) {
            tracks.push(track);
        }
    }
    Ok((
        playlist_from_full(&full, contributors, me_id.as_deref()),
        tracks,
    ))
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
    let me_id = current_user_id(client).await;
    let user = client.me().await.map_err(map_spotify_error)?;
    let full = client
        .user_playlist_create(user.id, name, Some(false), Some(false), None)
        .await
        .map_err(map_spotify_error)
        .context("creating playlist failed")?;
    Ok(playlist_from_full(&full, Vec::new(), me_id.as_deref()))
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
    let id = TrackId::from_uri(uri).with_context(|| format!("invalid track URI: {uri}"))?;
    Ok(PlayableId::Track(id))
}

/// Append a track to a playlist (used when the target is not currently open).
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

/// Compute Spotify's `range_start`/`insert_before` pair for moving one item.
/// `insert_before` is an index in the *original* list, so moving down needs one
/// extra position after the removal.
pub fn reorder_positions(from: usize, to: usize) -> (i32, i32) {
    let insert_before = if to > from { to + 1 } else { to };
    (from as i32, insert_before as i32)
}

/// A minimal set of Web API calls that turns `original` into `current`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PlaylistChangePlan {
    /// Unique URIs whose count changed; all occurrences are removed before re-adding.
    pub remove: Vec<String>,
    /// URIs to append after removals, in their final relative order.
    pub add: Vec<String>,
    /// (from, to) moves to apply after removals/additions.
    pub moves: Vec<(usize, usize)>,
}

/// Server indices whose values form the longest strictly increasing sequence.
/// Tracks outside this set are the smallest group that must be moved.
fn longest_increasing_subset(values: &[usize]) -> Vec<bool> {
    let mut tails: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = vec![None; values.len()];
    for (index, &value) in values.iter().enumerate() {
        match tails.binary_search_by(|&tail| values[tail].cmp(&value)) {
            Ok(_) => {} // equal values cannot extend a strictly increasing run
            Err(position) => {
                if position > 0 {
                    previous[index] = tails.get(position - 1).copied();
                }
                if position == tails.len() {
                    tails.push(index);
                } else {
                    tails[position] = index;
                }
            }
        }
    }
    let mut keep = vec![false; values.len()];
    if let Some(&last) = tails.last() {
        let mut cursor = Some(last);
        while let Some(index) = cursor {
            keep[index] = true;
            cursor = previous[index];
        }
    }
    keep
}

/// Plan a batch playlist edit. Reorders collapse into the minimal number of
/// moves: a track dragged repeatedly through the local draft still costs one
/// Spotify call at save time.
pub fn plan_playlist_changes(original: &[String], current: &[String]) -> PlaylistChangePlan {
    let mut original_counts: HashMap<&str, usize> = HashMap::new();
    for uri in original {
        *original_counts.entry(uri.as_str()).or_insert(0) += 1;
    }
    let mut current_counts: HashMap<&str, usize> = HashMap::new();
    for uri in current {
        *current_counts.entry(uri.as_str()).or_insert(0) += 1;
    }

    let changed: HashSet<&str> = original_counts
        .keys()
        .chain(current_counts.keys())
        .filter(|&&uri| {
            original_counts.get(uri).copied().unwrap_or(0)
                != current_counts.get(uri).copied().unwrap_or(0)
        })
        .copied()
        .collect();

    let mut plan = PlaylistChangePlan::default();
    let mut seen_remove = HashSet::new();
    for uri in original {
        if changed.contains(uri.as_str()) && seen_remove.insert(uri.as_str()) {
            plan.remove.push(uri.clone());
        }
    }
    let mut add_remaining: HashMap<&str, usize> = changed
        .iter()
        .map(|uri| (*uri, current_counts.get(*uri).copied().unwrap_or(0)))
        .collect();
    for uri in current {
        if let Some(remaining) = add_remaining.get_mut(uri.as_str()) {
            if *remaining > 0 {
                plan.add.push(uri.clone());
                *remaining -= 1;
            }
        }
    }

    // State of the playlist on Spotify after removals and additions.
    let mut server: Vec<String> = original
        .iter()
        .filter(|uri| !changed.contains(uri.as_str()))
        .cloned()
        .collect();
    server.extend(plan.add.iter().cloned());

    // Match each server occurrence to its position in the final order. The
    // longest increasing run can stay put; only the other tracks need moves.
    let mut current_positions: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (index, uri) in current.iter().enumerate() {
        current_positions
            .entry(uri.as_str())
            .or_default()
            .push_back(index);
    }
    let mut server_to_current = Vec::with_capacity(server.len());
    for uri in &server {
        let position = current_positions
            .get_mut(uri.as_str())
            .and_then(VecDeque::pop_front)
            .expect("playlist change plan: unmatched URI");
        server_to_current.push(position);
    }
    let mut current_to_server = vec![0usize; current.len()];
    for (server_index, &current_index) in server_to_current.iter().enumerate() {
        current_to_server[current_index] = server_index;
    }

    let keep = longest_increasing_subset(&server_to_current);
    let mut order: Vec<usize> = (0..server.len()).collect();
    for (target, &desired) in current_to_server.iter().enumerate() {
        if keep[desired] {
            continue;
        }
        let from = order
            .iter()
            .position(|&index| index == desired)
            .expect("playlist change plan: missing item");
        if from == target {
            continue;
        }
        plan.moves.push((from, target));
        let moved = order.remove(from);
        order.insert(target, moved);
    }
    debug_assert_eq!(
        order
            .iter()
            .map(|&index| server[index].as_str())
            .collect::<Vec<_>>(),
        current.iter().map(String::as_str).collect::<Vec<_>>()
    );
    plan
}

/// Save a batch of staged playlist edits: rename, add/remove tracks and apply
/// the minimal reorder sequence computed by `plan_playlist_changes`.
pub async fn save_playlist(
    client: &AuthCodePkceSpotify,
    playlist_id: &str,
    original_name: &str,
    name: &str,
    original: &[String],
    current: &[String],
) -> Result<()> {
    if name != original_name {
        let id = PlaylistId::from_id_or_uri(playlist_id)
            .with_context(|| format!("invalid playlist ID/URI: {playlist_id}"))?;
        client
            .playlist_change_detail(id, Some(name), None, None, None)
            .await
            .map_err(map_spotify_error)
            .context("renaming playlist failed")?;
    }

    let plan = plan_playlist_changes(original, current);

    if !plan.remove.is_empty() {
        let items = plan
            .remove
            .iter()
            .map(|uri| playable_track_id(uri))
            .collect::<Result<Vec<_>>>()?;
        let id = PlaylistId::from_id_or_uri(playlist_id)?;
        client
            .playlist_remove_all_occurrences_of_items(id, items, None)
            .await
            .map_err(map_spotify_error)
            .context("removing playlist tracks failed")?;
    }

    if !plan.add.is_empty() {
        let items = plan
            .add
            .iter()
            .map(|uri| playable_track_id(uri))
            .collect::<Result<Vec<_>>>()?;
        let id = PlaylistId::from_id_or_uri(playlist_id)?;
        client
            .playlist_add_items(id, items, None)
            .await
            .map_err(map_spotify_error)
            .context("adding playlist tracks failed")?;
    }

    for (from, to) in plan.moves {
        let (range_start, insert_before) = reorder_positions(from, to);
        let id = PlaylistId::from_id_or_uri(playlist_id)?;
        client
            .playlist_reorder_items(id, Some(range_start), Some(insert_before), Some(1), None)
            .await
            .map_err(map_spotify_error)
            .context("reordering playlist tracks failed")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rspotify::model::{DatePrecision, Followers, Page, UserId};

    #[allow(deprecated)]
    fn user(id: &str, display_name: Option<&str>) -> PublicUser {
        PublicUser {
            display_name: display_name.map(str::to_string),
            external_urls: HashMap::new(),
            followers: None,
            href: String::new(),
            id: UserId::from_id(id.to_string()).unwrap(),
            images: Vec::new(),
        }
    }

    fn image(width: u32, url: &str) -> SpotifyImage {
        SpotifyImage {
            height: Some(width),
            url: url.to_string(),
            width: Some(width),
        }
    }

    fn uris(list: &[&str]) -> Vec<String> {
        list.iter().map(|uri| (*uri).to_string()).collect()
    }

    fn simplified_artist(id: Option<&str>, name: &str) -> SimplifiedArtist {
        SimplifiedArtist {
            external_urls: HashMap::new(),
            href: None,
            id: id.map(|id| ArtistId::from_id(id.to_string()).unwrap()),
            name: name.to_string(),
        }
    }

    #[allow(deprecated)]
    fn simplified_album(
        id: Option<&str>,
        name: &str,
        artists: Vec<SimplifiedArtist>,
        images: Vec<SpotifyImage>,
    ) -> SimplifiedAlbum {
        SimplifiedAlbum {
            album_group: None,
            album_type: Some("album".to_string()),
            artists,
            available_markets: Vec::new(),
            external_urls: HashMap::new(),
            href: None,
            id: id.map(|id| AlbumId::from_id(id.to_string()).unwrap()),
            images,
            name: name.to_string(),
            release_date: Some("2026-01-02".to_string()),
            release_date_precision: Some("day".to_string()),
            restrictions: None,
        }
    }

    #[allow(deprecated)]
    fn simplified_track(
        id: Option<&str>,
        name: &str,
        artists: Vec<SimplifiedArtist>,
    ) -> SimplifiedTrack {
        SimplifiedTrack {
            album: None,
            artists,
            available_markets: None,
            disc_number: 1,
            duration: chrono::TimeDelta::seconds(200),
            explicit: false,
            external_urls: HashMap::new(),
            href: None,
            id: id.map(|id| TrackId::from_id(id.to_string()).unwrap()),
            is_local: false,
            is_playable: None,
            linked_from: None,
            restrictions: None,
            name: name.to_string(),
            preview_url: None,
            track_number: 1,
        }
    }

    #[allow(deprecated)]
    fn full_album(id: &str, name: &str, total: u32) -> FullAlbum {
        FullAlbum {
            artists: vec![simplified_artist(None, "Solo")],
            album_type: AlbumType::Album,
            available_markets: None,
            copyrights: Vec::new(),
            external_ids: HashMap::new(),
            external_urls: HashMap::new(),
            genres: Vec::new(),
            href: String::new(),
            id: AlbumId::from_id(id.to_string()).unwrap(),
            images: vec![image(300, "full-art")],
            name: name.to_string(),
            popularity: 0,
            release_date: "2026-01-02".to_string(),
            release_date_precision: DatePrecision::Day,
            tracks: Page {
                href: String::new(),
                items: Vec::new(),
                limit: 50,
                next: None,
                offset: 0,
                previous: None,
                total,
            },
            label: None,
        }
    }

    #[allow(deprecated)]
    fn full_artist(id: &str, name: &str, genres: Vec<String>) -> FullArtist {
        FullArtist {
            external_urls: HashMap::new(),
            followers: Followers { total: 0 },
            genres,
            href: String::new(),
            id: ArtistId::from_id(id.to_string()).unwrap(),
            images: vec![image(300, "artist-art")],
            name: name.to_string(),
            popularity: 0,
        }
    }

    #[test]
    fn simplified_album_converter_keeps_metadata_and_skips_missing_id() {
        let album = simplified_album(
            Some("albumid"),
            "Record",
            vec![
                simplified_artist(None, "First"),
                simplified_artist(None, "Second"),
            ],
            vec![image(300, "cover")],
        );
        let converted = album_from_simplified(&album).unwrap();
        assert_eq!(converted.id, "albumid");
        assert_eq!(converted.name, "Record");
        assert_eq!(converted.artist, "First");
        assert_eq!(converted.artists, ["First", "Second"]);
        assert_eq!(converted.album_type.as_deref(), Some("album"));
        assert_eq!(converted.release_date.as_deref(), Some("2026-01-02"));
        assert_eq!(converted.track_count, None);
        assert_eq!(converted.art_url.as_deref(), Some("cover"));

        let mut no_id = album;
        no_id.id = None;
        assert!(album_from_simplified(&no_id).is_none());

        let no_artists = simplified_album(Some("solo"), "Solo", Vec::new(), Vec::new());
        let converted = album_from_simplified(&no_artists).unwrap();
        assert_eq!(converted.artist, "Unknown artist");
        assert_eq!(converted.art_url, None);
    }

    #[test]
    fn simplified_track_converter_uses_album_context_and_skips_missing_id() {
        let track = simplified_track(
            Some("trackid"),
            "Song",
            vec![simplified_artist(None, "Singer")],
        );
        let converted = track_from_simplified(&track, "Record", Some("cover".to_string())).unwrap();
        assert_eq!(converted.uri, "spotify:track:trackid");
        assert_eq!(converted.album, "Record");
        assert_eq!(converted.artist, "Singer");
        assert_eq!(converted.duration, Duration::from_secs(200));
        assert_eq!(converted.art_url.as_deref(), Some("cover"));
        assert_eq!(converted.added_by, None);

        let mut no_id = track;
        no_id.id = None;
        assert!(track_from_simplified(&no_id, "Record", None).is_none());
    }

    #[test]
    fn full_album_converter_reports_track_count() {
        let converted = album_from_full(&full_album("albumid", "Record", 123));
        assert_eq!(converted.id, "albumid");
        assert_eq!(converted.name, "Record");
        assert_eq!(converted.album_type.as_deref(), Some("album"));
        assert_eq!(converted.track_count, Some(123));
        assert_eq!(converted.art_url.as_deref(), Some("full-art"));
    }

    #[test]
    fn artist_converter_handles_empty_genres_and_ignores_deprecated_fields() {
        let converted = artist_from_full(&full_artist("artistid", "Solo", Vec::new()));
        assert_eq!(converted.id, "artistid");
        assert_eq!(converted.name, "Solo");
        assert!(converted.genres.is_empty());
        assert_eq!(converted.art_url.as_deref(), Some("artist-art"));

        let converted = artist_from_full(&full_artist(
            "other",
            "Band",
            vec!["rock".to_string(), "indie".to_string()],
        ));
        assert_eq!(converted.genres, ["rock", "indie"]);
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
    fn user_labels_prefer_display_names_and_hide_opaque_ids() {
        let alice = user("aliceid", Some("Alice"));
        let me = user("meid", None);
        let stranger = user("bobid", None);
        assert_eq!(
            label_for_user(&alice, Some("meid"), "Collaborator 1"),
            "Alice"
        );
        assert_eq!(label_for_user(&me, Some("meid"), "Collaborator 1"), "You");
        assert_eq!(
            label_for_user(&stranger, Some("meid"), "Collaborator 1"),
            "Collaborator 1"
        );
    }

    #[test]
    fn playlist_reorder_translates_to_spotify_positions() {
        // Original [A, B, C, D]. B moves down: range 1, insert before D (3).
        assert_eq!(reorder_positions(1, 2), (1, 3));
        // C moves up: range 2, insert before B (1).
        assert_eq!(reorder_positions(2, 1), (2, 1));
        assert_eq!(reorder_positions(0, 0), (0, 0));
    }

    #[test]
    fn change_plan_collapses_a_single_drag_into_one_move() {
        let original = uris(&["a", "b", "c", "d"]);
        let current = uris(&["a", "c", "b", "d"]);
        let plan = plan_playlist_changes(&original, &current);
        assert!(plan.remove.is_empty());
        assert!(plan.add.is_empty());
        assert_eq!(plan.moves.len(), 1);

        // Several keyboard steps still produce one Spotify call at save time.
        let current = uris(&["d", "a", "b", "c"]);
        let plan = plan_playlist_changes(&original, &current);
        assert_eq!(plan.moves, vec![(3, 0)]);

        // Dragging the first track to the end is also a single move.
        let current = uris(&["b", "c", "d", "a"]);
        let plan = plan_playlist_changes(&original, &current);
        assert_eq!(plan.moves, vec![(0, 3)]);
    }

    #[test]
    fn change_plan_batches_additions_and_removals() {
        let original = uris(&["a", "b", "c"]);
        let current = uris(&["a", "c", "x", "y"]);
        let plan = plan_playlist_changes(&original, &current);
        assert_eq!(plan.remove, uris(&["b"]));
        assert_eq!(plan.add, uris(&["x", "y"]));
        // After removing b and appending x/y the order already matches.
        assert!(plan.moves.is_empty());

        let original = uris(&["a", "a", "b"]);
        let current = uris(&["a", "b"]);
        let plan = plan_playlist_changes(&original, &current);
        assert_eq!(plan.remove, uris(&["a"]));
        assert_eq!(plan.add, uris(&["a"]));
        // The surviving copy is appended after b, so b moves to the end.
        assert_eq!(plan.moves, vec![(0, 1)]);
    }
}
