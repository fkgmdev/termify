//! Discord Rich Presence for the currently playing termify track.
//!
//! This uses Discord's local IPC API rather than the Game SDK, so it only needs
//! a Discord application ID and a running desktop Discord client. Presence is
//! best-effort: missing configuration, a closed Discord client, or IPC errors
//! simply disable the integration without affecting playback.

use std::env;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use discord_rich_presence::{
    DiscordIpc, DiscordIpcClient,
    activity::{Activity, ActivityType, Assets, Button, StatusDisplayType, Timestamps},
};
use tokio::sync::watch;

use crate::config::Config;
use crate::mpris::MprisState;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RETRY_INTERVAL: Duration = Duration::from_secs(10);
const DISCORD_FIELD_LIMIT: usize = 128;
const BUTTON_LABEL_LIMIT: usize = 32;
const BUTTON_URL_LIMIT: usize = 512;
const ASSET_URL_LIMIT: usize = 512;

/// Owner of the Discord presence thread. Dropping it closes the state channel
/// and waits for the thread to clear the activity, so Rich Presence never
/// outlives the TUI.
#[derive(Debug)]
pub struct DiscordHandle {
    tx: Option<watch::Sender<MprisState>>,
    thread: Option<JoinHandle<()>>,
}

impl DiscordHandle {
    /// Publish the same media snapshot used by MPRIS / SMTC.
    pub fn publish(&self, state: MprisState) {
        if let Some(tx) = &self.tx {
            // A closed receiver only means the presence thread already exited.
            let _ = tx.send(state);
        }
    }
}

