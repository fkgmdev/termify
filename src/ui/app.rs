use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::Sender;

use crate::art::{self, ArtStatus, CoverArt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    Playlists,
    Queue,
    Lyrics,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Search, Tab::Playlists, Tab::Queue, Tab::Lyrics];

    pub fn next(self) -> Self {
        match self {
            Tab::Search => Tab::Playlists,
            Tab::Playlists => Tab::Queue,
            Tab::Queue => Tab::Lyrics,
            Tab::Lyrics => Tab::Search,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Tab::Search => Tab::Lyrics,
            Tab::Playlists => Tab::Search,
            Tab::Queue => Tab::Playlists,
            Tab::Lyrics => Tab::Queue,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Search => "Search",
            Tab::Playlists => "Playlists",
            Tab::Queue => "Queue",
            Tab::Lyrics => "Lyrics",
        }
    }
}

#[derive(Debug, Clone)]
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
        format!("{} — {}  [{:02}:{:02}]", self.title, self.artist, mins, secs)
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
    /// Needed to make reorder requests against the right playlist version.
    pub snapshot_id: String,
}

impl Playlist {
    pub fn display(&self) -> String {
        let mut details = Vec::new();
        if let Some(count) = self.track_count {
            details.push(format!("{count} tracks"));
        }
        if self.collaborative {
            details.push("collaborative".to_string());
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

/// Playlist selection overlay shown when adding a track.
#[derive(Debug, Clone)]
pub struct PlaylistPicker {
    pub cursor: usize,
    pub track_uri: String,
    pub track_title: String,
}

#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub track: Track,
    pub position: Duration,
    pub playing: bool,
    pub volume: u8,
}

#[derive(Debug)]
pub enum Action {
    Tick,
    Render,
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
    LikedSongsLoaded(Result<Vec<Track>, String>),
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
    /// Cover art downloaded and decoded off the UI thread. `url` keeps stale
    /// responses from replacing the image for a different track.
    CoverArtLoaded {
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
    MprisSetPosition { track: String, position_ms: u64 },
    /// Volume in the MPRIS range 0.0..=1.0.
    MprisSetVolume(f64),
    MprisSetShuffle(bool),
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
    pub now_playing: Option<NowPlaying>,
    pub status: String,
    pub loading: bool,
    pub error: Option<String>,
    pub api_client: Option<std::sync::Arc<rspotify::AuthCodePkceSpotify>>,
    pub player: Option<std::sync::Arc<crate::player::LocalPlayer>>,
    pub lyrics: std::collections::HashMap<String, crate::lyrics::Lyrics>,
    pub lyrics_loading: bool,
    /// Selected lyric line on the Lyrics tab; Enter seeks to its timestamp.
    pub lyric_cursor: usize,
    pub lyrics_auto_scroll: bool,
    pub translations: std::collections::HashMap<String, Vec<String>>,
    pub translate_lyrics: bool,
    pub translation_loading: bool,
    pub shuffle: bool,
    /// Playback order as indices into `tracks`; identity when shuffle is off.
    pub play_order: Vec<usize>,
    /// Selected row on the Queue tab, as a position in `play_order`.
    pub queue_cursor: usize,
    /// Current cover-art state (image protocol or text placeholder).
    pub art: CoverArt,
    /// User-facing art toggle. Forced off when the terminal cannot show images.
    pub art_enabled: bool,
    art_blocked_reason: Option<String>,
    picker: Picker,
    http_client: reqwest::Client,
    tx: Sender<Action>,
}

impl App {
    pub fn new(tx: Sender<Action>, picker: Picker, art_blocked_reason: Option<String>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let art = match &art_blocked_reason {
            Some(reason) => CoverArt::disabled(reason.clone()),
            None => CoverArt::no_art(),
        };
        let art_enabled = art_blocked_reason.is_none();
        Self {
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
            now_playing: None,
            status: "Ready — press / to search, ? for help".to_string(),
            loading: false,
            error: None,
            api_client: None,
            player: None,
            lyrics: std::collections::HashMap::new(),
            lyrics_loading: false,
            lyric_cursor: 0,
            lyrics_auto_scroll: true,
            translations: std::collections::HashMap::new(),
            translate_lyrics: false,
            translation_loading: false,
            shuffle: false,
            play_order: Vec::new(),
            queue_cursor: 0,
            art,
            art_enabled,
            art_blocked_reason,
            picker,
            http_client: reqwest::Client::new(),
            tx,
        }
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
        }
    }

    pub fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Tick => {
                if let Some(np) = &mut self.now_playing {
                    if np.playing {
                        np.position += Duration::from_millis(250);
                        if let Some(track) = self.tracks.iter().find(|t| t.uri == np.track.uri) {
                            if np.position >= track.duration {
                                self.next_track();
                            }
                        }
                    }
                }
                self.sync_lyric_cursor();
            }
            Action::Render => {}
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
                        self.playlists = playlists;
                        self.status = format!("Loaded {} playlists", self.playlists.len());
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::PlaylistDetailsLoaded(result) => {
                self.loading = false;
                match result {
                    Ok((playlist, tracks)) => {
                        self.current_playlist_id = Some(playlist.id.clone());
                        if let Some(existing) = self
                            .playlists
                            .iter_mut()
                            .find(|item| item.id == playlist.id)
                        {
                            *existing = playlist;
                        } else {
                            self.playlists.push(playlist);
                        }
                        self.set_tracks(tracks);
                        self.tab = Tab::Search;
                        self.status = format!("Loaded {} playlist tracks", self.tracks.len());
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
            Action::PlayerEndOfTrack => {
                self.next_track();
            }
            Action::PlayerPlaying(position_ms) => {
                if let Some(np) = &mut self.now_playing {
                    np.playing = true;
                    np.position = Duration::from_millis(u64::from(position_ms));
                }
            }
            Action::PlayerPaused(position_ms) => {
                if let Some(np) = &mut self.now_playing {
                    np.playing = false;
                    np.position = Duration::from_millis(u64::from(position_ms));
                }
            }
            Action::LyricsLoaded { uri, result } => {
                self.lyrics_loading = false;
                match result {
                    Ok(Some(lyrics)) => {
                        self.lyrics.insert(uri.clone(), lyrics);
                        self.status = "Lyrics loaded".to_string();
                        self.sync_lyric_cursor();
                        if self.translate_lyrics {
                            self.fetch_translation(&uri);
                        }
                    }
                    Ok(None) => self.status = "No lyrics found for this track".to_string(),
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
            Action::CoverArtLoaded { url, result } => {
                if !self.art_enabled {
                    return Ok(());
                }
                let current = self
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.track.art_url.as_deref());
                if current != Some(url.as_str()) {
                    return Ok(()); // stale response for a previous track
                }
                self.art = match result {
                    Ok(image) => match art::build_protocol(&self.picker, image) {
                        Ok(protocol) => CoverArt::ready(url, protocol),
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
                    let last = self.current_lyrics().map_or(0, |l| l.lines.len().saturating_sub(1));
                    self.set_lyric_cursor(last);
                }
                Tab::Queue => self.queue_cursor = self.play_order.len().saturating_sub(1),
                _ if !self.tracks.is_empty() => self.move_to(self.tracks.len() - 1),
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
            KeyCode::Tab | KeyCode::BackTab => {
                // crossterm reports Shift+Tab as BackTab; some terminals instead
                // send Tab with the SHIFT modifier, so handle both.
                let backward = key.code == KeyCode::BackTab
                    || key.modifiers.contains(KeyModifiers::SHIFT);
                self.tab = if backward {
                    self.tab.prev()
                } else {
                    self.tab.next()
                };
                self.status = format!("Tab: {}", self.tab.title());
            }
            KeyCode::Char('/') => {
                self.search_active = true;
                self.search_query.clear();
                self.status = "Search: type query, Enter to submit, Esc to cancel".to_string();
            }
            KeyCode::Enter => match self.tab {
                Tab::Queue => self.play_queue_item(),
                Tab::Lyrics => self.jump_to_lyric(),
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
            KeyCode::Char('s') => self.toggle_shuffle(),
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
                self.add_track_to_playlist(playlist, picker.track_uri, picker.track_title);
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
            self.list_state.selected().and_then(|i| self.playlists.get(i))
        } else {
            None
        };
        let Some(playlist) = selected else {
            self.status = "Select a playlist on the Playlists tab first".to_string();
            return;
        };
        self.prompt = Some(PromptKind::RenamePlaylist {
            id: playlist.id.clone(),
        });
        self.prompt_input = playlist.name.clone();
        self.status = "Rename playlist: Enter to save, Esc to cancel".to_string();
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
        self.playlist_picker = Some(PlaylistPicker {
            cursor: 0,
            track_uri: track.uri,
            track_title: track.title,
        });
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

    fn add_track_to_playlist(&mut self, playlist: Playlist, track_uri: String, track_title: String) {
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to edit playlists".to_string();
            return;
        };
        self.loading = true;
        self.status = format!("Adding '{track_title}' to '{}'...", playlist.name);
        let tx = self.tx.clone();
        let playlist_name = playlist.name.clone();
        let playlist_id = playlist.id.clone();
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
        let Some(playlist_id) = self.current_playlist_id.clone() else {
            self.status = "Open a playlist first, then press X to remove a track".to_string();
            return;
        };
        let Some(track) = self
            .list_state
            .selected()
            .and_then(|i| self.tracks.get(i))
            .cloned()
        else {
            self.status = "No track selected".to_string();
            return;
        };
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to edit playlists".to_string();
            return;
        };
        self.loading = true;
        self.status = format!("Removing '{}' from playlist...", track.title);
        let tx = self.tx.clone();
        let title = track.title.clone();
        let reload_id = playlist_id.clone();
        tokio::spawn(async move {
            let result = crate::api::remove_playlist_track(&client, &playlist_id, &track.uri)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx
                .send(Action::PlaylistEdited {
                    message: format!("Removed '{title}' from playlist"),
                    result,
                    reload_playlists: false,
                    reload_playlist: Some(reload_id),
                })
                .await;
        });
    }

    fn move_playlist_track(&mut self, delta: i32) {
        let Some(playlist_id) = self.current_playlist_id.clone() else {
            self.status = "Open a playlist first, then press [ or ] to reorder".to_string();
            return;
        };
        let Some(from) = self.list_state.selected() else {
            self.status = "No track selected".to_string();
            return;
        };
        let to = (from as i32 + delta).clamp(0, self.tracks.len().saturating_sub(1) as i32) as usize;
        if from == to {
            return;
        }
        let Some(snapshot_id) = self
            .playlists
            .iter()
            .find(|p| p.id == playlist_id)
            .map(|p| p.snapshot_id.clone())
        else {
            self.status = "Playlist snapshot unavailable; reopen the playlist".to_string();
            return;
        };
        let Some(client) = self.api_client.clone() else {
            self.status = "Sign in to edit playlists".to_string();
            return;
        };
        self.loading = true;
        self.status = format!("Moving track to position {}...", to + 1);
        let tx = self.tx.clone();
        let reload_id = playlist_id.clone();
        tokio::spawn(async move {
            let result = crate::api::move_playlist_track(
                &client, &playlist_id, &snapshot_id, from, to,
            )
            .await
            .map_err(|err| format!("{err:#}"));
            let _ = tx
                .send(Action::PlaylistEdited {
                    message: format!("Moved track to position {}", to + 1),
                    result,
                    reload_playlists: false,
                    reload_playlist: Some(reload_id),
                })
                .await;
        });
    }

    fn move_selection(&mut self, delta: i32) {
        if self.tracks.is_empty() {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, self.tracks.len() as i32 - 1) as usize;
        self.list_state.select(Some(next));
    }

    fn move_to(&mut self, index: usize) {
        if !self.tracks.is_empty() {
            self.list_state.select(Some(index.min(self.tracks.len() - 1)));
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
        self.lyric_cursor = (self.lyric_cursor as i32 + delta)
            .clamp(0, line_count as i32 - 1) as usize;
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
        self.queue_cursor = (self.queue_cursor as i32 + delta)
            .clamp(0, self.play_order.len() as i32 - 1) as usize;
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
        self.list_state
            .select(if self.tracks.is_empty() { None } else { Some(0) });
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
            if let Some(track) = self.now_playing.as_ref().map(|np| np.track.clone()) {
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
        if !self.art_enabled {
            return;
        }
        let Some(url) = track.art_url.clone() else {
            self.art = CoverArt::no_art();
            return;
        };
        if self.art.url() == Some(url.as_str())
            && matches!(self.art.status(), ArtStatus::Ready | ArtStatus::Loading)
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
            let _ = tx.send(Action::CoverArtLoaded { url, result }).await;
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

        let track = index.and_then(|i| self.tracks.get(i)).cloned();
        let Some(track) = track else { return };

        let volume = self.now_playing.as_ref().map_or(60, |np| np.volume);
        self.now_playing = Some(NowPlaying {
            track: track.clone(),
            position: Duration::ZERO,
            playing: false,
            volume,
        });
        // A new track starts with lyrics following playback again.
        self.lyrics_auto_scroll = true;
        self.lyric_cursor = 0;
        self.sync_lyric_cursor();
        // Keep the queue cursor on whatever is now playing.
        self.queue_cursor = self.play_position();

        if let Some(player) = &self.player {
            match player.play_uri(&track.uri) {
                Ok(()) => self.status = format!("Playing: {}", track.title),
                Err(err) => self.set_error(format!("Could not play track: {err}")),
            }
        } else {
            self.status = format!("Selected: {} (demo mode)", track.title);
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
        let Some(np) = &mut self.now_playing else { return };
        if np.playing == playing {
            return;
        }
        np.playing = playing;
        if let Some(player) = &self.player {
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
        let selected = self.list_state.selected().unwrap_or(0);
        self.play_order
            .iter()
            .position(|&index| index == selected)
            .unwrap_or(0)
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
        self.list_state.select(Some(track_index));
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
        self.list_state.select(Some(track_index));
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
        let target = (current.saturating_add(delta_ms)).clamp(0, np.track.duration.as_millis() as i64);
        self.seek_to(target as u64);
    }

    /// Absolute seek to a position within the current track.
    fn seek_to(&mut self, position_ms: u64) {
        let Some(np) = &mut self.now_playing else { return };
        let target = Duration::from_millis(position_ms).min(np.track.duration);
        np.position = target;
        if let Some(player) = &self.player {
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
        let Some(np) = &mut self.now_playing else { return };
        let percent = percent.min(100);
        if np.volume == percent {
            return;
        }
        np.volume = percent;
        if let Some(player) = &self.player {
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
    let focus = if auto_scroll { active } else { cursor.or(active) };
    focus
        .unwrap_or(0)
        .saturating_sub(height / 2)
        .min(max_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(app.now_playing.as_ref().unwrap().position, Duration::from_secs(37));
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
        assert_eq!(picker.track_uri, "spotify:track:test0");

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
        let mut app = App::new(
            tx,
            Picker::halfblocks(),
            Some("TERM=dumb".to_string()),
        );
        assert!(!app.art_enabled);

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .unwrap();
        assert!(!app.art_enabled);
        assert!(app.status.contains("TERM=dumb"));
    }
}
