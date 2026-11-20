use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use tokio::sync::mpsc::Sender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    Playlists,
    Lyrics,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Search, Tab::Playlists, Tab::Lyrics];

    pub fn next(self) -> Self {
        match self {
            Tab::Search => Tab::Playlists,
            Tab::Playlists => Tab::Lyrics,
            Tab::Lyrics => Tab::Search,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Tab::Search => Tab::Lyrics,
            Tab::Playlists => Tab::Search,
            Tab::Lyrics => Tab::Playlists,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Search => "Search",
            Tab::Playlists => "Playlists",
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
                        self.tracks = tracks;
                        self.list_state.select(if self.tracks.is_empty() { None } else { Some(0) });
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
                        self.tracks = tracks;
                        self.list_state.select(if self.tracks.is_empty() { None } else { Some(0) });
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
                        self.tracks = tracks;
                        self.list_state.select(if self.tracks.is_empty() { None } else { Some(0) });
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
                        self.lyrics.insert(uri, lyrics);
                        self.status = "Lyrics loaded".to_string();
                    }
                    Ok(None) => self.status = "No lyrics found for this track".to_string(),
                    Err(err) => self.status = format!("Lyrics unavailable: {err}"),
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
            KeyCode::Char('j') | KeyCode::Down => {
                if self.tab == Tab::Lyrics {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_add(1);
                } else {
                    self.move_selection(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.tab == Tab::Lyrics {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_sub(1);
                } else {
                    self.move_selection(-1);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => {
                if !self.tracks.is_empty() {
                    self.move_to(self.tracks.len() - 1);
                }
            }
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
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
            KeyCode::Enter => self.select_current(),
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

    fn next_track(&mut self) {
        if self.tracks.is_empty() {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0);
        let next = (current + 1) % self.tracks.len();
        self.list_state.select(Some(next));
        self.select_current();
    }

    fn prev_track(&mut self) {
        if self.tracks.is_empty() {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0);
        let prev = if current == 0 {
            self.tracks.len() - 1
        } else {
            current - 1
        };
        self.list_state.select(Some(prev));
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

    pub fn send(&self, action: Action) {
        let _ = self.tx.try_send(action);
    }
}

pub fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
}
