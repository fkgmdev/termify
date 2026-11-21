use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Rect, Size};
use ratatui::widgets::ListState;
use ratatui_image::picker::Picker;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::art::{self, ArtStatus, CoverArt};
use crate::offline::{self, DownloadEntry, DownloadStore, OfflinePlayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    Playlists,
    Queue,
    Lyrics,
    Downloads,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Search,
        Tab::Playlists,
        Tab::Queue,
        Tab::Lyrics,
        Tab::Downloads,
    ];

    pub fn next(self) -> Self {
        match self {
            Tab::Search => Tab::Playlists,
            Tab::Playlists => Tab::Queue,
            Tab::Queue => Tab::Lyrics,
            Tab::Lyrics => Tab::Downloads,
            Tab::Downloads => Tab::Search,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Tab::Search => Tab::Downloads,
            Tab::Playlists => Tab::Search,
            Tab::Queue => Tab::Playlists,
            Tab::Lyrics => Tab::Queue,
            Tab::Downloads => Tab::Lyrics,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Search => "Search",
            Tab::Playlists => "Playlists",
            Tab::Queue => "Queue",
            Tab::Lyrics => "Lyrics",
            Tab::Downloads => "Offline",
        }
    }
}

/// Which backend is responsible for the current track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlaybackMode {
    #[default]
    Online,
    Offline,
}

/// Repeat behaviour at the end of a track. `r` cycles through the modes; the
/// same three states are exposed as MPRIS `Loop_Status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Duration,
    pub uri: String,
    /// Album-cover URL from the Web API, used for TUI and MPRIS artwork.
    pub art_url: Option<String>,
    /// Spotify user who added this track to the current playlist, when known.
    pub added_by: Option<String>,
}

impl Track {
    pub fn display(&self) -> String {
        let mins = self.duration.as_secs() / 60;
        let secs = self.duration.as_secs() % 60;
        format!(
            "{} — {}  [{:02}:{:02}]",
            self.title, self.artist, mins, secs
        )
    }
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub description: String,
    pub track_count: Option<u32>,
    /// Playlist owner's display name (or Spotify ID).
    pub owner: Option<String>,
    pub collaborative: bool,
    pub is_public: Option<bool>,
    /// Distinct users who added items to the playlist (from item attribution).
    pub contributors: Vec<String>,
    /// Playlist cover image from Spotify, shown on the Playlists tab.
    pub art_url: Option<String>,
    /// Kept for future optimistic-concurrency checks; batch saves use positions.
    #[allow(dead_code)]
    pub snapshot_id: String,
}

impl Playlist {
    /// Spotify's `collaborative` flag is often stale or missing even when
    /// several people have added tracks, so infer a shared playlist too.
    pub fn is_shared(&self) -> bool {
        self.collaborative || self.contributors.len() > 1
    }

    /// Label used in the Details pane. The inferred case makes it clear that
    /// Spotify did not set the flag but the item attribution disagrees.
    pub fn collaboration_label(&self) -> &'static str {
        if self.collaborative {
            "Yes"
        } else if self.is_shared() {
            "Yes (inferred)"
        } else {
            "No"
        }
    }

    pub fn display(&self) -> String {
        let mut details = Vec::new();
        if let Some(count) = self.track_count {
            details.push(format!("{count} tracks"));
        }
        if self.collaborative {
            details.push("collaborative".to_string());
        } else if self.is_shared() {
            details.push("shared".to_string());
        }
        if details.is_empty() {
            self.name.clone()
        } else {
            format!("{}  ({})", self.name, details.join(", "))
        }
    }
}

/// Text input overlay used for playlist name editing.
#[derive(Debug, Clone)]
pub enum PromptKind {
    NewPlaylist,
    RenamePlaylist { id: String },
}

impl PromptKind {
    pub fn label(&self) -> &'static str {
        match self {
            PromptKind::NewPlaylist => "New playlist name",
            PromptKind::RenamePlaylist { .. } => "Rename playlist",
        }
    }
}

/// What a decoded cover image belongs to. Stale responses are ignored so a
/// slow track fetch cannot replace a playlist cover (or vice versa).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverArtTarget {
    Track { uri: String },
    Playlist { id: String },
}

/// Locally staged edits for one playlist. Nothing is sent to Spotify until
/// `S` is pressed, so repeated reorders cost one batched save instead of one
/// request per keystroke.
#[derive(Debug, Clone)]
pub struct PlaylistDraft {
    pub original_name: String,
    pub name: String,
    pub original_uris: Vec<String>,
    pub original_tracks: Vec<Track>,
    pub tracks: Vec<Track>,
}

impl PlaylistDraft {
    fn new(playlist: &Playlist, tracks: &[Track]) -> Self {
        let original_uris = tracks.iter().map(|track| track.uri.clone()).collect();
        Self {
            original_name: playlist.name.clone(),
            name: playlist.name.clone(),
            original_uris,
            original_tracks: tracks.to_vec(),
            tracks: tracks.to_vec(),
        }
    }

    pub fn uris(&self) -> Vec<String> {
        self.tracks.iter().map(|track| track.uri.clone()).collect()
    }

    pub fn is_dirty(&self) -> bool {
        self.name != self.original_name || self.uris() != self.original_uris
    }
}

/// Progress of a whole-playlist download. The URI set lets ordinary
/// `DownloadFinished` actions update the playlist progress without adding a
/// second download path.
#[derive(Debug, Clone, Default)]
pub struct PlaylistDownload {
    pub name: String,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    /// URIs still being downloaded, used for progress and duplicate avoidance.
    pub uris: HashSet<String>,
    /// Every unique URI in the playlist, including tracks already on disk.
    pub members: Vec<String>,
}

/// A row in the Offline tab. Playlists and the "all tracks" row are queues
/// that can be played without a network connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineItem {
    Playlist {
        id: String,
        name: String,
        count: usize,
    },
    AllTracks,
    Track {
        uri: String,
    },
}

/// Which offline queue was last loaded for playback. This is deliberately
/// separate from `current_playlist_id`: offline playlists are local groupings
/// and must never be edited or renamed through the Web API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineQueue {
    All,
    Playlist { id: String, name: String },
}

impl OfflineQueue {
    pub fn name(&self) -> String {
        match self {
            Self::All => "All downloaded tracks".to_string(),
            Self::Playlist { name, .. } => name.clone(),
        }
    }
}

/// Playlist selection overlay shown when adding a track.
#[derive(Debug, Clone)]
pub struct PlaylistPicker {
    pub cursor: usize,
    /// Full selected track so staging an add keeps all metadata (art, length).
    pub track: Track,
}

#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub track: Track,
    pub position: Duration,
    pub playing: bool,
    pub volume: u8,
}

/// Optional paths used when constructing `App`; tests can omit them.
#[derive(Default)]
pub struct AppOptions {
    pub lyrics_cache_file: Option<PathBuf>,
    pub downloads: Option<DownloadStore>,
}

#[derive(Debug)]
pub enum Action {
    Tick,
    /// Terminal resize from crossterm; used to re-encode cover art.
    Resize(u16, u16),
    Key(KeyEvent),
    Loading(String),
    SearchResults(Result<Vec<Track>, String>),
    PlaylistsLoaded(Result<Vec<Playlist>, String>),
    /// Full playlist metadata plus its tracks and per-track attribution.
    PlaylistDetailsLoaded(Result<(Playlist, Vec<Track>), String>),
    PlaylistCreated(Result<Playlist, String>),
    PlaylistRenamed {
        id: String,
        result: Result<String, String>,
    },
    /// Result of an add/remove/reorder command. `reload_playlists` requests a
    /// fresh list; otherwise the currently open playlist is reloaded.
    PlaylistEdited {
        message: String,
        result: Result<(), String>,
        reload_playlists: bool,
        /// Reload the currently viewed playlist when it matches this ID.
        reload_playlist: Option<String>,
    },
    /// Result of saving a locally staged playlist draft.
    PlaylistSaved {
        id: String,
        name: String,
        result: Result<(), String>,
    },
    LikedSongsLoaded(Result<Vec<Track>, String>),
    /// A track finished downloading and was written to the offline store.
    DownloadFinished {
        uri: String,
        result: Result<DownloadEntry, String>,
    },
    /// Playlist tracks were fetched and queued for offline download. `members`
    /// is the full playlist so already-saved tracks can join the offline queue.
    PlaylistDownloadStarted {
        id: String,
        name: String,
        total: usize,
        uris: Vec<String>,
        members: Vec<String>,
    },
    /// All tracks for a playlist download finished, or fetching it failed.
    PlaylistDownloadFinished {
        id: String,
        name: String,
        total: usize,
        failed: usize,
        members: Vec<String>,
        error: Option<String>,
    },
    PlayerEndOfTrack,
    PlayerPlaying(u32),
    PlayerPaused(u32),
    LyricsLoaded {
        uri: String,
        result: Result<Option<crate::lyrics::Lyrics>, String>,
    },
    TranslationsLoaded {
        uri: String,
        result: Result<Vec<String>, String>,
    },
    /// Cover art downloaded and decoded off the UI thread. `target` and `url`
    /// keep stale responses from replacing the image for a different item.
    CoverArtLoaded {
        target: CoverArtTarget,
        url: String,
        result: Result<image::DynamicImage, String>,
    },
    // Controls coming from MPRIS (desktop media player / global media keys).
    MprisPlayPause,
    MprisPlay,
    MprisPause,
    MprisNext,
    MprisPrev,
    /// Relative seek, in milliseconds (negative seeks backwards).
    MprisSeek(i64),
    /// Absolute seek; ignored unless `track` matches the current track id.
    MprisSetPosition {
        track: String,
        position_ms: u64,
    },
    /// Volume in the MPRIS range 0.0..=1.0.
    MprisSetVolume(f64),
    MprisSetShuffle(bool),
    MprisSetRepeat(RepeatMode),
    MprisOpenUri(String),
    MprisQuit,
    Error(String),
}

pub struct App {
    pub running: bool,
    pub tab: Tab,
    pub show_help: bool,
    pub search_query: String,
    pub search_active: bool,
    pub list_state: ListState,
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
    /// Playlist whose tracks are currently loaded (for track edits).
    pub current_playlist_id: Option<String>,
    /// Active playlist name input overlay.
    pub prompt: Option<PromptKind>,
    pub prompt_input: String,
    /// Active "add track to playlist" overlay.
    pub playlist_picker: Option<PlaylistPicker>,
    /// Unsaved edits keyed by playlist ID; applied locally until `S`/`D`.
    pub playlist_drafts: HashMap<String, PlaylistDraft>,
    pub now_playing: Option<NowPlaying>,
    pub status: String,
    pub loading: bool,
    pub error: Option<String>,
    pub api_client: Option<std::sync::Arc<rspotify::AuthCodePkceSpotify>>,
    pub player: Option<std::sync::Arc<crate::player::LocalPlayer>>,
    /// Downloaded tracks and their location on disk.
    pub downloads: DownloadStore,
    /// URIs with a download in progress, to avoid duplicate background work.
    pub downloading: HashSet<String>,
    /// Whole-playlist downloads currently in progress, keyed by playlist ID.
    pub playlist_downloads: HashMap<String, PlaylistDownload>,
    /// Rows rendered on the Offline tab (playlists, all tracks, then tracks).
    pub offline_items: Vec<OfflineItem>,
    /// Queue selected on the Offline tab, used by next/prev and repeat-all.
    pub offline_queue: Option<OfflineQueue>,
    /// Selected track inside `tracks` while viewing the Offline tab.
    pub offline_track_index: usize,
    /// Lazily created when an offline track is first played.
    offline_player: Option<OfflinePlayer>,
    pub playback_mode: PlaybackMode,
    offline_end_handled: bool,
    pub lyrics: std::collections::HashMap<String, crate::lyrics::Lyrics>,
    pub lyrics_loading: bool,
    /// Selected lyric line on the Lyrics tab; Enter seeks to its timestamp.
    pub lyric_cursor: usize,
    pub lyrics_auto_scroll: bool,
    pub translations: std::collections::HashMap<String, Vec<String>>,
    pub translate_lyrics: bool,
    pub translation_loading: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    /// Playback order as indices into `tracks`; identity when shuffle is off.
    pub play_order: Vec<usize>,
    /// Selected row on the Queue tab, as a position in `play_order`.
    pub queue_cursor: usize,
    /// Tracks known to have no lyrics, persisted with the cache to avoid
    /// repeating failed LRCLIB lookups after a restart.
    pub no_lyrics: HashSet<String>,
    lyrics_cache_file: Option<PathBuf>,
    /// Current cover-art state (image protocol or text placeholder).
    pub art: CoverArt,
    /// User-facing art toggle. Forced off when the terminal cannot show images.
    pub art_enabled: bool,
    /// Terminal size used to choose the cover dimensions and rebuild its protocol.
    terminal_area: Rect,
    art_blocked_reason: Option<String>,
    picker: Picker,
    http_client: reqwest::Client,
    tx: Sender<Action>,
}

impl App {
    #[cfg(test)]
    pub fn new(tx: Sender<Action>, picker: Picker, art_blocked_reason: Option<String>) -> Self {
        Self::new_with_paths(tx, picker, art_blocked_reason, None)
    }

    /// Build the app without offline storage (used by tests).
    #[cfg(test)]
    pub fn new_with_paths(
        tx: Sender<Action>,
        picker: Picker,
        art_blocked_reason: Option<String>,
        lyrics_cache_file: Option<PathBuf>,
    ) -> Self {
        Self::new_with_options(
            tx,
            picker,
            art_blocked_reason,
            AppOptions {
                lyrics_cache_file,
                downloads: None,
            },
        )
    }

