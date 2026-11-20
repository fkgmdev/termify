use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use tokio::sync::mpsc::Sender;

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
}

impl Playlist {
    pub fn display(&self) -> String {
        match self.track_count {
            Some(count) => format!("{}  ({count} tracks)", self.name),
            None => self.name.clone(),
        }
    }
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
    PlaylistTracksLoaded(Result<Vec<Track>, String>),
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
    pub now_playing: Option<NowPlaying>,
    pub status: String,
    pub loading: bool,
    pub error: Option<String>,
    pub api_client: Option<std::sync::Arc<rspotify::AuthCodePkceSpotify>>,
    pub player: Option<std::sync::Arc<crate::player::LocalPlayer>>,
    pub lyrics: std::collections::HashMap<String, crate::lyrics::Lyrics>,
    pub lyrics_loading: bool,
    pub lyrics_scroll: usize,
    pub lyrics_auto_scroll: bool,
    pub translations: std::collections::HashMap<String, Vec<String>>,
    pub translate_lyrics: bool,
    pub translation_loading: bool,
    pub shuffle: bool,
    /// Playback order as indices into `tracks`; identity when shuffle is off.
    pub play_order: Vec<usize>,
    /// Selected row on the Queue tab, as a position in `play_order`.
    pub queue_cursor: usize,
    tx: Sender<Action>,
}