impl Drop for DiscordHandle {
    fn drop(&mut self) {
        // Closing the channel tells the thread to clear the activity. Joining
        // keeps that cleanup from racing with process shutdown.
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start Discord Rich Presence when configured and available.
pub fn spawn(config: &Config) -> anyhow::Result<Option<DiscordHandle>> {
    if env::var_os("TERMIFY_NO_DISCORD").is_some() {
        tracing::info!("TERMIFY_NO_DISCORD is set; Discord Rich Presence is disabled");
        return Ok(None);
    }

    let Some(client_id) = resolve_client_id(config) else {
        tracing::info!(
            "Discord Rich Presence is disabled; set discord_client_id in config.toml or TERMIFY_DISCORD_CLIENT_ID to enable it"
        );
        return Ok(None);
    };

    let (tx, rx) = watch::channel(MprisState::empty());
    let thread = thread::Builder::new()
        .name("termify-discord".to_string())
        .spawn(move || run(client_id, rx))
        .context("spawning Discord Rich Presence thread")?;

    Ok(Some(DiscordHandle {
        tx: Some(tx),
        thread: Some(thread),
    }))
}

fn resolve_client_id(config: &Config) -> Option<String> {
    let mut candidates: Vec<String> = ["TERMIFY_DISCORD_CLIENT_ID", "DISCORD_CLIENT_ID"]
        .into_iter()
        .filter_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();

    let configured = config.discord_client_id.trim();
    if !configured.is_empty() {
        candidates.push(configured.to_string());
    }

    for client_id in candidates {
        if valid_client_id(&client_id) {
            return Some(client_id);
        }
    }

    tracing::warn!("Ignoring invalid Discord application ID; expected numeric ID");
    None
}

fn valid_client_id(id: &str) -> bool {
    (15..=25).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}

fn run(client_id: String, mut state_rx: watch::Receiver<MprisState>) {
    let mut client: Option<DiscordIpcClient> = None;
    let mut next_attempt = Instant::now();
    let mut warned_about_connection = false;

    loop {
        let changed = match state_rx.has_changed() {
            Ok(changed) => changed,
            Err(_) => {
                clear_and_close(&mut client);
                tracing::debug!("Discord Rich Presence thread stopped");
                return;
            }
        };

        if changed {
            let state = state_rx.borrow_and_update().clone();
            apply_state(
                &mut client,
                &client_id,
                &mut next_attempt,
                &mut warned_about_connection,
                &state,
            );
        }

        thread::sleep(POLL_INTERVAL);
    }
}

fn apply_state(
    client: &mut Option<DiscordIpcClient>,
    client_id: &str,
    next_attempt: &mut Instant,
    warned_about_connection: &mut bool,
    state: &MprisState,
) -> bool {
    if state.track.is_none() {
        let Some(active) = client.as_mut() else {
            return false;
        };
        if let Err(err) = active.clear_activity() {
            tracing::debug!("Could not clear Discord activity: {err}");
            close_client(client);
            *next_attempt = Instant::now() + RETRY_INTERVAL;
            return false;
        }
        return true;
    }

    if !ensure_connected(client, client_id, next_attempt, warned_about_connection) {
        return false;
    }

    let activity = build_activity(state, unix_millis());
    let Some(active) = client.as_mut() else {
        return false;
    };
    if let Err(err) = active.set_activity(activity) {
        tracing::warn!("Discord Rich Presence update failed: {err}");
        close_client(client);
        *next_attempt = Instant::now() + RETRY_INTERVAL;
        return false;
    }
    true
}

fn ensure_connected(
    client: &mut Option<DiscordIpcClient>,
    client_id: &str,
    next_attempt: &mut Instant,
    warned_about_connection: &mut bool,
) -> bool {
    if client.is_some() {
        return true;
    }
    if Instant::now() < *next_attempt {
        return false;
    }

    let mut candidate = DiscordIpcClient::new(client_id);
    match candidate.connect() {
        Ok(()) => {
            *client = Some(candidate);
            *warned_about_connection = false;
            true
        }
        Err(err) => {
            if !*warned_about_connection {
                tracing::warn!("Discord Rich Presence unavailable: {err}");
                *warned_about_connection = true;
            }
            *next_attempt = Instant::now() + RETRY_INTERVAL;
            false
        }
    }
}

fn close_client(client: &mut Option<DiscordIpcClient>) {
    if let Some(mut active) = client.take() {
        if let Err(err) = active.close() {
            tracing::debug!("Could not close Discord IPC connection: {err}");
        }
    }
}

fn clear_and_close(client: &mut Option<DiscordIpcClient>) {
    if let Some(active) = client.as_mut() {
        if let Err(err) = active.clear_activity() {
            tracing::debug!("Could not clear Discord activity: {err}");
        }
    }
    close_client(client);
}

/// Build the Rich Presence payload for one playback snapshot.
fn build_activity(state: &MprisState, now_ms: i64) -> Activity<'static> {
    let Some(track) = &state.track else {
        return Activity::new();
    };

    let title = non_empty(&track.title, "Unknown title");
    let artist = non_empty(&track.artist, "Unknown artist");
    let album = non_empty(&track.album, "");
    let track_url = spotify_track_url(&track.uri);

    let mut activity = Activity::new()
        .name("termify")
        .activity_type(ActivityType::Listening)
        .status_display_type(StatusDisplayType::Details)
        .details(truncate(&title, DISCORD_FIELD_LIMIT))
        .state(status_text(state.playing, &artist, &album));

    if let Some(url) = track_url.as_deref() {
        activity = activity
            .state_url(url.to_string())
            .details_url(url.to_string());
    }

    if state.playing && track.duration > Duration::ZERO {
        let duration_ms = millis_i64(track.duration);
        let position_ms = millis_i64(state.position).min(duration_ms);
        activity = activity.timestamps(
            Timestamps::new()
                .start(now_ms.saturating_sub(position_ms))
                .end(now_ms.saturating_add(duration_ms - position_ms)),
        );
    }

    let large_text = if album.is_empty() { &title } else { &album };
    let mut assets = Assets::new().large_text(truncate(large_text, DISCORD_FIELD_LIMIT));
    if let Some(art_url) = track
        .art_url
        .as_deref()
        .and_then(http_url)
        .map(|url| truncate(url, ASSET_URL_LIMIT))
    {
        assets = assets.large_image(art_url);
        if let Some(url) = track_url.as_deref() {
            assets = assets.large_url(url.to_string());
        }
    }
    activity = activity.assets(assets);

    if let Some(url) = track_url {
        activity = activity.buttons(vec![Button::new(
            truncate("Open in Spotify", BUTTON_LABEL_LIMIT),
            truncate(&url, BUTTON_URL_LIMIT),
        )]);
    }

    activity
}

fn status_text(playing: bool, artist: &str, album: &str) -> String {
    let mut text = String::new();
    if !playing {
        text.push_str("⏸ ");
    }
    text.push_str("by ");
    text.push_str(artist);
    if !album.is_empty() {
        text.push_str(" • ");
        text.push_str(album);
    }
    truncate(&text, DISCORD_FIELD_LIMIT)
}

fn non_empty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

fn spotify_track_url(uri: &str) -> Option<String> {
    let id = uri.strip_prefix("spotify:track:")?;
    (!id.is_empty()).then(|| format!("https://open.spotify.com/track/{id}"))
}

fn http_url(url: &str) -> Option<&str> {
    (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn millis_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(millis_i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::mpris::{MprisState, MprisTrack};
    use crate::ui::RepeatMode;

    fn track_state() -> MprisState {
        MprisState {
            track: Some(MprisTrack {
                uri: "spotify:track:abc123".to_string(),
                title: "Midnight City".to_string(),
                artist: "M83".to_string(),
                album: "Hurry Up, We're Dreaming".to_string(),
                art_url: Some("https://example.com/cover.jpg".to_string()),
                duration: Duration::from_secs(180),
            }),
            position: Duration::from_secs(30),
            playing: true,
            volume: 0.6,
            shuffle: false,
            repeat: RepeatMode::Off,
        }
    }

    fn activity_json(state: &MprisState, now_ms: i64) -> Value {
        serde_json::to_value(build_activity(state, now_ms)).expect("activity should serialize")
    }

    #[test]
    fn valid_client_id_requires_numeric_discord_snowflake() {
        assert!(valid_client_id("123456789012345678"));
        assert!(!valid_client_id(""));
        assert!(!valid_client_id("1234"));
        assert!(!valid_client_id("not-a-discord-id"));
    }

    #[test]
    fn playing_activity_has_listening_details_progress_and_art() {
        let value = activity_json(&track_state(), 1_700_000_000_000);

        assert_eq!(value["name"], "termify");
        assert_eq!(value["type"], 2);
        assert_eq!(value["status_display_type"], 2);
        assert_eq!(value["details"], "Midnight City");
        assert_eq!(value["state"], "by M83 • Hurry Up, We're Dreaming");
        assert_eq!(value["timestamps"]["start"], 1_699_999_970_000_i64);
        assert_eq!(value["timestamps"]["end"], 1_700_000_150_000_i64);
        assert_eq!(
            value["assets"]["large_image"],
            "https://example.com/cover.jpg"
        );
        assert_eq!(value["assets"]["large_text"], "Hurry Up, We're Dreaming");
        assert_eq!(
            value["assets"]["large_url"],
            "https://open.spotify.com/track/abc123"
        );
        assert_eq!(value["buttons"][0]["label"], "Open in Spotify");
        assert_eq!(
            value["buttons"][0]["url"],
            "https://open.spotify.com/track/abc123"
        );
    }

    #[test]
    fn paused_activity_has_no_progress_bar_and_is_marked_paused() {
        let mut state = track_state();
        state.playing = false;
        let value = activity_json(&state, 1_700_000_000_000);

        assert!(value.get("timestamps").is_none());
        assert!(
            value["state"]
                .as_str()
                .is_some_and(|text| text.starts_with("⏸ by M83"))
        );
    }

    #[test]
    fn local_or_missing_art_url_does_not_add_discord_image() {
        let mut state = track_state();
        state.track.as_mut().unwrap().art_url = Some("/tmp/cover.jpg".to_string());
        let value = activity_json(&state, 0);

        assert!(value["assets"].get("large_image").is_none());
        assert_eq!(value["assets"]["large_text"], "Hurry Up, We're Dreaming");
    }

    #[test]
    fn non_spotify_uri_has_no_button() {
        let mut state = track_state();
        state.track.as_mut().unwrap().uri = "local:track:abc123".to_string();
        let value = activity_json(&state, 0);

        assert!(value.get("buttons").is_none());
        assert!(value["assets"].get("large_url").is_none());
    }

    #[test]
    fn long_metadata_is_truncated_to_discord_limits() {
        let mut state = track_state();
        let track = state.track.as_mut().unwrap();
        track.title = "t".repeat(200);
        track.artist = "a".repeat(200);
        track.album = "b".repeat(200);
        let value = activity_json(&state, 0);

        assert_eq!(value["details"].as_str().unwrap().chars().count(), 128);
        assert_eq!(value["state"].as_str().unwrap().chars().count(), 128);
        assert_eq!(
            value["assets"]["large_text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            128
        );
    }

    #[test]
    fn apply_state_without_client_is_a_no_op() {
        let mut client = None;
        let mut next_attempt = Instant::now() + RETRY_INTERVAL;
        let mut warned = false;

        let applied = apply_state(
            &mut client,
            "123456789012345678",
            &mut next_attempt,
            &mut warned,
            &track_state(),
        );

        assert!(!applied);
        assert!(client.is_none());
        assert!(!warned);
        assert!(next_attempt > Instant::now());
    }
}