    /// Build the app, restoring lyrics and the download index from disk.
    pub fn new_with_options(
        tx: Sender<Action>,
        picker: Picker,
        art_blocked_reason: Option<String>,
        options: AppOptions,
    ) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let art = match &art_blocked_reason {
            Some(reason) => CoverArt::disabled(reason.clone()),
            None => CoverArt::no_art(),
        };
        let art_enabled = art_blocked_reason.is_none();
        let mut app = Self {
            running: true,
            tab: Tab::Search,
            show_help: false,
            search_query: String::new(),
            search_active: false,
            list_state,
            tracks: Vec::new(),
            playlists: Vec::new(),
            current_playlist_id: None,
            prompt: None,
            prompt_input: String::new(),
            playlist_picker: None,
            playlist_drafts: HashMap::new(),
            now_playing: None,
            status: "Ready — press / to search, ? for help".to_string(),
            loading: false,
            error: None,
            api_client: None,
            player: None,
            downloads: options.downloads.unwrap_or_else(DownloadStore::disabled),
            downloading: HashSet::new(),
            playlist_downloads: HashMap::new(),
            offline_items: Vec::new(),
            offline_queue: None,
            offline_track_index: 0,
            offline_player: None,
            playback_mode: PlaybackMode::Online,
            offline_end_handled: false,
            lyrics: std::collections::HashMap::new(),
            no_lyrics: HashSet::new(),
            lyrics_cache_file: options.lyrics_cache_file,
            lyrics_loading: false,
            lyric_cursor: 0,
            lyrics_auto_scroll: true,
            translations: std::collections::HashMap::new(),
            translate_lyrics: false,
            translation_loading: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            play_order: Vec::new(),
            queue_cursor: 0,
            art,
            art_enabled,
            terminal_area: Rect::new(0, 0, 100, 30),
            art_blocked_reason,
            picker,
            http_client: reqwest::Client::new(),
            tx,
        };
        app.load_lyrics_cache();
        app
    }

    /// Terminal size affects the cover protocol; update it and re-encode any
    /// ready image without refetching it.
    pub fn set_terminal_size(&mut self, width: u16, height: u16) {
        self.terminal_area = Rect::new(0, 0, width.max(1), height.max(1));
        self.refresh_cover_protocol();
    }

    fn art_size(&self) -> Size {
        art::cover_size(self.terminal_area)
    }

    fn refresh_cover_protocol(&mut self) {
        let size = self.art_size();
        let previous = std::mem::replace(&mut self.art, CoverArt::NoArt);
        if size.width == 0 || size.height == 0 {
            // Keep the decoded image so it can be encoded once the terminal is
            // large enough again, without refetching from Spotify.
            self.art = match previous {
                CoverArt::Ready { url, image, .. } | CoverArt::Deferred { url, image } => {
                    CoverArt::deferred(url, image)
                }
                CoverArt::Failed { url, message } => CoverArt::failed(url, message),
                CoverArt::Disabled { reason } => CoverArt::disabled(reason),
                CoverArt::Loading { url } => CoverArt::loading(url),
                CoverArt::NoArt => CoverArt::NoArt,
            };
            return;
        }
        self.art = match previous {
            CoverArt::Ready {
                url,
                image,
                protocol,
            } if protocol.size() == size => CoverArt::ready(url, image, protocol),
            CoverArt::Ready { url, image, .. } | CoverArt::Deferred { url, image } => {
                match art::build_protocol(&self.picker, image.clone(), size) {
                    Ok(protocol) => CoverArt::ready(url, image, protocol),
                    Err(err) => CoverArt::failed(url, format!("{err:#}")),
                }
            }
            CoverArt::Loading { url } => CoverArt::loading(url),
            CoverArt::Failed { url, message } => CoverArt::failed(url, message),
            CoverArt::Disabled { reason } => CoverArt::disabled(reason),
            CoverArt::NoArt => CoverArt::NoArt,
        };
    }

    fn load_lyrics_cache(&mut self) {
        let Some(path) = self.lyrics_cache_file.clone() else {
            return;
        };
        match crate::lyrics::load_cache(&path) {
            Ok(entries) => {
                for (uri, cached) in entries {
                    match cached {
                        Some(lyrics) => {
                            self.lyrics.insert(uri, lyrics);
                        }
                        None => {
                            self.no_lyrics.insert(uri);
                        }
                    }
                }
            }
            Err(err) => tracing::warn!("Ignoring lyrics cache: {err:#}"),
        }
    }

    fn persist_lyrics_cache(&self, uri: &str, result: &Option<crate::lyrics::Lyrics>) {
        let Some(path) = &self.lyrics_cache_file else {
            return;
        };
        let mut entries: HashMap<String, Option<crate::lyrics::Lyrics>> = HashMap::new();
        for (cached_uri, lyrics) in &self.lyrics {
            entries.insert(cached_uri.clone(), Some(lyrics.clone()));
        }
        for cached_uri in &self.no_lyrics {
            entries.insert(cached_uri.clone(), None);
        }
        entries.insert(uri.to_string(), result.clone());
        if let Err(err) = crate::lyrics::save_cache(path, &entries) {
            tracing::warn!("Could not save lyrics cache: {err:#}");
        }
    }

    /// Show the offline library. This also becomes the current track list, so
    /// Enter/j/k/n/p work exactly as they do for search results or playlists.
    pub fn load_downloaded_tracks(&mut self) {
        let tracks = self.downloads.tracks();
        let mut items: Vec<OfflineItem> = self
            .downloads
            .playlists()
            .into_iter()
            .filter_map(|playlist| {
                let count = self.downloads.playlist_tracks(&playlist.id).len();
                (count > 0).then_some(OfflineItem::Playlist {
                    id: playlist.id,
                    name: playlist.name,
                    count,
                })
            })
            .collect();
        if !tracks.is_empty() {
            items.push(OfflineItem::AllTracks);
            items.extend(tracks.iter().map(|track| OfflineItem::Track {
                uri: track.uri.clone(),
            }));
        }
        self.offline_items = items;
        let playlists = self
            .offline_items
            .iter()
            .filter(|item| matches!(item, OfflineItem::Playlist { .. }))
            .count();
        self.tab = Tab::Downloads;
        self.current_playlist_id = None;
        self.offline_track_index = 0;
        if self
            .offline_queue
            .as_ref()
            .is_some_and(|queue| !matches!(queue, OfflineQueue::All) && tracks.is_empty())
        {
            self.offline_queue = None;
        }
        self.set_tracks(tracks.clone());
        self.status = if tracks.is_empty() {
            "No downloads yet — select a track and press d".to_string()
        } else {
            format!(
                "Offline library — {} tracks, {} playlists",
                tracks.len(),
                playlists
            )
        };
    }

    fn show_downloads(&mut self) {
        self.load_downloaded_tracks();
    }

    /// Launch in offline mode: only downloaded tracks are available.
    pub fn set_offline_mode(&mut self) {
        self.playback_mode = PlaybackMode::Offline;
        self.offline_queue = None;
        self.load_downloaded_tracks();
        self.status = if self.downloads.is_empty() {
            "Offline mode — no downloads found".to_string()
        } else {
            format!("Offline mode — {} tracks available", self.downloads.len())
        };
    }

    fn play_downloaded_track(&mut self) {
        let Some(index) = self.list_state.selected() else {
            self.status = "Select an offline playlist, all tracks, or a track".to_string();
            return;
        };
        let Some(item) = self.offline_items.get(index).cloned() else {
            self.status = "Select an offline playlist, all tracks, or a track".to_string();
            return;
        };
        match item {
            OfflineItem::Playlist { id, name, .. } => {
                let tracks = self.downloads.playlist_tracks(&id);
                if tracks.is_empty() {
                    self.status = format!("Offline playlist '{name}' has no saved tracks");
                    return;
                }
                let count = tracks.len();
                self.set_tracks(tracks);
                self.offline_queue = Some(OfflineQueue::Playlist { id, name: name.clone() });
                self.offline_track_index = 0;
                self.list_state.select(Some(index));
                self.status = format!("Playing offline playlist '{name}' — {count} tracks");
                self.select_current();
                self.status = format!("Playing offline playlist '{name}' — {count} tracks");
            }
            OfflineItem::AllTracks => {
                let tracks = self.downloads.tracks();
                if tracks.is_empty() {
                    self.status = "No downloaded tracks to play".to_string();
                    return;
                }
                let count = tracks.len();
                self.set_tracks(tracks);
                self.offline_queue = Some(OfflineQueue::All);
                self.offline_track_index = 0;
                self.list_state.select(Some(index));
                self.status = format!("Playing all {count} downloaded tracks");
                self.select_current();
                self.status = format!("Playing all {count} downloaded tracks");
            }
            OfflineItem::Track { uri } => {
                if self.offline_queue.is_none() {
                    self.offline_queue = Some(OfflineQueue::All);
                }
                let Some(track_index) = self.tracks.iter().position(|track| track.uri == uri)
                else {
                    self.status = "That track is no longer in the offline library".to_string();
                    return;
                };
                self.offline_track_index = track_index;
                self.select_current();
            }
        }
    }

    /// Keep the Downloads list selection on the row representing `uri` while
    /// still tracking the track inside the playback queue separately.
    fn select_downloads_row_for_uri(&mut self, uri: &str) {
        if let Some(row) = self
            .offline_items
            .iter()
            .position(|item| matches!(item, OfflineItem::Track { uri: item_uri } if item_uri == uri))
        {
            self.list_state.select(Some(row));
        }
    }

    fn download_selected(&mut self) {
        // On the Playlists tab the selected row is a playlist, so `d` saves all
        // of its tracks instead of the last track list shown in Search.
        if self.tab == Tab::Playlists {
            self.download_selected_playlist();
            return;
        }
        let Some(track) = self
            .list_state
            .selected()
            .and_then(|index| self.tracks.get(index))
            .cloned()
        else {
            self.status = "Select a track to download".to_string();
            return;
        };
        if self.downloads.contains(&track.uri) {
            self.status = format!("'{}' is already downloaded", track.title);
            return;
        }
        if !self.downloading.insert(track.uri.clone()) {
            self.status = format!("Already downloading '{}'", track.title);
            return;
        }
        let Some(player) = self.player.clone() else {
            self.downloading.remove(&track.uri);
            self.status = "Sign in with local playback enabled to download tracks".to_string();
            return;
        };
        let tx = self.tx.clone();
        let dir = self.downloads.dir().to_path_buf();
        let download_uri = track.uri.clone();
        let title = track.title.clone();
        self.status = format!("Downloading '{title}'...");
        tokio::spawn(async move {
            let result = offline::download_track(&player.session, &track, &dir)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx
                .send(Action::DownloadFinished {
                    uri: download_uri,
                    result,
                })
                .await;
        });
    }

    /// Download every track of the selected playlist. Tracks that are already
    /// stored or in flight are skipped; results reuse `DownloadFinished` so the
    /// normal index and error handling still applies.
    fn download_selected_playlist(&mut self) {
        let Some(playlist) = self
            .list_state
            .selected()
            .and_then(|index| self.playlists.get(index))
            .cloned()
        else {
            self.status = "Select a playlist to download".to_string();
            return;
        };
        if self.playlist_downloads.contains_key(&playlist.id) {
            self.status = format!("Already downloading '{}'", playlist.name);
            return;
        }
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to download playlists".to_string();
            return;
        };
        let Some(player) = self.player.clone() else {
            self.status = "Sign in with local playback enabled to download tracks".to_string();
            return;
        };

        let id = playlist.id.clone();
        let name = playlist.name.clone();
        let mut skip: HashSet<String> = self.downloading.clone();
        skip.extend(self.downloads.tracks().into_iter().map(|track| track.uri));
        self.playlist_downloads.insert(
            id.clone(),
            PlaylistDownload {
                name: name.clone(),
                ..PlaylistDownload::default()
            },
        );
        self.status = format!("Fetching '{}' for offline download...", name);

        let tx = self.tx.clone();
        let dir = self.downloads.dir().to_path_buf();
        tokio::spawn(async move {
            let tracks = match crate::api::playlist_details(&client, &id).await {
                Ok((_, tracks)) => tracks,
                Err(err) => {
                    let message = format!("Could not download playlist '{name}': {err:#}");
                    let _ = tx
                        .send(Action::PlaylistDownloadFinished {
                            id,
                            name,
                            total: 0,
                            failed: 0,
                            members: Vec::new(),
                            error: Some(message),
                        })
                        .await;
                    return;
                }
            };

            let mut seen = HashSet::new();
            let mut members = Vec::new();
            let mut pending = Vec::new();
            for track in tracks {
                if !seen.insert(track.uri.clone()) {
                    continue;
                }
                members.push(track.uri.clone());
                if !skip.contains(&track.uri) {
                    pending.push(track);
                }
            }
            let total = pending.len();
            let uris: Vec<String> = pending.iter().map(|track| track.uri.clone()).collect();
            let _ = tx
                .send(Action::PlaylistDownloadStarted {
                    id: id.clone(),
                    name: name.clone(),
                    total,
                    uris,
                    members: members.clone(),
                })
                .await;

            let mut failed = 0;
            for (index, track) in pending.into_iter().enumerate() {
                // Give Spotify's audio-key service room between tracks. The
                // download itself also retries key requests with backoff.
                if index > 0 {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                let result = offline::download_track(&player.session, &track, &dir)
                    .await
                    .map_err(|err| format!("{err:#}"));
                if result.is_err() {
                    failed += 1;
                }
                let _ = tx
                    .send(Action::DownloadFinished {
                        uri: track.uri.clone(),
                        result,
                    })
                    .await;
            }
            let _ = tx
                .send(Action::PlaylistDownloadFinished {
                    id,
                    name,
                    total,
                    failed,
                    members,
                    error: None,
                })
                .await;
        });
    }

    fn delete_selected_download(&mut self) {
        let selected = self
            .list_state
            .selected()
            .and_then(|index| self.offline_items.get(index))
            .cloned();
        match selected {
            Some(OfflineItem::Playlist { id, name, .. }) => {
                if let Err(err) = self.downloads.remove_playlist(&id) {
                    self.set_error(format!("Could not remove offline playlist: {err:#}"));
                    return;
                }
                if self.offline_queue.as_ref().is_some_and(|queue| {
                    matches!(queue, OfflineQueue::Playlist { id: queue_id, .. } if queue_id == &id)
                }) {
                    self.offline_queue = None;
                }
                self.load_downloaded_tracks();
                self.status = format!("Forgot offline playlist '{name}' — audio files kept");
                return;
            }
            Some(OfflineItem::AllTracks) => {
                self.status = "Select a track row to delete its offline copy".to_string();
                return;
            }
            Some(OfflineItem::Track { uri }) => {
                let Some(track) = self
                    .downloads
                    .get(&uri)
                    .map(|entry| entry.track.clone())
                    .or_else(|| self.tracks.iter().find(|track| track.uri == uri).cloned())
                else {
                    return;
                };
                self.delete_downloaded_track(track);
            }
            None => {
                self.status = "Select a track row to delete its offline copy".to_string();
            }
        }
    }

    fn delete_downloaded_track(&mut self, track: crate::ui::Track) {
        let current_is_offline = self.playback_mode == PlaybackMode::Offline
            && self
                .now_playing
                .as_ref()
                .is_some_and(|np| np.track.uri == track.uri);
        if let Err(err) = self.downloads.remove(&track.uri) {
            self.set_error(format!("Could not delete download: {err:#}"));
            return;
        }

        let mut status = format!("Removed offline copy of '{}'", track.title);
        // Deleting the file behind a playing rodio source is safe on Unix but
        // repeat/seek would then fail. Stop it and resume the stream when the
        // user is still signed in; otherwise leave the track paused.
        if current_is_offline {
            if let Some(player) = &self.offline_player {
                player.stop();
            }
            if self.player.is_some() {
                if let Err(err) = self.play_track(&track, Duration::ZERO) {
                    status.push_str(" (could not resume streaming)");
                    tracing::warn!("Could not resume streaming after delete: {err:#}");
                    if let Some(np) = &mut self.now_playing {
                        np.playing = false;
                        np.position = Duration::ZERO;
                    }
                }
            } else {
                self.offline_player = None;
                if let Some(np) = &mut self.now_playing {
                    np.playing = false;
                    np.position = Duration::ZERO;
                }
            }
        }
        self.load_downloaded_tracks();
        self.status = status;
    }

    /// Populate demo data so the UI shell can be developed/tested without
    /// Spotify credentials. Real data replaces this in M2.
    pub fn load_demo(&mut self) {
        self.tracks = vec![
            Track {
                title: "Bohemian Rhapsody".into(),
                artist: "Queen".into(),
                album: "A Night at the Opera".into(),
                duration: Duration::from_secs(354),
                uri: "spotify:track:demo1".into(),
                art_url: None,
                added_by: Some("Demo User".into()),
            },
            Track {
                title: "Stairway to Heaven".into(),
                artist: "Led Zeppelin".into(),
                album: "Led Zeppelin IV".into(),
                duration: Duration::from_secs(482),
                uri: "spotify:track:demo2".into(),
                art_url: None,
                added_by: Some("Demo User".into()),
            },
            Track {
                title: "Imagine".into(),
                artist: "John Lennon".into(),
                album: "Imagine".into(),
                duration: Duration::from_secs(183),
                uri: "spotify:track:demo3".into(),
                art_url: None,
                added_by: Some("Alice".into()),
            },
            Track {
                title: "Hotel California".into(),
                artist: "Eagles".into(),
                album: "Hotel California".into(),
                duration: Duration::from_secs(391),
                uri: "spotify:track:demo4".into(),
                art_url: None,
                added_by: Some("Bob".into()),
            },
            Track {
                title: "Sweet Child O' Mine".into(),
                artist: "Guns N' Roses".into(),
                album: "Appetite for Destruction".into(),
                duration: Duration::from_secs(356),
                uri: "spotify:track:demo5".into(),
                art_url: None,
                added_by: None,
            },
        ];
        self.build_play_order();
        self.now_playing = Some(NowPlaying {
            track: self.tracks[0].clone(),
            position: Duration::from_secs(42),
            playing: false,
            volume: 60,
        });
        self.status = "Demo mode — press ? for help".to_string();
    }

    /// Snapshot of everything the desktop needs to show in its media player.
    pub fn mpris_state(&self) -> crate::mpris::MprisState {
        let now_playing = self.now_playing.as_ref();
        crate::mpris::MprisState {
            track: now_playing.map(|np| crate::mpris::MprisTrack {
                uri: np.track.uri.clone(),
                title: np.track.title.clone(),
                artist: np.track.artist.clone(),
                album: np.track.album.clone(),
                art_url: np.track.art_url.clone(),
                duration: np.track.duration,
            }),
            position: now_playing.map_or(Duration::ZERO, |np| np.position),
            playing: now_playing.is_some_and(|np| np.playing),
            volume: now_playing.map_or(1.0, |np| f64::from(np.volume) / 100.0),
            shuffle: self.shuffle,
            repeat: self.repeat,
        }
    }

    pub fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Tick => {
                let mut ended = false;
                if let Some(np) = &mut self.now_playing {
                    if np.playing {
                        if self.playback_mode == PlaybackMode::Online {
                            np.position += Duration::from_millis(250);
                            if let Some(track) = self.tracks.iter().find(|t| t.uri == np.track.uri)
                            {
                                ended = np.position >= track.duration;
                            }
                        } else if let Some(player) = &self.offline_player {
                            np.position = player.position();
                            ended = player.is_finished();
                        }
                    }
                }
                if ended && !self.offline_end_handled {
                    self.handle_track_end();
                }
                self.sync_lyric_cursor();
            }
            Action::Resize(width, height) => self.set_terminal_size(width, height),
            Action::Key(key) => self.handle_key(key)?,
            Action::Loading(message) => {
                self.loading = true;
                self.status = message;
            }
            Action::SearchResults(result) => {
                self.loading = false;
                match result {
                    Ok(tracks) => {
                        self.current_playlist_id = None;
                        self.set_tracks(tracks);
                        self.status = format!("Found {} tracks", self.tracks.len());
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::PlaylistsLoaded(result) => {
                self.loading = false;
                match result {
                    Ok(playlists) => {
                        self.playlists = playlists
                            .into_iter()
                            .map(|mut playlist| {
                                if let Some(draft) = self.playlist_drafts.get(&playlist.id) {
                                    playlist.name = draft.name.clone();
                                    playlist.track_count = Some(draft.tracks.len() as u32);
                                }
                                playlist
                            })
                            .collect();
                        self.status = format!("Loaded {} playlists", self.playlists.len());
                        self.load_selected_playlist_cover();
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::PlaylistDetailsLoaded(result) => {
                self.loading = false;
                match result {
                    Ok((mut playlist, server_tracks)) => {
                        let id = playlist.id.clone();
                        // Reopening a playlist with staged edits restores the
                        // local draft instead of silently losing the changes.
                        let tracks = if let Some(draft) = self.playlist_drafts.get(&id) {
                            playlist.name = draft.name.clone();
                            playlist.track_count = Some(draft.tracks.len() as u32);
                            draft.tracks.clone()
                        } else {
                            server_tracks
                        };
                        self.current_playlist_id = Some(id.clone());
                        if let Some(existing) = self.playlists.iter_mut().find(|item| item.id == id)
                        {
                            *existing = playlist;
                        } else {
                            self.playlists.push(playlist);
                        }
                        self.set_tracks(tracks);
                        self.tab = Tab::Search;
                        let dirty = self
                            .playlist_drafts
                            .get(&id)
                            .is_some_and(PlaylistDraft::is_dirty);
                        self.status = if dirty {
                            format!(
                                "Restored {} unsaved playlist edits — S save / D discard",
                                self.tracks.len()
                            )
                        } else {
                            format!("Loaded {} playlist tracks", self.tracks.len())
                        };
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::PlaylistCreated(result) => {
                self.loading = false;
                match result {
                    Ok(playlist) => {
                        let name = playlist.name.clone();
                        self.playlists.push(playlist);
                        self.status = format!("Created playlist '{name}'");
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::PlaylistRenamed { id, result } => {
                self.loading = false;
                match result {
                    Ok(name) => {
                        if let Some(playlist) = self.playlists.iter_mut().find(|p| p.id == id) {
                            playlist.name = name.clone();
                        }
                        self.status = format!("Renamed playlist to '{name}'");
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::PlaylistEdited {
                message,
                result,
                reload_playlists,
                reload_playlist,
            } => {
                self.loading = false;
                match result {
                    Ok(()) => {
                        self.status = message;
                        if reload_playlists {
                            self.load_playlists();
                        } else if reload_playlist.as_deref() == self.current_playlist_id.as_deref()
                            && self.current_playlist_id.is_some()
                        {
                            self.load_playlist_tracks(reload_playlist.unwrap());
                        }
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::PlaylistSaved { id, name, result } => {
                self.loading = false;
                match result {
                    Ok(()) => {
                        self.playlist_drafts.remove(&id);
                        if let Some(playlist) = self.playlists.iter_mut().find(|p| p.id == id) {
                            playlist.name = name.clone();
                        }
                        self.status = format!("Saved playlist '{name}'");
                        if self.current_playlist_id.as_deref() == Some(id.as_str()) {
                            self.load_playlist_tracks(id);
                        } else {
                            self.load_playlists();
                        }
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::LikedSongsLoaded(result) => {
                self.loading = false;
                match result {
                    Ok(tracks) => {
                        self.current_playlist_id = None;
                        self.set_tracks(tracks);
                        self.tab = Tab::Search;
                        self.status = format!("Loaded {} liked songs", self.tracks.len());
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::DownloadFinished { uri, result } => {
                self.downloading.remove(&uri);
                let playlist = self
                    .playlist_downloads
                    .values_mut()
                    .find(|progress| progress.uris.contains(&uri));
                if let Some(progress) = playlist {
                    progress.completed += 1;
                    match result {
                        Ok(entry) => {
                            let audio_path = self.downloads.audio_path(&entry);
                            if self.downloads.insert(entry).is_err() {
                                let _ = std::fs::remove_file(audio_path);
                                progress.failed += 1;
                            }
                        }
                        Err(_) => progress.failed += 1,
                    }
                    let name = progress.name.clone();
                    let total = progress.total;
                    let completed = progress.completed;
                    let failed = progress.failed;
                    self.status = if completed >= total {
                        format!(
                            "Finished playlist '{name}': {} saved, {failed} failed",
                            total.saturating_sub(failed)
                        )
                    } else {
                        format!(
                            "Downloading playlist '{name}': {completed}/{total} ({failed} failed)"
                        )
                    };
                } else {
                    match result {
                        Ok(entry) => {
                            let title = entry.track.title.clone();
                            let audio_path = self.downloads.audio_path(&entry);
                            if let Err(err) = self.downloads.insert(entry) {
                                let _ = std::fs::remove_file(audio_path);
                                self.set_error(format!("Could not index download: {err:#}"));
                            } else {
                                self.status = format!("Downloaded '{title}' for offline listening");
                            }
                        }
                        Err(err) => self.set_error(format!("Download failed: {err}")),
                    }
                }
            }
            Action::PlaylistDownloadStarted {
                id,
                name,
                total,
                uris,
                members,
            } => {
                let progress = self
                    .playlist_downloads
                    .entry(id.clone())
                    .or_insert_with(|| PlaylistDownload {
                        name: name.clone(),
                        ..PlaylistDownload::default()
                    });
                progress.name = name.clone();
                progress.total = total;
                progress.uris = uris.into_iter().collect();
                progress.members = members;
                for uri in &progress.uris {
                    self.downloading.insert(uri.clone());
                }
                self.status = if total == 0 {
                    format!("All tracks in '{name}' are already offline")
                } else {
                    format!("Downloading playlist '{name}': 0/{total}")
                };
            }
            Action::PlaylistDownloadFinished {
                id,
                name,
                total,
                failed,
                members,
                error,
            } => {
                self.playlist_downloads.remove(&id);
                if let Some(message) = error {
                    self.set_error(message);
                    return Ok(());
                }
                if let Err(err) = self.downloads.insert_playlist(&id, &name, &members) {
                    self.set_error(format!("Could not save offline playlist: {err:#}"));
                    return Ok(());
                }
                if self.tab == Tab::Downloads {
                    self.load_downloaded_tracks();
                }
                self.status = if total == 0 {
                    format!("All tracks in '{name}' are offline — press Enter to play")
                } else if failed > 0 {
                    format!(
                        "Downloaded {}/{} tracks from '{name}' ({} failed)",
                        total - failed,
                        total,
                        failed
                    )
                } else {
                    format!("Downloaded {total} tracks from '{name}' for offline listening")
                };
            }
            Action::PlayerEndOfTrack if self.playback_mode == PlaybackMode::Online => {
                self.handle_track_end();
            }
            Action::PlayerEndOfTrack => {}
            Action::PlayerPlaying(position_ms) if self.playback_mode == PlaybackMode::Online => {
                if let Some(np) = &mut self.now_playing {
                    np.playing = true;
                    np.position = Duration::from_millis(u64::from(position_ms));
                }
            }
            Action::PlayerPaused(position_ms) if self.playback_mode == PlaybackMode::Online => {
                if let Some(np) = &mut self.now_playing {
                    np.playing = false;
                    np.position = Duration::from_millis(u64::from(position_ms));
                }
            }
            Action::PlayerPlaying(_) | Action::PlayerPaused(_) => {}
            Action::LyricsLoaded { uri, result } => {
                self.lyrics_loading = false;
                match result {
                    Ok(Some(lyrics)) => {
                        self.no_lyrics.remove(&uri);
                        self.persist_lyrics_cache(&uri, &Some(lyrics.clone()));
                        self.lyrics.insert(uri.clone(), lyrics);
                        self.status = "Lyrics loaded".to_string();
                        self.sync_lyric_cursor();
                        if self.translate_lyrics {
                            self.fetch_translation(&uri);
                        }
                    }
                    Ok(None) => {
                        self.no_lyrics.insert(uri.clone());
                        self.persist_lyrics_cache(&uri, &None);
                        self.status = "No lyrics found for this track".to_string();
                    }
                    Err(err) => self.status = format!("Lyrics unavailable: {err}"),
                }
            }
            Action::TranslationsLoaded { uri, result } => {
                self.translation_loading = false;
                match result {
                    Ok(lines) => {
                        self.translations.insert(uri, lines);
                        self.status = "Translation on".to_string();
                    }
                    Err(err) => {
                        self.translate_lyrics = false;
                        self.set_error(format!("Translation failed: {err}"));
                    }
                }
            }
            Action::CoverArtLoaded {
                target,
                url,
                result,
            } => {
                if !self.art_enabled {
                    return Ok(());
                }
                let current = match &target {
                    CoverArtTarget::Track { uri } => self
                        .now_playing
                        .as_ref()
                        .filter(|np| &np.track.uri == uri)
                        .and_then(|np| np.track.art_url.as_deref()),
                    CoverArtTarget::Playlist { id } => self
                        .playlists
                        .iter()
                        .find(|playlist| &playlist.id == id)
                        .and_then(|playlist| playlist.art_url.as_deref()),
                };
                if current != Some(url.as_str()) {
                    return Ok(()); // stale response for a previous item
                }
                let size = self.art_size();
                self.art = match result {
                    Ok(image) if size.width == 0 || size.height == 0 => {
                        CoverArt::deferred(url, image)
                    }
                    Ok(image) => match art::build_protocol(&self.picker, image.clone(), size) {
                        Ok(protocol) => CoverArt::ready(url, image, protocol),
                        Err(err) => CoverArt::failed(url, format!("{err:#}")),
                    },
                    Err(err) => CoverArt::failed(url, err),
                };
            }
            Action::MprisPlayPause => self.toggle_play(),
            Action::MprisPlay => self.set_playing(true),
            Action::MprisPause => self.set_playing(false),
            Action::MprisNext => self.next_track(),
            Action::MprisPrev => self.prev_track(),
            Action::MprisSeek(millis) => self.seek_millis(millis),
            Action::MprisSetPosition { track, position_ms } => {
                let current = self
                    .now_playing
                    .as_ref()
                    .map(|np| crate::mpris::track_id(&np.track.uri));
                if current.as_deref() == Some(track.as_str()) {
                    self.seek_to(position_ms);
                }
            }
            Action::MprisSetVolume(volume) => {
                if volume.is_finite() {
                    self.set_volume_percent((volume.clamp(0.0, 1.0) * 100.0).round() as u8);
                }
            }
            Action::MprisSetShuffle(shuffle) => {
                if shuffle != self.shuffle {
                    self.toggle_shuffle();
                }
            }
            Action::MprisSetRepeat(repeat) => self.set_repeat(repeat),
            Action::MprisOpenUri(uri) => self.play_uri(&uri),
            Action::MprisQuit => self.running = false,
            Action::Error(message) => self.set_error(message),
        }
        Ok(())
    }

    fn set_error(&mut self, message: String) {
        self.loading = false;
        self.error = Some(message.clone());
        self.status = format!("Error: {message}");
    }

    pub fn active_playlist_draft(&self) -> Option<&PlaylistDraft> {
        self.current_playlist_id
            .as_deref()
            .and_then(|id| self.playlist_drafts.get(id))
    }

    pub fn playlist_is_dirty(&self) -> bool {
        self.active_playlist_draft()
            .is_some_and(PlaylistDraft::is_dirty)
    }

    /// Prefer the open playlist, but allow saving a dirty draft left behind on
    /// another playlist after the user searched for something else.
    fn draft_id_for_action(&self) -> Option<String> {
        if let Some(id) = &self.current_playlist_id
            && self
                .playlist_drafts
                .get(id)
                .is_some_and(PlaylistDraft::is_dirty)
        {
            return Some(id.clone());
        }
        self.playlist_drafts
            .iter()
            .find(|(_, draft)| draft.is_dirty())
            .map(|(id, _)| id.clone())
    }

    fn ensure_playlist_draft(&mut self) {
        let Some(id) = self.current_playlist_id.clone() else {
            return;
        };
        if self.playlist_drafts.contains_key(&id) {
            return;
        }
        let Some(playlist) = self.playlists.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let draft = PlaylistDraft::new(&playlist, &self.tracks);
        self.playlist_drafts.insert(id, draft);
    }

    fn replace_tracks_keep_selection(&mut self, tracks: Vec<Track>) {
        let selected = self.list_state.selected();
        self.tracks = tracks;
        self.build_play_order();
        self.queue_cursor = 0;
        self.list_state.select(if self.tracks.is_empty() {
            None
        } else {
            Some(selected.unwrap_or(0).min(self.tracks.len() - 1))
        });
    }

    /// Mirror staged draft changes into the visible list and details panes.
    fn apply_draft_to_view(&mut self) {
        let Some(id) = self.current_playlist_id.clone() else {
            return;
        };
        let Some(draft) = self.playlist_drafts.get(&id).cloned() else {
            return;
        };
        if let Some(playlist) = self.playlists.iter_mut().find(|p| p.id == id) {
            playlist.name = draft.name.clone();
            playlist.track_count = Some(draft.tracks.len() as u32);
            let mut contributors = playlist.contributors.clone();
            for track in &draft.tracks {
                if let Some(name) = &track.added_by
                    && !contributors.contains(name)
                {
                    contributors.push(name.clone());
                }
            }
            playlist.contributors = contributors;
        }
        self.replace_tracks_keep_selection(draft.tracks.clone());
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Ignore key release/repeat events; only act on presses.
        if key.kind == crossterm::event::KeyEventKind::Release {
            return Ok(());
        }

        // Ctrl+C always quits, even while an overlay owns the keyboard.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.running = false;
            return Ok(());
        }

        if self.playlist_picker.is_some() {
            self.handle_playlist_picker_key(key);
            return Ok(());
        }

        if self.prompt.is_some() {
            self.handle_prompt_key(key);
            return Ok(());
        }

        if self.search_active {
            match key.code {
                KeyCode::Esc => {
                    self.search_active = false;
                    self.status = "Search cancelled".to_string();
                }
                KeyCode::Enter => {
                    self.search_active = false;
                    let query = self.search_query.trim().to_string();
                    if query.is_empty() {
                        self.status = "Search cleared".to_string();
                    } else {
                        self.status = format!("Searching for '{query}'...");
                        self.loading = true;
                        self.search_tracks(query);
                    }
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                }
                KeyCode::Char(c) => self.search_query.push(c),
                _ => {}
            }
            return Ok(());
        }

        if self.show_help {
            if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter) {
                self.show_help = false;
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') => self.running = false,
            KeyCode::Char('j') | KeyCode::Down => match self.tab {
                Tab::Lyrics => self.move_lyric_cursor(1),
                Tab::Queue => self.move_queue_cursor(1),
                _ => self.move_selection(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.tab {
                Tab::Lyrics => self.move_lyric_cursor(-1),
                Tab::Queue => self.move_queue_cursor(-1),
                _ => self.move_selection(-1),
            },
            KeyCode::Char('g') | KeyCode::Home => match self.tab {
                Tab::Lyrics => self.set_lyric_cursor(0),
                Tab::Queue => self.queue_cursor = 0,
                _ => self.move_to(0),
            },
            KeyCode::Char('G') | KeyCode::End => match self.tab {
                Tab::Lyrics => {
                    let last = self
                        .current_lyrics()
                        .map_or(0, |l| l.lines.len().saturating_sub(1));
                    self.set_lyric_cursor(last);
                }
                Tab::Queue => self.queue_cursor = self.play_order.len().saturating_sub(1),
                _ if self.list_len() > 0 => self.move_to(self.list_len() - 1),
                _ => {}
            },
            KeyCode::PageDown => match self.tab {
                Tab::Lyrics => self.move_lyric_cursor(10),
                Tab::Queue => self.move_queue_cursor(10),
                _ => self.move_selection(10),
            },
            KeyCode::PageUp => match self.tab {
                Tab::Lyrics => self.move_lyric_cursor(-10),
                Tab::Queue => self.move_queue_cursor(-10),
                _ => self.move_selection(-10),
            },
            KeyCode::Char('J') if self.tab == Tab::Queue => self.reorder_queue(1),
            KeyCode::Char('K') if self.tab == Tab::Queue => self.reorder_queue(-1),
            // With a playlist open, J/K are a faster alternative to [ ].
            KeyCode::Char('J') if self.tab == Tab::Search && self.current_playlist_id.is_some() => {
                self.move_playlist_track(1)
            }
            KeyCode::Char('K') if self.tab == Tab::Search && self.current_playlist_id.is_some() => {
                self.move_playlist_track(-1)
            }
            KeyCode::Tab | KeyCode::BackTab => {
                // crossterm reports Shift+Tab as BackTab; some terminals instead
                // send Tab with the SHIFT modifier, so handle both.
                let backward =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.tab = if backward {
                    self.tab.prev()
                } else {
                    self.tab.next()
                };
                if self.tab == Tab::Downloads {
                    self.load_downloaded_tracks();
                } else if self.tab == Tab::Playlists {
                    self.load_selected_playlist_cover();
                    self.status = format!("Tab: {}", self.tab.title());
                } else {
                    self.status = format!("Tab: {}", self.tab.title());
                }
            }
            KeyCode::Char('/') => {
                self.search_active = true;
                self.search_query.clear();
                self.status = "Search: type query, Enter to submit, Esc to cancel".to_string();
            }
            KeyCode::Enter => match self.tab {
                Tab::Queue => self.play_queue_item(),
                Tab::Lyrics => self.jump_to_lyric(),
                Tab::Downloads => self.play_downloaded_track(),
                _ => self.select_current(),
            },
            KeyCode::Char(' ') => self.toggle_play(),
            KeyCode::Char('n') => self.next_track(),
            KeyCode::Char('p') => self.prev_track(),
            KeyCode::Left => self.seek(-5),
            KeyCode::Right => self.seek(5),
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust_volume(5),
            KeyCode::Char('-') => self.adjust_volume(-5),
            KeyCode::Char('l') => {
                self.tab = Tab::Lyrics;
                self.status = "Lyrics".to_string();
            }
            KeyCode::Char('Q') => {
                self.tab = Tab::Queue;
                self.status = "Queue — J/K to move tracks".to_string();
            }
            KeyCode::Char('o') => self.show_downloads(),
            KeyCode::Char('d') => self.download_selected(),
            KeyCode::Char('D') if self.tab == Tab::Downloads => self.delete_selected_download(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('r') => self.toggle_repeat(),
            KeyCode::Char('c') => self.toggle_art(),
            KeyCode::Char('a') => self.toggle_auto_scroll(),
            KeyCode::Char('t') => self.toggle_translation(),
            KeyCode::Char('L') => {
                self.status = "Loading liked songs...".to_string();
                self.loading = true;
                self.load_liked_songs();
            }
            // Playlist editing keys are upper-case so they don't collide with
            // the single-letter navigation/playback commands.
            KeyCode::Char('N') => self.begin_new_playlist(),
            KeyCode::Char('R') => self.begin_rename_playlist(),
            KeyCode::Char('A') => self.begin_add_to_playlist(),
            KeyCode::Char('X') => self.remove_selected_playlist_track(),
            KeyCode::Char('[') => self.move_playlist_track(-1),
            KeyCode::Char(']') => self.move_playlist_track(1),
            KeyCode::Char('S') => self.save_playlist_draft(),
            KeyCode::Char('D') => self.discard_playlist_draft(),
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        Ok(())
    }

    /// Key handling while the "add track to playlist" overlay is open.
    fn handle_playlist_picker_key(&mut self, key: KeyEvent) {
        enum PickerStep {
            Move(i32),
            First,
            Last,
            Confirm,
            Cancel,
            Ignore,
        }
        let step = match key.code {
            KeyCode::Esc => PickerStep::Cancel,
            KeyCode::Enter => PickerStep::Confirm,
            KeyCode::Up | KeyCode::Char('k') => PickerStep::Move(-1),
            KeyCode::Down | KeyCode::Char('j') => PickerStep::Move(1),
            KeyCode::Char('g') => PickerStep::First,
            KeyCode::Char('G') => PickerStep::Last,
            _ => PickerStep::Ignore,
        };
        match step {
            PickerStep::Cancel => {
                self.playlist_picker = None;
                self.status = "Add to playlist cancelled".to_string();
            }
            PickerStep::Confirm => {
                let Some(picker) = self.playlist_picker.take() else {
                    return;
                };
                let Some(playlist) = self.playlists.get(picker.cursor).cloned() else {
                    self.status = "No playlist selected".to_string();
                    return;
                };
                self.add_track_to_playlist(playlist, picker.track);
            }
            PickerStep::Move(delta) => {
                if let Some(picker) = &mut self.playlist_picker {
                    let max = self.playlists.len().saturating_sub(1) as i32;
                    picker.cursor = (picker.cursor as i32 + delta).clamp(0, max) as usize;
                }
            }
            PickerStep::First => {
                if let Some(picker) = &mut self.playlist_picker {
                    picker.cursor = 0;
                }
            }
            PickerStep::Last => {
                if let Some(picker) = &mut self.playlist_picker {
                    picker.cursor = self.playlists.len().saturating_sub(1);
                }
            }
            PickerStep::Ignore => {}
        }
    }

    /// Key handling while a playlist name prompt is open.
    fn handle_prompt_key(&mut self, key: KeyEvent) {
        enum PromptStep {
            Cancel,
            Confirm,
            Backspace,
            Insert(char),
            Ignore,
        }
        let step = match key.code {
            KeyCode::Esc => PromptStep::Cancel,
            KeyCode::Enter => PromptStep::Confirm,
            KeyCode::Backspace => PromptStep::Backspace,
            KeyCode::Char(c) => PromptStep::Insert(c),
            _ => PromptStep::Ignore,
        };
        match step {
            PromptStep::Cancel => {
                self.prompt = None;
                self.prompt_input.clear();
                self.status = "Cancelled".to_string();
            }
            PromptStep::Backspace => {
                self.prompt_input.pop();
            }
            PromptStep::Insert(c) => {
                // Playlist names are capped well below Spotify's own limit.
                if self.prompt_input.chars().count() < 100 {
                    self.prompt_input.push(c);
                }
            }
            PromptStep::Confirm => {
                let name = self.prompt_input.trim().to_string();
                let Some(prompt) = self.prompt.take() else {
                    return;
                };
                self.prompt_input.clear();
                if name.is_empty() {
                    self.status = "Playlist name cannot be empty".to_string();
                    return;
                }
                match prompt {
                    PromptKind::NewPlaylist => self.create_playlist(name),
                    PromptKind::RenamePlaylist { id } => self.rename_playlist(id, name),
                }
            }
            PromptStep::Ignore => {}
        }
    }

    fn begin_new_playlist(&mut self) {
        self.prompt = Some(PromptKind::NewPlaylist);
        self.prompt_input.clear();
        self.status = "New playlist (private): type a name, Enter to create".to_string();
    }

    fn begin_rename_playlist(&mut self) {
        let selected = if self.tab == Tab::Playlists {
            self.list_state
                .selected()
                .and_then(|i| self.playlists.get(i))
                .cloned()
                .or_else(|| {
                    self.current_playlist_id
                        .as_deref()
                        .and_then(|id| self.playlists.iter().find(|p| p.id == id).cloned())
                })
        } else {
            None
        };
        let Some(playlist) = selected else {
            self.status = "Select a playlist on the Playlists tab first".to_string();
            return;
        };
        let staged = self.current_playlist_id.as_deref() == Some(playlist.id.as_str());
        self.prompt = Some(PromptKind::RenamePlaylist {
            id: playlist.id.clone(),
        });
        self.prompt_input = playlist.name.clone();
        self.status = if staged {
            "Rename playlist: Enter stages it, S saves, Esc cancels".to_string()
        } else {
            "Rename playlist: Enter to save, Esc to cancel".to_string()
        };
    }

    fn begin_add_to_playlist(&mut self) {
        if self.tab == Tab::Playlists {
            self.status = "Select a track, then press A to add it to a playlist".to_string();
            return;
        }
        let Some(track) = self
            .list_state
            .selected()
            .and_then(|i| self.tracks.get(i))
            .cloned()
        else {
            self.status = "No track selected".to_string();
            return;
        };
        if self.playlists.is_empty() {
            self.status = "No playlists loaded — open the Playlists tab first".to_string();
            return;
        }
        self.playlist_picker = Some(PlaylistPicker { cursor: 0, track });
        self.status = "Add to playlist: Enter to add, Esc to cancel".to_string();
    }

    fn create_playlist(&mut self, name: String) {
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to edit playlists".to_string();
            return;
        };
        self.loading = true;
        self.status = "Creating playlist...".to_string();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = crate::api::create_playlist(&client, &name)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::PlaylistCreated(result)).await;
        });
    }

    fn rename_playlist(&mut self, id: String, name: String) {
        // Renaming the open playlist joins its staged batch; other playlists
        // still use the immediate single-field API call.
        if self.current_playlist_id.as_deref() == Some(id.as_str()) {
            self.ensure_playlist_draft();
            if let Some(draft) = self.playlist_drafts.get_mut(&id) {
                draft.name = name.clone();
            }
            self.apply_draft_to_view();
            self.status = format!("Staged rename to '{name}' — press S to save");
            return;
        }
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to edit playlists".to_string();
            return;
        };
        self.loading = true;
        self.status = "Renaming playlist...".to_string();
        let tx = self.tx.clone();
        let result_name = name.clone();
        tokio::spawn(async move {
            let result = crate::api::rename_playlist(&client, &id, &name)
                .await
                .map(|()| result_name)
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::PlaylistRenamed { id, result }).await;
        });
    }

    fn add_track_to_playlist(&mut self, playlist: Playlist, track: Track) {
        if self.current_playlist_id.as_deref() == Some(playlist.id.as_str()) {
            self.ensure_playlist_draft();
            if let Some(draft) = self.playlist_drafts.get_mut(&playlist.id) {
                let mut staged = track.clone();
                staged.added_by = Some("You".to_string());
                draft.tracks.push(staged);
            }
            self.apply_draft_to_view();
            if let Some(draft) = self.playlist_drafts.get(&playlist.id) {
                self.list_state
                    .select(Some(draft.tracks.len().saturating_sub(1)));
            }
            self.status = format!(
                "Staged '{}' in '{}' — press S to save",
                track.title, playlist.name
            );
            return;
        }
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to edit playlists".to_string();
            return;
        };
        self.loading = true;
        self.status = format!("Adding '{}' to '{}'...", track.title, playlist.name);
        let tx = self.tx.clone();
        let playlist_name = playlist.name.clone();
        let playlist_id = playlist.id.clone();
        let track_title = track.title.clone();
        let track_uri = track.uri.clone();
        tokio::spawn(async move {
            let result = crate::api::add_playlist_track(&client, &playlist_id, &track_uri)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx
                .send(Action::PlaylistEdited {
                    message: format!("Added '{track_title}' to '{playlist_name}'"),
                    result,
                    reload_playlists: false,
                    reload_playlist: Some(playlist_id),
                })
                .await;
        });
    }

    fn remove_selected_playlist_track(&mut self) {
        if self.loading {
            return;
        }
        let Some(id) = self.current_playlist_id.clone() else {
            self.status = "Open a playlist first, then press X to remove a track".to_string();
            return;
        };
        let Some(index) = self.list_state.selected() else {
            self.status = "No track selected".to_string();
            return;
        };
        self.ensure_playlist_draft();
        let removed = {
            let Some(draft) = self.playlist_drafts.get_mut(&id) else {
                return;
            };
            if index >= draft.tracks.len() {
                return;
            }
            draft.tracks.remove(index)
        };
        self.apply_draft_to_view();
        self.status = format!("Staged removal of '{}' — press S to save", removed.title);
    }

    fn move_playlist_track(&mut self, delta: i32) {
        if self.loading {
            return;
        }
        let Some(id) = self.current_playlist_id.clone() else {
            self.status = "Open a playlist first, then press J/K or [ ] to reorder".to_string();
            return;
        };
        let Some(from) = self.list_state.selected() else {
            self.status = "No track selected".to_string();
            return;
        };
        let to =
            (from as i32 + delta).clamp(0, self.tracks.len().saturating_sub(1) as i32) as usize;
        if from == to {
            return;
        }
        self.ensure_playlist_draft();
        let adjusted_to;
        {
            let Some(draft) = self.playlist_drafts.get_mut(&id) else {
                return;
            };
            if from >= draft.tracks.len() {
                return;
            }
            adjusted_to = to.min(draft.tracks.len() - 1);
            let track = draft.tracks.remove(from);
            draft.tracks.insert(adjusted_to, track);
        }
        self.apply_draft_to_view();
        self.list_state.select(Some(adjusted_to));
        self.status = format!(
            "Moved track to position {} (staged) — press S to save",
            adjusted_to + 1
        );
    }

    fn save_playlist_draft(&mut self) {
        if self.loading {
            return;
        }
        let Some(id) = self.draft_id_for_action() else {
            self.status = "No staged playlist changes to save".to_string();
            return;
        };
        let Some(draft) = self.playlist_drafts.get(&id).cloned() else {
            return;
        };
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to save playlist changes".to_string();
            return;
        };
        self.loading = true;
        self.status = format!("Saving playlist '{}'...", draft.name);
        let tx = self.tx.clone();
        let original_name = draft.original_name.clone();
        let name = draft.name.clone();
        let original = draft.original_uris.clone();
        let current = draft.uris();
        tokio::spawn(async move {
            let result =
                crate::api::save_playlist(&client, &id, &original_name, &name, &original, &current)
                    .await
                    .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::PlaylistSaved { id, name, result }).await;
        });
    }

    fn discard_playlist_draft(&mut self) {
        if self.loading {
            return;
        }
        let Some(id) = self.draft_id_for_action() else {
            self.status = "No staged playlist changes to discard".to_string();
            return;
        };
        let Some(draft) = self.playlist_drafts.remove(&id) else {
            return;
        };
        if let Some(playlist) = self.playlists.iter_mut().find(|p| p.id == id) {
            playlist.name = draft.original_name.clone();
            playlist.track_count = Some(draft.original_tracks.len() as u32);
            let mut contributors = Vec::new();
            for track in &draft.original_tracks {
                if let Some(name) = &track.added_by
                    && !contributors.contains(name)
                {
                    contributors.push(name.clone());
                }
            }
            playlist.contributors = contributors;
        }
        if self.current_playlist_id.as_deref() == Some(id.as_str()) {
            self.replace_tracks_keep_selection(draft.original_tracks.clone());
        }
        self.status = format!("Discarded unsaved changes to '{}'", draft.original_name);
    }

    /// Length of the list currently rendered on the left side.
    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Playlists => self.playlists.len(),
            Tab::Downloads => self.offline_items.len(),
            _ => self.tracks.len(),
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, len as i32 - 1) as usize;
        self.list_state.select(Some(next));
        if self.tab == Tab::Playlists {
            self.load_selected_playlist_cover();
        }
    }

    fn move_to(&mut self, index: usize) {
        let len = self.list_len();
        if len > 0 {
            self.list_state.select(Some(index.min(len - 1)));
            if self.tab == Tab::Playlists {
                self.load_selected_playlist_cover();
            }
        }
    }

    fn current_lyrics(&self) -> Option<&crate::lyrics::Lyrics> {
        self.lyrics
            .get(self.now_playing.as_ref()?.track.uri.as_str())
    }

    fn move_lyric_cursor(&mut self, delta: i32) {
        let line_count = self.current_lyrics().map_or(0, |lyrics| lyrics.lines.len());
        if line_count == 0 {
            return;
        }
        self.lyrics_auto_scroll = false;
        self.lyric_cursor =
            (self.lyric_cursor as i32 + delta).clamp(0, line_count as i32 - 1) as usize;
    }

    fn set_lyric_cursor(&mut self, index: usize) {
        let line_count = self.current_lyrics().map_or(0, |lyrics| lyrics.lines.len());
        if line_count == 0 {
            return;
        }
        self.lyrics_auto_scroll = false;
        self.lyric_cursor = index.min(line_count - 1);
    }

    /// In follow mode the cursor tracks the line currently being played.
    fn sync_lyric_cursor(&mut self) {
        if !self.lyrics_auto_scroll {
            return;
        }
        let Some(lyrics) = self.current_lyrics() else {
            return;
        };
        let position = self
            .now_playing
            .as_ref()
            .map_or(Duration::ZERO, |np| np.position);
        self.lyric_cursor = crate::lyrics::active_line(lyrics, position).unwrap_or(0);
    }

    /// Seek playback to the timestamp of the selected lyric line.
    fn jump_to_lyric(&mut self) {
        let Some((time, text)) = self.current_lyrics().and_then(|lyrics| {
            lyrics
                .lines
                .get(self.lyric_cursor)
                .map(|line| (line.time, line.text.clone()))
        }) else {
            self.status = "No lyrics for this track".to_string();
            return;
        };
        let Some(np) = &self.now_playing else {
            return;
        };
        let target = time.min(np.track.duration);
        self.lyrics_auto_scroll = true;
        self.seek_to(target.as_millis() as u64);
        self.status = format!("Jumped to {} — {}", format_duration(target), text);
    }

    fn move_queue_cursor(&mut self, delta: i32) {
        if self.play_order.is_empty() {
            return;
        }
        self.queue_cursor =
            (self.queue_cursor as i32 + delta).clamp(0, self.play_order.len() as i32 - 1) as usize;
    }

    /// Swap the selected queue entry with its neighbour so it plays earlier or
    /// later. The cursor follows the moved track.
    fn reorder_queue(&mut self, delta: i32) {
        if self.play_order.len() < 2 {
            return;
        }
        let from = self.queue_cursor.min(self.play_order.len() - 1);
        let to = (from as i32 + delta).clamp(0, self.play_order.len() as i32 - 1) as usize;
        if from == to {
            return;
        }
        self.play_order.swap(from, to);
        self.queue_cursor = to;
        self.status = format!("Queue: moved track to position {}", to + 1);
    }

    /// Play the track at the queue cursor by pointing the list selection at it.
    fn play_queue_item(&mut self) {
        let Some(&track_index) = self.play_order.get(self.queue_cursor) else {
            return;
        };
        self.list_state.select(Some(track_index));
        self.select_current();
    }

    /// Replace the track list and rebuild the playback order.
    fn set_tracks(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        self.build_play_order();
        self.queue_cursor = 0;
        self.list_state.select(if self.tracks.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    /// Build the order used by next/prev. With shuffle on, the current track is
    /// kept first so playback continues from where it was.
    fn build_play_order(&mut self) {
        let mut order: Vec<usize> = (0..self.tracks.len()).collect();
        if self.shuffle {
            use rand::seq::SliceRandom;
            order.shuffle(&mut rand::thread_rng());
            let current_uri = self.now_playing.as_ref().map(|np| np.track.uri.clone());
            if let Some(uri) = current_uri {
                if let Some(index) = self.tracks.iter().position(|track| track.uri == uri) {
                    if let Some(position) = order.iter().position(|&i| i == index) {
                        let first = order.remove(position);
                        order.insert(0, first);
                    }
                }
            }
        }
        self.play_order = order;
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.build_play_order();
        self.status = if self.shuffle {
            format!("Shuffle on ({} tracks)", self.tracks.len())
        } else {
            "Shuffle off".to_string()
        };
    }

    fn toggle_repeat(&mut self) {
        self.set_repeat(self.repeat.next());
    }

    fn set_repeat(&mut self, repeat: RepeatMode) {
        self.repeat = repeat;
        self.status = if repeat == RepeatMode::All && self.current_playlist_id.is_some() {
            "Repeat playlist".to_string()
        } else {
            format!("Repeat {}", repeat.label())
        };
    }

    /// End-of-track behaviour, distinct from manual `n`: repeat-one restarts the
    /// track, repeat-all restarts the playlist (in the current/shuffled order),
    /// and repeat-off stops at the end of the queue.
    fn handle_track_end(&mut self) {
        self.offline_end_handled = true;
        if self.repeat == RepeatMode::One {
            self.restart_track();
            return;
        }
        let at_last = self.play_position() + 1 >= self.play_order.len().max(1);
        if at_last && self.repeat == RepeatMode::Off {
            self.set_playing(false);
            self.status = "End of queue — press n or enable repeat with r".to_string();
            return;
        }
        let was_playing = self.now_playing.as_ref().is_some_and(|np| np.playing);
        let repeating_playlist = at_last
            && self.repeat == RepeatMode::All
            && (self.current_playlist_id.is_some() || self.offline_queue.is_some());
        if repeating_playlist {
            self.restart_playlist();
        } else {
            self.next_track();
        }
        if was_playing {
            self.set_playing(true);
        }
        if repeating_playlist {
            self.status = format!("Repeating playlist '{}'", self.current_playlist_label());
        }
    }

    /// Return to the first track of the current playback order. With shuffle
    /// enabled that is the first entry of the shuffled `play_order`, so a whole
    /// shuffle cycle repeats instead of only wrapping one track at a time.
    fn restart_playlist(&mut self) {
        if self.tracks.is_empty() {
            return;
        }
        if self.play_order.len() != self.tracks.len() {
            self.build_play_order();
        }
        let Some(&first) = self.play_order.first() else {
            return;
        };
        self.set_queue_track_index(first);
        self.queue_cursor = 0;
        self.select_current();
    }

    fn current_playlist_label(&self) -> String {
        if let Some(queue) = &self.offline_queue {
            return queue.name();
        }
        let Some(id) = &self.current_playlist_id else {
            return "playlist".to_string();
        };
        self.playlists
            .iter()
            .find(|playlist| &playlist.id == id)
            .map_or_else(|| "playlist".to_string(), |playlist| playlist.name.clone())
    }

    /// Reload the current URI from the start so librespot really restarts it.
    fn restart_track(&mut self) {
        let track = {
            let Some(np) = &mut self.now_playing else {
                return;
            };
            np.position = Duration::ZERO;
            np.playing = true;
            np.track.clone()
        };
        self.lyrics_auto_scroll = true;
        self.lyric_cursor = 0;
        self.sync_lyric_cursor();
        if let Err(err) = self.play_track(&track, Duration::ZERO) {
            self.set_error(format!("Could not repeat track: {err}"));
            return;
        }
        self.status = format!("Repeating {}", track.title);
    }

    fn toggle_auto_scroll(&mut self) {
        self.lyrics_auto_scroll = !self.lyrics_auto_scroll;
        if self.lyrics_auto_scroll {
            self.sync_lyric_cursor();
        }
        self.status = if self.lyrics_auto_scroll {
            "Lyrics follow playback".to_string()
        } else {
            "Lyrics: select a line with j/k, Enter to jump there".to_string()
        };
    }

    fn toggle_translation(&mut self) {
        if self.tab != Tab::Lyrics {
            self.status = "Open the Lyrics tab with l first".to_string();
            return;
        }
        let Some(np) = &self.now_playing else {
            self.status = "No track selected".to_string();
            return;
        };
        let uri = np.track.uri.clone();
        self.translate_lyrics = !self.translate_lyrics;
        if self.translate_lyrics {
            if self.translations.contains_key(&uri) {
                self.status = "Translation on".to_string();
            } else {
                self.fetch_translation(&uri);
            }
        } else {
            self.status = "Showing original lyrics".to_string();
        }
    }

    fn toggle_art(&mut self) {
        if let Some(reason) = &self.art_blocked_reason {
            self.status = format!("Cover art unavailable ({reason})");
            return;
        }
        self.art_enabled = !self.art_enabled;
        if self.art_enabled {
            self.status = "Cover art on".to_string();
            if self.tab == Tab::Playlists {
                self.load_selected_playlist_cover();
            } else if let Some(track) = self.now_playing.as_ref().map(|np| np.track.clone()) {
                self.load_cover_art(&track);
            } else {
                self.art = CoverArt::no_art();
            }
        } else {
            self.art = CoverArt::disabled("press c to show");
            self.status = "Cover art hidden".to_string();
        }
    }

    /// Start a background fetch for the selected track's cover art. Repeated
    /// selections of the same track do not restart a load or replace a ready
    /// image.
    fn load_cover_art(&mut self, track: &Track) {
        self.load_cover_art_url(
            CoverArtTarget::Track {
                uri: track.uri.clone(),
            },
            track.art_url.clone(),
        );
    }

    /// Show the selected playlist's own cover image in the Details pane.
    fn load_selected_playlist_cover(&mut self) {
        if self.tab != Tab::Playlists {
            return;
        }
        let selected = self
            .list_state
            .selected()
            .and_then(|index| self.playlists.get(index))
            .cloned();
        let Some(playlist) = selected else {
            self.art = CoverArt::no_art();
            return;
        };
        self.load_cover_art_url(
            CoverArtTarget::Playlist {
                id: playlist.id.clone(),
            },
            playlist.art_url.clone(),
        );
    }

    fn load_cover_art_url(&mut self, target: CoverArtTarget, url: Option<String>) {
        if !self.art_enabled {
            return;
        }
        let Some(url) = url else {
            self.art = CoverArt::no_art();
            return;
        };
        if self.art.url() == Some(url.as_str())
            && (matches!(self.art.status(), ArtStatus::Ready | ArtStatus::Loading)
                || matches!(self.art, CoverArt::Deferred { .. }))
        {
            return;
        }

        self.art = CoverArt::loading(url.clone());
        let tx = self.tx.clone();
        let client = self.http_client.clone();
        let fetch_url = url.clone();
        tokio::spawn(async move {
            let result = art::fetch_image(&client, &fetch_url)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::CoverArtLoaded {
                target,
                url,
                result,
            })
            .await;
        });
    }

    fn select_current(&mut self) {
        let index = self.list_state.selected();

        if self.tab == Tab::Playlists {
            if let Some(playlist) = index.and_then(|i| self.playlists.get(i)) {
                self.load_playlist_tracks(playlist.id.clone());
            }
            return;
        }

        let track = if self.tab == Tab::Downloads {
            // The left list contains offline playlists and group rows before the
            // individual tracks, so map the selected row back to a queue index.
            match index.and_then(|i| self.offline_items.get(i)).cloned() {
                Some(OfflineItem::Track { uri }) => {
                    let track_index = self.tracks.iter().position(|track| track.uri == uri);
                    self.offline_track_index = track_index.unwrap_or(self.offline_track_index);
                    self.tracks.get(self.offline_track_index).cloned()
                }
                Some(OfflineItem::Playlist { .. } | OfflineItem::AllTracks) => {
                    self.offline_track_index = 0;
                    self.tracks.first().cloned()
                }
                None => None,
            }
        } else {
            index.and_then(|i| self.tracks.get(i)).cloned()
        };
        let Some(track) = track else { return };

        let volume = self.now_playing.as_ref().map_or(60, |np| np.volume);
        // Downloaded tracks play from disk, even while signed in. Demo mode has
        // neither backend and stays paused until Space is pressed.
        let playing = self.player.is_some() || self.downloads.contains(&track.uri);
        self.now_playing = Some(NowPlaying {
            track: track.clone(),
            position: Duration::ZERO,
            playing,
            volume,
        });
        // A new track starts with lyrics following playback again.
        self.lyrics_auto_scroll = true;
        self.lyric_cursor = 0;
        self.sync_lyric_cursor();
        // Keep the queue cursor on whatever is now playing.
        self.queue_cursor = self.play_position();

        let was_online = self.playback_mode == PlaybackMode::Online;
        match self.play_track(&track, Duration::ZERO) {
            Ok(()) => {
                self.status = if self.playback_mode == PlaybackMode::Offline {
                    format!("Playing offline copy: {}", track.title)
                } else if self.player.is_some() {
                    format!("Playing: {}", track.title)
                } else {
                    format!("Selected: {} (demo mode)", track.title)
                };
            }
            Err(err) => {
                if was_online {
                    self.playback_mode = PlaybackMode::Online;
                }
                if let Some(np) = &mut self.now_playing {
                    np.playing = false;
                }
                self.set_error(format!("Could not play track: {err}"));
            }
        }

        self.fetch_lyrics(&track);
        self.load_cover_art(&track);
    }

    fn toggle_play(&mut self) {
        let playing = self.now_playing.as_ref().is_some_and(|np| !np.playing);
        self.set_playing(playing);
    }

    /// Idempotent play/pause used by both keybindings and MPRIS controls.
    fn set_playing(&mut self, playing: bool) {
        let Some(np) = &mut self.now_playing else {
            return;
        };
        // An offline sink is empty after the file ends, so pressing play again
        // has to reload the file rather than just unpause an empty queue.
        let restart = playing
            && self.playback_mode == PlaybackMode::Offline
            && self
                .offline_player
                .as_ref()
                .is_some_and(OfflinePlayer::is_finished);
        if np.playing == playing && !restart {
            return;
        }
        np.playing = playing;
        let track = np.track.clone();
        if restart {
            if let Err(err) = self.play_track(&track, Duration::ZERO) {
                if let Some(np) = &mut self.now_playing {
                    np.playing = false;
                }
                self.set_error(format!("Could not restart offline track: {err:#}"));
                return;
            }
        } else if self.playback_mode == PlaybackMode::Offline {
            if let Some(player) = &self.offline_player {
                if playing {
                    player.play();
                } else {
                    player.pause();
                }
            }
        } else if let Some(player) = &self.player {
            if playing {
                player.play();
            } else {
                player.pause();
            }
        }
        self.status = if playing { "Playing" } else { "Paused" }.to_string();
    }

    /// Position of the selected track in the current playback order.
    fn play_position(&self) -> usize {
        let selected = if self.tab == Tab::Downloads {
            Some(self.offline_track_index)
        } else {
            self.list_state.selected()
        };
        self.play_order
            .iter()
            .position(|&index| Some(index) == selected)
            .unwrap_or(0)
    }

    /// Point playback at a track index without confusing the Offline tab's
    /// group rows with tracks.
    fn set_queue_track_index(&mut self, track_index: usize) {
        if self.tab == Tab::Downloads {
            self.offline_track_index = track_index;
            if let Some(track) = self.tracks.get(track_index) {
                let uri = track.uri.clone();
                self.select_downloads_row_for_uri(&uri);
            }
        } else {
            self.list_state.select(Some(track_index));
        }
    }

    fn next_track(&mut self) {
        if self.tracks.is_empty() {
            return;
        }
        if self.play_order.len() != self.tracks.len() {
            self.build_play_order();
        }
        let next = (self.play_position() + 1) % self.play_order.len();
        let track_index = self.play_order[next];
        self.set_queue_track_index(track_index);
        self.select_current();
    }

    fn prev_track(&mut self) {
        if self.tracks.is_empty() {
            return;
        }
        if self.play_order.len() != self.tracks.len() {
            self.build_play_order();
        }
        let position = self.play_position();
        let prev = if position == 0 {
            self.play_order.len() - 1
        } else {
            position - 1
        };
        let track_index = self.play_order[prev];
        self.set_queue_track_index(track_index);
        self.select_current();
    }

    /// Relative seek, in seconds (used by the left/right keybindings).
    fn seek(&mut self, seconds: i64) {
        self.seek_millis(seconds.saturating_mul(1000));
    }

    /// Relative seek, in milliseconds (used by MPRIS `Seek`).
    fn seek_millis(&mut self, delta_ms: i64) {
        let Some(np) = &self.now_playing else { return };
        let current = np.position.as_millis() as i64;
        let target =
            (current.saturating_add(delta_ms)).clamp(0, np.track.duration.as_millis() as i64);
        self.seek_to(target as u64);
    }

    /// Absolute seek to a position within the current track.
    fn seek_to(&mut self, position_ms: u64) {
        let Some(np) = &mut self.now_playing else {
            return;
        };
        let target = Duration::from_millis(position_ms).min(np.track.duration);
        np.position = target;
        if self.playback_mode == PlaybackMode::Offline {
            if let Some(player) = &self.offline_player {
                player.seek(target);
            }
        } else if let Some(player) = &self.player {
            player.seek(target.as_millis().min(u128::from(u32::MAX)) as u32);
        }
        self.status = format!("Seek to {:.0}s", target.as_secs_f64());
        self.sync_lyric_cursor();
    }

    fn adjust_volume(&mut self, delta: i16) {
        let Some(np) = &self.now_playing else { return };
        let next = (np.volume as i16 + delta).clamp(0, 100) as u8;
        self.set_volume_percent(next);
    }

    /// Set volume in percent, shared by keybindings and MPRIS.
    fn set_volume_percent(&mut self, percent: u8) {
        let Some(np) = &mut self.now_playing else {
            return;
        };
        let percent = percent.min(100);
        if np.volume == percent {
            return;
        }
        np.volume = percent;
        if self.playback_mode == PlaybackMode::Offline {
            if let Some(player) = &self.offline_player {
                player.set_volume(percent);
            }
        } else if let Some(player) = &self.player {
            player.set_volume(u16::from(percent));
        }
        self.status = format!("Volume: {}%", percent);
    }

    /// Start playing a track by URI (used by MPRIS `OpenUri`).
    fn play_uri(&mut self, uri: &str) {
        let Some(index) = self.tracks.iter().position(|track| track.uri == uri) else {
            self.status = format!("Unknown track: {uri}");
            return;
        };
        self.list_state.select(Some(index));
        self.select_current();
    }

    /// Route playback to the offline file when one exists, otherwise stream.
    fn play_track(&mut self, track: &Track, position: Duration) -> Result<()> {
        let volume = self.now_playing.as_ref().map_or(60, |np| np.volume);
        if let Some(entry) = self.downloads.get(&track.uri).cloned() {
            let path = self.downloads.audio_path(&entry);
            if self.offline_player.is_none() {
                self.offline_player = Some(OfflinePlayer::new()?);
            }
            let player = self.offline_player.as_ref().expect("offline player");
            player.play_file(&path, position)?;
            player.set_volume(volume);
            self.playback_mode = PlaybackMode::Offline;
            self.offline_end_handled = false;
            return Ok(());
        }
        if let Some(player) = &self.player {
            player.play_uri_at(
                &track.uri,
                position.as_millis().min(u128::from(u32::MAX)) as u32,
            )?;
            self.playback_mode = PlaybackMode::Online;
            self.offline_end_handled = false;
        }
        Ok(())
    }

    pub fn search_tracks(&self, query: String) {
        let Some(client) = self.api_client.clone() else {
            self.send(Action::Error("Not signed in".to_string()));
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = crate::api::search_tracks(&client, &query)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::SearchResults(result)).await;
        });
    }

    pub fn load_playlists(&self) {
        let Some(client) = self.api_client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = crate::api::current_user_playlists(&client)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::PlaylistsLoaded(result)).await;
        });
    }

    pub fn load_playlist_tracks(&self, playlist_id: String) {
        let Some(client) = self.api_client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        self.send(Action::Loading("Loading playlist tracks...".to_string()));
        tokio::spawn(async move {
            let result = crate::api::playlist_details(&client, &playlist_id)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::PlaylistDetailsLoaded(result)).await;
        });
    }

    pub fn load_liked_songs(&self) {
        let Some(client) = self.api_client.clone() else {
            self.send(Action::Error("Not signed in".to_string()));
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = crate::api::liked_songs(&client)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::LikedSongsLoaded(result)).await;
        });
    }

    pub fn fetch_lyrics(&mut self, track: &Track) {
        if self.lyrics.contains_key(&track.uri) {
            if self.translate_lyrics && !self.translations.contains_key(&track.uri) {
                self.fetch_translation(&track.uri);
            }
            return;
        }
        if self.no_lyrics.contains(&track.uri) {
            self.status = "No lyrics found for this track (cached)".to_string();
            return;
        }
        self.lyrics_loading = true;
        self.lyric_cursor = 0;
        let tx = self.tx.clone();
        let uri = track.uri.clone();
        let track = track.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let result = crate::lyrics::fetch(&client, &track)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::LyricsLoaded { uri, result }).await;
        });
    }

    /// Translate the current track's lyrics to English in the background.
    pub fn fetch_translation(&mut self, uri: &str) {
        let Some(lyrics) = self.lyrics.get(uri).cloned() else {
            self.translate_lyrics = false;
            self.status = "Lyrics are not loaded yet".to_string();
            return;
        };
        self.translation_loading = true;
        self.status = "Translating lyrics...".to_string();
        let tx = self.tx.clone();
        let uri = uri.to_string();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let result = crate::lyrics::translate_lines(&client, &lyrics.lines, "en")
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::TranslationsLoaded { uri, result }).await;
        });
    }

    pub fn send(&self, action: Action) {
        let _ = self.tx.try_send(action);
    }
}