impl App {
    pub fn new(tx: Sender<Action>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            running: true,
            tab: Tab::Search,
            show_help: false,
            search_query: String::new(),
            search_active: false,
            list_state,
            tracks: Vec::new(),
            playlists: Vec::new(),
            now_playing: None,
            status: "Ready — press / to search, ? for help".to_string(),
            loading: false,
            error: None,
            api_client: None,
            player: None,
            lyrics: std::collections::HashMap::new(),
            lyrics_loading: false,
            lyrics_scroll: 0,
            lyrics_auto_scroll: true,
            translations: std::collections::HashMap::new(),
            translate_lyrics: false,
            translation_loading: false,
            shuffle: false,
            play_order: Vec::new(),
            queue_cursor: 0,
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
            },
            Track {
                title: "Stairway to Heaven".into(),
                artist: "Led Zeppelin".into(),
                album: "Led Zeppelin IV".into(),
                duration: Duration::from_secs(482),
                uri: "spotify:track:demo2".into(),
            },
            Track {
                title: "Imagine".into(),
                artist: "John Lennon".into(),
                album: "Imagine".into(),
                duration: Duration::from_secs(183),
                uri: "spotify:track:demo3".into(),
            },
            Track {
                title: "Hotel California".into(),
                artist: "Eagles".into(),
                album: "Hotel California".into(),
                duration: Duration::from_secs(391),
                uri: "spotify:track:demo4".into(),
            },
            Track {
                title: "Sweet Child O' Mine".into(),
                artist: "Guns N' Roses".into(),
                album: "Appetite for Destruction".into(),
                duration: Duration::from_secs(356),
                uri: "spotify:track:demo5".into(),
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
            Action::PlaylistTracksLoaded(result) => {
                self.loading = false;
                match result {
                    Ok(tracks) => {
                        self.set_tracks(tracks);
                        self.tab = Tab::Search;
                        self.status = format!("Loaded {} playlist tracks", self.tracks.len());
                    }
                    Err(err) => self.set_error(err),
                }
            }
            Action::LikedSongsLoaded(result) => {
                self.loading = false;
                match result {
                    Ok(tracks) => {
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

        // Ctrl+C always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.running = false;
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') => self.running = false,
            KeyCode::Char('j') | KeyCode::Down => match self.tab {
                Tab::Lyrics => {
                    self.lyrics_auto_scroll = false;
                    self.lyrics_scroll = self.lyrics_scroll.saturating_add(1);
                }
                Tab::Queue => self.move_queue_cursor(1),
                _ => self.move_selection(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.tab {
                Tab::Lyrics => {
                    self.lyrics_auto_scroll = false;
                    self.lyrics_scroll = self.lyrics_scroll.saturating_sub(1);
                }
                Tab::Queue => self.move_queue_cursor(-1),
                _ => self.move_selection(-1),
            },
            KeyCode::Char('g') | KeyCode::Home => match self.tab {
                Tab::Queue => self.queue_cursor = 0,
                _ => self.move_to(0),
            },
            KeyCode::Char('G') | KeyCode::End => match self.tab {
                Tab::Queue => self.queue_cursor = self.play_order.len().saturating_sub(1),
                _ if !self.tracks.is_empty() => self.move_to(self.tracks.len() - 1),
                _ => {}
            },
            KeyCode::PageDown => match self.tab {
                Tab::Queue => self.move_queue_cursor(10),
                _ => self.move_selection(10),
            },
            KeyCode::PageUp => match self.tab {
                Tab::Queue => self.move_queue_cursor(-10),
                _ => self.move_selection(-10),
            },
            KeyCode::Char('J') => self.reorder_queue(1),
            KeyCode::Char('K') => self.reorder_queue(-1),
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
            KeyCode::Enter => {
                if self.tab == Tab::Queue {
                    self.play_queue_item();
                } else {
                    self.select_current();
                }
            }
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
            KeyCode::Char('a') => self.toggle_auto_scroll(),
            KeyCode::Char('t') => self.toggle_translation(),
            KeyCode::Char('L') => {
                self.status = "Loading liked songs...".to_string();
                self.loading = true;
                self.load_liked_songs();
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        Ok(())
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
        self.status = if self.lyrics_auto_scroll {
            "Lyrics follow playback".to_string()
        } else {
            "Lyrics scroll manually (j/k)".to_string()
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
        self.lyrics_scroll = 0;
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
    }

    fn toggle_play(&mut self) {
        let Some(np) = &mut self.now_playing else { return };
        np.playing = !np.playing;
        let playing = np.playing;
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

    fn seek(&mut self, seconds: i64) {
        let Some(np) = &mut self.now_playing else { return };
        let delta = if seconds < 0 {
            Duration::from_secs(seconds.unsigned_abs())
        } else {
            Duration::from_secs(seconds as u64)
        };
        np.position = if seconds < 0 {
            np.position.saturating_sub(delta)
        } else {
            np.position.saturating_add(delta).min(np.track.duration)
        };
        if let Some(player) = &self.player {
            player.seek(np.position.as_millis().min(u128::from(u32::MAX)) as u32);
        }
        self.status = format!("Seek to {:.0}s", np.position.as_secs_f64());
    }

    fn adjust_volume(&mut self, delta: i16) {
        let Some(np) = &mut self.now_playing else { return };
        np.volume = (np.volume as i16 + delta).clamp(0, 100) as u8;
        if let Some(player) = &self.player {
            player.set_volume(u16::from(np.volume));
        }
        self.status = format!("Volume: {}%", np.volume);
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
            let result = crate::api::playlist_tracks(&client, &playlist_id)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Action::PlaylistTracksLoaded(result)).await;
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
        self.lyrics_scroll = 0;
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
    line_count: usize,
    auto_scroll: bool,
    manual_scroll: usize,
    height: usize,
) -> usize {
    let max_offset = line_count.saturating_sub(height);
    if auto_scroll {
        active
            .unwrap_or(0)
            .saturating_sub(height / 2)
            .min(max_offset)
    } else {
        manual_scroll.min(max_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        App::new(tx)
    }

    fn demo_tracks() -> Vec<Track> {
        (0..5)
            .map(|i| Track {
                title: format!("Track {i}"),
                artist: "Artist".into(),
                album: "Album".into(),
                duration: Duration::from_secs(120),
                uri: format!("spotify:track:test{i}"),
            })
            .collect()
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
    fn lyric_scroll_follows_active_line_and_clamps_manual_offset() {
        assert_eq!(lyric_scroll_offset(Some(15), 20, true, 0, 10), 10);
        assert_eq!(lyric_scroll_offset(Some(3), 20, true, 0, 10), 0);
        assert_eq!(lyric_scroll_offset(Some(15), 20, false, 18, 10), 10);
    }
}