pub fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// Line where the lyrics view starts. In follow mode the active line is kept
/// near the middle; in manual mode the stored scroll offset is used.
pub fn lyric_scroll_offset(
    active: Option<usize>,
    cursor: Option<usize>,
    line_count: usize,
    auto_scroll: bool,
    height: usize,
) -> usize {
    let max_offset = line_count.saturating_sub(height);
    let focus = if auto_scroll {
        active
    } else {
        cursor.or(active)
    };
    focus
        .unwrap_or(0)
        .saturating_sub(height / 2)
        .min(max_offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        App::new(tx, Picker::halfblocks(), None)
    }

    fn demo_tracks() -> Vec<Track> {
        (0..5)
            .map(|i| Track {
                title: format!("Track {i}"),
                artist: "Artist".into(),
                album: "Album".into(),
                duration: Duration::from_secs(120),
                uri: format!("spotify:track:test{i}"),
                art_url: None,
                added_by: Some("Test User".into()),
            })
            .collect()
    }

    fn demo_playlist(id: &str, name: &str) -> Playlist {
        Playlist {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            track_count: Some(2),
            owner: Some("Alice".to_string()),
            collaborative: true,
            is_public: Some(false),
            contributors: vec!["Alice".into(), "Bob".into()],
            art_url: None,
            snapshot_id: "snapshot-1".to_string(),
        }
    }

    #[tokio::test]
    async fn shuffle_visits_every_track_and_starts_from_current() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        for track in &app.tracks {
            app.lyrics.insert(
                track.uri.clone(),
                crate::lyrics::Lyrics {
                    lines: Vec::new(),
                    synced: true,
                    source: "test".into(),
                },
            );
        }
        app.list_state.select(Some(2));
        app.now_playing = Some(NowPlaying {
            track: app.tracks[2].clone(),
            position: Duration::ZERO,
            playing: false,
            volume: 60,
        });

        app.toggle_shuffle();
        assert!(app.shuffle);
        assert_eq!(app.play_order.len(), app.tracks.len());
        assert_eq!(app.play_order[0], 2);

        let mut seen = std::collections::HashSet::new();
        for _ in 0..app.tracks.len() {
            seen.insert(app.list_state.selected().unwrap());
            app.next_track();
        }
        assert_eq!(seen.len(), app.tracks.len());
        assert_eq!(app.list_state.selected().unwrap(), 2);
    }

    #[tokio::test]
    async fn normal_order_wraps_in_both_directions() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        app.next_track();
        assert_eq!(app.list_state.selected(), Some(1));
        app.prev_track();
        assert_eq!(app.list_state.selected(), Some(0));
        app.prev_track();
        assert_eq!(app.list_state.selected(), Some(4));
    }

    #[test]
    fn queue_reorder_swaps_tracks_and_follows_cursor() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        assert_eq!(app.play_order, vec![0, 1, 2, 3, 4]);

        app.queue_cursor = 3;
        app.reorder_queue(-1);
        assert_eq!(app.play_order, vec![0, 1, 3, 2, 4]);
        assert_eq!(app.queue_cursor, 2);

        app.reorder_queue(1);
        assert_eq!(app.play_order, vec![0, 1, 2, 3, 4]);
        assert_eq!(app.queue_cursor, 3);

        app.queue_cursor = 0;
        app.reorder_queue(-1);
        assert_eq!(app.play_order, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn playing_a_queue_item_keeps_cursor_on_current_track() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        for track in &app.tracks {
            app.lyrics.insert(
                track.uri.clone(),
                crate::lyrics::Lyrics {
                    lines: Vec::new(),
                    synced: true,
                    source: "test".into(),
                },
            );
        }
        app.play_order.swap(0, 3);

        app.queue_cursor = 2;
        app.play_queue_item();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test2"
        );
        assert_eq!(app.queue_cursor, 2);

        app.next_track();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test0"
        );
        assert_eq!(app.queue_cursor, 3);
    }

    #[test]
    fn lyric_scroll_follows_active_line_or_selected_cursor() {
        assert_eq!(lyric_scroll_offset(Some(15), Some(15), 20, true, 10), 10);
        assert_eq!(lyric_scroll_offset(Some(3), Some(3), 20, true, 10), 0);
        assert_eq!(lyric_scroll_offset(Some(15), Some(18), 20, false, 10), 10);
    }

    #[tokio::test]
    async fn lyric_keys_select_and_jump() {
        let mut app = test_app();
        app.tab = Tab::Lyrics;
        app.now_playing = Some(NowPlaying {
            track: demo_tracks()[0].clone(),
            position: Duration::from_secs(1),
            playing: false,
            volume: 60,
        });
        app.lyrics.insert(
            "spotify:track:test0".to_string(),
            crate::lyrics::Lyrics {
                lines: vec![
                    crate::lyrics::LyricLine {
                        time: Duration::from_secs(0),
                        text: "intro".into(),
                    },
                    crate::lyrics::LyricLine {
                        time: Duration::from_secs(30),
                        text: "verse".into(),
                    },
                    crate::lyrics::LyricLine {
                        time: Duration::from_secs(60),
                        text: "chorus".into(),
                    },
                ],
                synced: true,
                source: "test".into(),
            },
        );
        app.sync_lyric_cursor();
        assert_eq!(app.lyric_cursor, 0);

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.lyric_cursor, 1);
        assert!(!app.lyrics_auto_scroll);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().position,
            Duration::from_secs(30)
        );
        assert!(app.lyrics_auto_scroll);
    }

    #[tokio::test]
    async fn jump_to_lyric_seeks_to_selected_line() {
        let mut app = test_app();
        app.now_playing = Some(NowPlaying {
            track: demo_tracks()[0].clone(),
            position: Duration::from_secs(5),
            playing: true,
            volume: 60,
        });
        app.lyrics.insert(
            "spotify:track:test0".to_string(),
            crate::lyrics::Lyrics {
                lines: vec![
                    crate::lyrics::LyricLine {
                        time: Duration::from_secs(2),
                        text: "first".into(),
                    },
                    crate::lyrics::LyricLine {
                        time: Duration::from_secs(37),
                        text: "chorus".into(),
                    },
                ],
                synced: true,
                source: "test".into(),
            },
        );

        app.lyric_cursor = 1;
        app.lyrics_auto_scroll = false;
        app.jump_to_lyric();
        assert_eq!(
            app.now_playing.as_ref().unwrap().position,
            Duration::from_secs(37)
        );
        assert!(app.lyrics_auto_scroll);
        assert!(app.status.contains("Jumped to 00:37"));
    }

    #[test]
    fn mpris_state_snapshot_matches_now_playing() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        app.now_playing = Some(NowPlaying {
            track: app.tracks[1].clone(),
            position: Duration::from_secs(12),
            playing: true,
            volume: 40,
        });
        app.shuffle = true;

        let state = app.mpris_state();
        let track = state.track.unwrap();
        assert_eq!(track.uri, "spotify:track:test1");
        assert_eq!(track.title, "Track 1");
        assert_eq!(track.artist, "Artist");
        assert_eq!(track.album, "Album");
        assert_eq!(track.duration, Duration::from_secs(120));
        assert_eq!(state.position, Duration::from_secs(12));
        assert!(state.playing);
        assert_eq!(state.volume, 0.4);
        assert!(state.shuffle);
    }

    #[tokio::test]
    async fn mpris_controls_drive_playback() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        for track in &app.tracks {
            app.lyrics.insert(
                track.uri.clone(),
                crate::lyrics::Lyrics {
                    lines: Vec::new(),
                    synced: true,
                    source: "test".into(),
                },
            );
        }
        app.now_playing = Some(NowPlaying {
            track: app.tracks[0].clone(),
            position: Duration::from_secs(10),
            playing: false,
            volume: 50,
        });

        app.handle_action(Action::MprisPlay).unwrap();
        assert!(app.now_playing.as_ref().unwrap().playing);
        app.handle_action(Action::MprisPlay).unwrap(); // idempotent
        assert!(app.now_playing.as_ref().unwrap().playing);
        app.handle_action(Action::MprisPlayPause).unwrap();
        assert!(!app.now_playing.as_ref().unwrap().playing);
        app.handle_action(Action::MprisPlayPause).unwrap();
        assert!(app.now_playing.as_ref().unwrap().playing);

        app.handle_action(Action::MprisSeek(-4_000)).unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().position,
            Duration::from_secs(6)
        );
        app.handle_action(Action::MprisSeek(2_000)).unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().position,
            Duration::from_secs(8)
        );

        // A stale SetPosition from another track must be ignored.
        app.handle_action(Action::MprisSetPosition {
            track: crate::mpris::track_id("spotify:track:other"),
            position_ms: 99_000,
        })
        .unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().position,
            Duration::from_secs(8)
        );
        app.handle_action(Action::MprisSetPosition {
            track: crate::mpris::track_id("spotify:track:test0"),
            position_ms: 30_000,
        })
        .unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().position,
            Duration::from_secs(30)
        );

        app.handle_action(Action::MprisSetVolume(0.25)).unwrap();
        assert_eq!(app.now_playing.as_ref().unwrap().volume, 25);
        app.handle_action(Action::MprisSetVolume(2.0)).unwrap();
        assert_eq!(app.now_playing.as_ref().unwrap().volume, 100);

        app.handle_action(Action::MprisSetShuffle(true)).unwrap();
        assert!(app.shuffle);
        app.handle_action(Action::MprisSetShuffle(false)).unwrap();
        assert!(!app.shuffle);

        app.handle_action(Action::MprisNext).unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test1"
        );
        app.handle_action(Action::MprisPrev).unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test0"
        );

        app.handle_action(Action::MprisOpenUri("spotify:track:test3".into()))
            .unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test3"
        );
        app.handle_action(Action::MprisOpenUri("spotify:track:missing".into()))
            .unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test3"
        );
        assert!(app.status.contains("Unknown track"));

        app.handle_action(Action::MprisQuit).unwrap();
        assert!(!app.running);
    }

    #[test]
    fn mpris_state_includes_cover_art_url() {
        let mut app = test_app();
        let mut track = demo_tracks()[0].clone();
        track.art_url = Some("https://example.com/cover.jpg".to_string());
        app.now_playing = Some(NowPlaying {
            track,
            position: Duration::ZERO,
            playing: false,
            volume: 60,
        });

        let state = app.mpris_state();
        assert_eq!(
            state.track.unwrap().art_url.as_deref(),
            Some("https://example.com/cover.jpg")
        );
    }

    #[test]
    fn cover_art_toggle_disables_and_restores() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        app.now_playing = Some(NowPlaying {
            track: app.tracks[0].clone(),
            position: Duration::ZERO,
            playing: false,
            volume: 60,
        });

        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        app.handle_key(c.clone()).unwrap();
        assert!(!app.art_enabled);
        assert_eq!(app.art.status(), ArtStatus::Disabled);
        assert!(app.status.contains("hidden"));

        app.handle_key(c).unwrap();
        assert!(app.art_enabled);
        assert_eq!(app.art.status(), ArtStatus::NoArt);
    }

    #[test]
    fn cover_art_failure_shows_placeholder_without_erroring() {
        let mut app = test_app();
        let mut track = demo_tracks()[0].clone();
        track.art_url = Some("https://example.com/cover.jpg".to_string());
        app.now_playing = Some(NowPlaying {
            track,
            position: Duration::ZERO,
            playing: false,
            volume: 60,
        });

        app.handle_action(Action::CoverArtLoaded {
            target: CoverArtTarget::Track {
                uri: demo_tracks()[0].uri.clone(),
            },
            url: "https://example.com/cover.jpg".to_string(),
            result: Result::<image::DynamicImage, String>::Err("offline".to_string()),
        })
        .unwrap();

        assert_eq!(app.art.status(), ArtStatus::Failed);
        assert!(app.art.placeholder_lines()[1].contains("offline"));
        assert!(app.error.is_none());
    }

    #[test]
    fn playlist_name_prompt_edits_and_cancels() {
        let mut app = test_app();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        app.handle_key(key(KeyCode::Char('N'))).unwrap();
        assert!(matches!(app.prompt, Some(PromptKind::NewPlaylist)));
        for c in "Mixtape".chars() {
            app.handle_key(key(KeyCode::Char(c))).unwrap();
        }
        app.handle_key(key(KeyCode::Backspace)).unwrap();
        assert_eq!(app.prompt_input, "Mixtap");

        // No API client in tests: submitting reports that sign-in is required
        // instead of issuing a request.
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert!(app.prompt.is_none());
        assert!(app.prompt_input.is_empty());
        assert!(app.status.contains("Sign in"));

        app.tab = Tab::Playlists;
        app.playlists = vec![demo_playlist("id-1", "Road Trip")];
        app.list_state.select(Some(0));
        app.handle_key(key(KeyCode::Char('R'))).unwrap();
        assert!(matches!(
            app.prompt,
            Some(PromptKind::RenamePlaylist { ref id }) if id == "id-1"
        ));
        assert_eq!(app.prompt_input, "Road Trip");

        app.handle_key(key(KeyCode::Esc)).unwrap();
        assert!(app.prompt.is_none());
        assert!(app.prompt_input.is_empty());
    }

    #[test]
    fn playlist_edits_stage_locally_and_discard_restores() {
        let mut app = test_app();
        let original = demo_tracks();
        app.playlists = vec![demo_playlist("id-1", "Road Trip")];
        app.current_playlist_id = Some("id-1".into());
        app.set_tracks(original.clone());
        app.ensure_playlist_draft();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        app.handle_key(key(KeyCode::Char('J'))).unwrap();
        assert_eq!(app.tracks[0].uri, "spotify:track:test1");
        assert_eq!(app.list_state.selected(), Some(1));
        assert!(app.playlist_is_dirty());

        app.handle_key(key(KeyCode::Char('X'))).unwrap();
        assert_eq!(app.tracks.len(), original.len() - 1);

        app.tab = Tab::Playlists;
        app.handle_key(key(KeyCode::Char('R'))).unwrap();
        app.prompt_input.clear();
        for c in "Mix".chars() {
            app.handle_key(key(KeyCode::Char(c))).unwrap();
        }
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(app.active_playlist_draft().unwrap().name, "Mix");
        assert_eq!(app.playlists[0].name, "Mix");

        // No client in tests, so save is refused without losing the draft.
        app.handle_key(key(KeyCode::Char('S'))).unwrap();
        assert!(app.status.contains("Sign in"));
        assert!(app.playlist_is_dirty());

        app.handle_key(key(KeyCode::Char('D'))).unwrap();
        assert!(!app.playlist_is_dirty());
        assert!(app.playlist_drafts.is_empty());
        assert_eq!(app.playlists[0].name, "Road Trip");
        assert_eq!(
            app.tracks
                .iter()
                .map(|t| t.uri.as_str())
                .collect::<Vec<_>>(),
            original.iter().map(|t| t.uri.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn add_to_playlist_picker_navigates_and_cancels() {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        app.playlists = vec![
            demo_playlist("id-1", "Road Trip"),
            demo_playlist("id-2", "Workout"),
        ];
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        app.handle_key(key(KeyCode::Char('A'))).unwrap();
        let picker = app.playlist_picker.as_ref().unwrap();
        assert_eq!(picker.cursor, 0);
        assert_eq!(picker.track.uri, "spotify:track:test0");

        app.handle_key(key(KeyCode::Down)).unwrap();
        assert_eq!(app.playlist_picker.as_ref().unwrap().cursor, 1);
        app.handle_key(key(KeyCode::Up)).unwrap();
        assert_eq!(app.playlist_picker.as_ref().unwrap().cursor, 0);
        app.handle_key(key(KeyCode::Char('G'))).unwrap();
        assert_eq!(app.playlist_picker.as_ref().unwrap().cursor, 1);
        app.handle_key(key(KeyCode::Esc)).unwrap();
        assert!(app.playlist_picker.is_none());
        assert!(app.status.contains("cancelled"));
    }

    #[test]
    fn blocked_terminal_cannot_enable_art() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut app = App::new(tx, Picker::halfblocks(), Some("TERM=dumb".to_string()));
        assert!(!app.art_enabled);

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .unwrap();
        assert!(!app.art_enabled);
        assert!(app.status.contains("TERM=dumb"));
    }

    #[test]
    fn repeat_key_cycles_off_all_one() {
        let mut app = test_app();
        let r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(app.repeat, RepeatMode::Off);

        app.handle_key(r.clone()).unwrap();
        assert_eq!(app.repeat, RepeatMode::All);
        assert_eq!(app.status, "Repeat all");
        app.handle_key(r.clone()).unwrap();
        assert_eq!(app.repeat, RepeatMode::One);
        app.handle_key(r).unwrap();
        assert_eq!(app.repeat, RepeatMode::Off);

        app.handle_action(Action::MprisSetRepeat(RepeatMode::One))
            .unwrap();
        assert_eq!(app.repeat, RepeatMode::One);
        assert_eq!(app.mpris_state().repeat, RepeatMode::One);
    }

    fn playing_app_at(index: usize) -> App {
        let mut app = test_app();
        app.set_tracks(demo_tracks());
        app.list_state.select(Some(index));
        app.now_playing = Some(NowPlaying {
            track: app.tracks[index].clone(),
            position: Duration::from_secs(10),
            playing: true,
            volume: 60,
        });
        app
    }

    #[tokio::test]
    async fn track_end_honours_repeat_mode() {
        let mut app = playing_app_at(4);
        app.handle_action(Action::PlayerEndOfTrack).unwrap();
        assert!(!app.now_playing.as_ref().unwrap().playing);
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test4"
        );
        assert!(app.status.contains("End of queue"));

        let mut app = playing_app_at(4);
        app.set_repeat(RepeatMode::All);
        app.handle_action(Action::PlayerEndOfTrack).unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test0"
        );
        assert!(app.now_playing.as_ref().unwrap().playing);

        let mut app = playing_app_at(2);
        app.set_repeat(RepeatMode::One);
        app.handle_action(Action::PlayerEndOfTrack).unwrap();
        assert_eq!(
            app.now_playing.as_ref().unwrap().track.uri,
            "spotify:track:test2"
        );
        assert_eq!(app.now_playing.as_ref().unwrap().position, Duration::ZERO);
        assert!(app.now_playing.as_ref().unwrap().playing);
        assert!(app.status.contains("Repeating"));
    }

    #[test]
    fn cover_art_rebuilds_after_terminal_resize() {
        let mut app = test_app();
        app.set_terminal_size(100, 30);
        let mut track = demo_tracks()[0].clone();
        track.art_url = Some("https://example.com/cover.jpg".to_string());
        app.now_playing = Some(NowPlaying {
            track,
            position: Duration::ZERO,
            playing: false,
            volume: 60,
        });
        let image: image::DynamicImage = image::RgbImage::new(256, 256).into();
        app.handle_action(Action::CoverArtLoaded {
            target: CoverArtTarget::Track {
                uri: demo_tracks()[0].uri.clone(),
            },
            url: "https://example.com/cover.jpg".to_string(),
            result: Ok(image),
        })
        .unwrap();
        assert_eq!(
            app.art.protocol().unwrap().size(),
            art::cover_size(Rect::new(0, 0, 100, 30))
        );

        app.handle_action(Action::Resize(120, 40)).unwrap();
        assert_eq!(app.art.protocol().unwrap().size(), Size::new(44, 22));
    }

    #[tokio::test]
    async fn cover_art_defers_when_terminal_is_too_small() {
        let mut app = test_app();
        app.set_terminal_size(40, 10);
        let mut track = demo_tracks()[0].clone();
        track.art_url = Some("https://example.com/cover.jpg".to_string());
        app.now_playing = Some(NowPlaying {
            track,
            position: Duration::ZERO,
            playing: false,
            volume: 60,
        });
        let image: image::DynamicImage = image::RgbImage::new(256, 256).into();
        app.handle_action(Action::CoverArtLoaded {
            target: CoverArtTarget::Track {
                uri: demo_tracks()[0].uri.clone(),
            },
            url: "https://example.com/cover.jpg".to_string(),
            result: Ok(image),
        })
        .unwrap();
        assert!(matches!(app.art, CoverArt::Deferred { .. }));

        app.handle_action(Action::Resize(120, 40)).unwrap();
        assert_eq!(app.art.protocol().unwrap().size(), Size::new(44, 22));
    }

    #[test]
    fn lyrics_cache_restores_hits_and_known_misses() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "termify-app-lyrics-{}-{nanos}.json",
            std::process::id()
        ));
        let mut entries = HashMap::new();
        entries.insert(
            "spotify:track:test0".to_string(),
            Some(crate::lyrics::Lyrics {
                lines: vec![crate::lyrics::LyricLine {
                    time: Duration::from_secs(3),
                    text: "cached".to_string(),
                }],
                synced: true,
                source: "LRCLIB".to_string(),
            }),
        );
        entries.insert("spotify:track:test1".to_string(), None);
        crate::lyrics::save_cache(&path, &entries).expect("seed cache");

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut app = App::new_with_paths(tx, Picker::halfblocks(), None, Some(path.clone()));
        assert!(app.lyrics.contains_key("spotify:track:test0"));
        assert!(app.no_lyrics.contains("spotify:track:test1"));

        let track = demo_tracks()[1].clone();
        app.fetch_lyrics(&track);
        assert!(app.status.contains("cached"));
        assert!(app.lyrics_loading == false);
        let _ = std::fs::remove_file(&path);
    }

    fn offline_test_app(name: &str) -> (App, PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "termify-app-offline-{name}-{}-{nanos}",
            std::process::id()
        ));
        let store = DownloadStore::open(&dir).expect("open download store");
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let app = App::new_with_options(
            tx,
            Picker::halfblocks(),
            None,
            AppOptions {
                lyrics_cache_file: None,
                downloads: Some(store),
            },
        );
        (app, dir)
    }

    fn stored_entry(track: &Track, file: &str) -> DownloadEntry {
        DownloadEntry {
            track: track.clone(),
            file: file.into(),
            size: 12,
            downloaded_at: 1,
        }
    }

    #[test]
    fn download_finished_indexes_track_and_offline_tab_lists_it() {
        let (mut app, dir) = offline_test_app("finished");
        let track = demo_tracks()[0].clone();
        let entry = stored_entry(&track, "0123456789abcdef.ogg");
        std::fs::write(dir.join(&entry.file), b"audio").expect("write audio");

        app.handle_action(Action::DownloadFinished {
            uri: track.uri.clone(),
            result: Ok(entry),
        })
        .unwrap();
        assert!(app.downloads.contains(&track.uri));
        assert!(app.status.contains("Downloaded"));

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.tab, Tab::Downloads);
        assert_eq!(app.tracks.len(), 1);
        assert_eq!(app.tracks[0].uri, track.uri);
        assert!(app.status.contains("Offline library"));

        app.set_offline_mode();
        assert_eq!(app.playback_mode, PlaybackMode::Offline);
        assert!(app.status.contains("Offline mode"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_selected_without_player_reports_sign_in() {
        let (mut app, dir) = offline_test_app("no-player");
        app.set_tracks(demo_tracks());
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.status.contains("Sign in"));
        assert!(app.downloading.is_empty());
        assert!(app.downloads.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_error_clears_in_progress_and_reports_it() {
        let (mut app, dir) = offline_test_app("error");
        let uri = "spotify:track:test0".to_string();
        app.downloading.insert(uri.clone());
        app.handle_action(Action::DownloadFinished {
            uri: uri.clone(),
            result: Err("network down".to_string()),
        })
        .unwrap();
        assert!(!app.downloading.contains(&uri));
        assert!(
            app.error
                .as_deref()
                .is_some_and(|e| e.contains("network down"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_delete_key_removes_only_selected_copy() {
        let (mut app, dir) = offline_test_app("delete");
        let first = demo_tracks()[0].clone();
        let second = demo_tracks()[1].clone();
        let first_file = "0123456789abcdef.ogg";
        let second_file = "fedcba9876543210.mp3";
        for (track, file) in [(&first, first_file), (&second, second_file)] {
            let entry = stored_entry(track, file);
            std::fs::write(dir.join(file), b"audio").expect("write audio");
            app.handle_action(Action::DownloadFinished {
                uri: track.uri.clone(),
                result: Ok(entry),
            })
            .unwrap();
        }
        app.load_downloaded_tracks();
        let row = app
            .offline_items
            .iter()
            .position(|item| matches!(item, OfflineItem::Track { uri } if uri == &first.uri))
            .expect("first track row");
        app.list_state.select(Some(row));

        app.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE))
            .unwrap();
        assert!(!app.downloads.contains(&first.uri));
        assert!(app.downloads.contains(&second.uri));
        assert!(!dir.join(first_file).exists());
        assert!(dir.join(second_file).exists());
        assert!(app.status.contains("Removed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlist_tab_navigation_uses_playlists_even_with_no_tracks_loaded() {
        let mut app = test_app();
        app.tab = Tab::Playlists;
        app.playlists = vec![
            demo_playlist("id-1", "Road Trip"),
            demo_playlist("id-2", "Workout"),
            demo_playlist("id-3", "Chill"),
        ];
        app.list_state.select(Some(0));
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        app.handle_key(key(KeyCode::Down)).unwrap();
        assert_eq!(app.list_state.selected(), Some(1));
        app.handle_key(key(KeyCode::Char('j'))).unwrap();
        assert_eq!(app.list_state.selected(), Some(2));
        app.handle_key(key(KeyCode::Down)).unwrap();
        assert_eq!(app.list_state.selected(), Some(2));
        app.handle_key(key(KeyCode::Up)).unwrap();
        assert_eq!(app.list_state.selected(), Some(1));
        app.handle_key(key(KeyCode::Char('g'))).unwrap();
        assert_eq!(app.list_state.selected(), Some(0));
        app.handle_key(key(KeyCode::Char('G'))).unwrap();
        assert_eq!(app.list_state.selected(), Some(2));
    }

    #[tokio::test]
    async fn playlist_navigation_loads_the_selected_playlist_cover() {
        let mut app = test_app();
        app.art_enabled = true;
        app.tab = Tab::Playlists;
        let first = demo_playlist("id-1", "Road Trip");
        let mut second = demo_playlist("id-2", "Workout");
        second.art_url = Some("https://example.com/workout.jpg".to_string());
        app.playlists = vec![first, second];
        app.list_state.select(Some(0));

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.art.url(), Some("https://example.com/workout.jpg"));
    }

    #[test]
    fn download_key_on_playlist_tab_requires_sign_in() {
        let (mut app, dir) = offline_test_app("playlist-no-player");
        app.tab = Tab::Playlists;
        app.playlists = vec![demo_playlist("id-1", "Road Trip")];
        app.list_state.select(Some(0));

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.status.contains("Sign in"));
        assert!(app.playlist_downloads.is_empty());
        assert!(app.downloading.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlist_download_actions_track_progress_and_summarise_failures() {
        let (mut app, dir) = offline_test_app("playlist-progress");
        let tracks = demo_tracks();
        let id = "playlist-1".to_string();
        let uris: Vec<String> = tracks.iter().take(3).map(|t| t.uri.clone()).collect();
        app.handle_action(Action::PlaylistDownloadStarted {
            id: id.clone(),
            name: "Road Trip".to_string(),
            total: 3,
            uris: uris.clone(),
            members: uris.clone(),
        })
        .unwrap();
        assert!(uris.iter().all(|uri| app.downloading.contains(uri)));

        for (track, file) in tracks
            .iter()
            .take(2)
            .zip(["0000000000000001.ogg", "0000000000000002.ogg"])
        {
            let entry = stored_entry(track, file);
            std::fs::write(dir.join(file), b"audio").expect("write audio");
            app.handle_action(Action::DownloadFinished {
                uri: track.uri.clone(),
                result: Ok(entry),
            })
            .unwrap();
        }
        app.handle_action(Action::DownloadFinished {
            uri: uris[2].clone(),
            result: Err("key denied".to_string()),
        })
        .unwrap();
        assert!(app.downloads.contains(&uris[0]));
        assert!(app.downloads.contains(&uris[1]));
        assert!(!app.downloads.contains(&uris[2]));
        assert!(app.downloading.is_empty());
        assert!(app.status.contains("Finished playlist 'Road Trip'"));
        assert!(app.status.contains("2 saved, 1 failed"));

        app.handle_action(Action::PlaylistDownloadFinished {
            id,
            name: "Road Trip".to_string(),
            total: 3,
            failed: 1,
            members: uris.clone(),
            error: None,
        })
        .unwrap();
        assert!(app.playlist_downloads.is_empty());
        assert!(app.status.contains("Downloaded 2/3 tracks"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offline_playlist_row_loads_saved_tracks_as_a_playable_queue() {
        let (mut app, dir) = offline_test_app("playlist-playback");
        let tracks = demo_tracks();
        let uris: Vec<String> = tracks.iter().take(3).map(|track| track.uri.clone()).collect();
        for (index, track) in tracks.iter().take(3).enumerate() {
            let file = format!("000000000000000{index}.ogg");
            std::fs::write(dir.join(&file), b"OggS").expect("write audio");
            app.handle_action(Action::DownloadFinished {
                uri: track.uri.clone(),
                result: Ok(stored_entry(track, &file)),
            })
            .unwrap();
        }
        app.downloads
            .insert_playlist("offline-1", "Road Trip", &uris)
            .unwrap();
        app.load_downloaded_tracks();
        let row = app
            .offline_items
            .iter()
            .position(|item| matches!(item, OfflineItem::Playlist { id, .. } if id == "offline-1"))
            .expect("playlist row");
        app.list_state.select(Some(row));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.tracks.len(), 3);
        assert_eq!(app.tracks[0].uri, uris[0]);
        assert_eq!(
            app.offline_queue,
            Some(OfflineQueue::Playlist {
                id: "offline-1".to_string(),
                name: "Road Trip".to_string(),
            })
        );
        assert_eq!(
            app.now_playing.as_ref().map(|np| np.track.uri.clone()),
            Some(uris[0].clone())
        );
        assert!(app.status.contains("Playing offline playlist 'Road Trip'"));

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.now_playing.as_ref().map(|np| np.track.uri.clone()),
            Some(uris[1].clone())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn repeat_all_restarts_shuffled_playlist_from_first_track() {
        let mut app = test_app();
        app.playlists = vec![demo_playlist("id-1", "Road Trip")];
        app.current_playlist_id = Some("id-1".into());
        app.set_tracks(demo_tracks());
        app.shuffle = true;
        app.build_play_order();
        app.set_repeat(RepeatMode::All);

        let first = app.play_order[0];
        let first_uri = app.tracks[first].uri.clone();
        let last = *app.play_order.last().expect("play order");
        app.list_state.select(Some(last));
        app.now_playing = Some(NowPlaying {
            track: app.tracks[last].clone(),
            position: Duration::from_secs(10),
            playing: true,
            volume: 60,
        });

        app.handle_action(Action::PlayerEndOfTrack).unwrap();
        assert_eq!(app.list_state.selected(), Some(first));
        assert_eq!(app.now_playing.as_ref().unwrap().track.uri, first_uri);
        assert!(app.now_playing.as_ref().unwrap().playing);
        assert!(app.status.contains("Repeating playlist 'Road Trip'"));
    }
}
