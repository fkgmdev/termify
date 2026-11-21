//! MPRIS D-Bus integration.
//!
//! Publishes the current track and playback state to the desktop so the DE's
//! media player (GNOME Shell, KDE Plasma, XFCE, ...) can show what termify is
//! playing. Control callbacks are forwarded into the app's `Action` channel,
//! which makes global XF86 media keys work without grabbing them ourselves —
//! desktop environments route those keys to the active MPRIS player.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Player, Time, TrackId};
use tokio::sync::{mpsc, watch};

use crate::ui::{Action, RepeatMode};

/// Deterministic MPRIS object path for a Spotify URI. D-Bus object paths cannot
/// contain `:`, so a stable FNV-1a hash keeps the id valid and comparable.
pub fn track_id(uri: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in uri.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("/termify/track/{hash:016x}")
}

/// Track metadata snapshot that can cross thread boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MprisTrack {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
    pub duration: Duration,
}

/// Complete playback snapshot published after every handled `Action`.
#[derive(Debug, Clone, PartialEq)]
pub struct MprisState {
    pub track: Option<MprisTrack>,
    pub position: Duration,
    pub playing: bool,
    /// 0.0..=1.0, as required by the MPRIS spec.
    pub volume: f64,
    pub shuffle: bool,
    pub repeat: RepeatMode,
}

impl MprisState {
    pub fn empty() -> Self {
        Self {
            track: None,
            position: Duration::ZERO,
            playing: false,
            volume: 1.0,
            shuffle: false,
            repeat: RepeatMode::Off,
        }
    }
}

/// Map termify's repeat mode to the MPRIS `Loop_Status` property.
pub fn loop_status(repeat: RepeatMode) -> LoopStatus {
    match repeat {
        RepeatMode::Off => LoopStatus::None,
        RepeatMode::All => LoopStatus::Playlist,
        RepeatMode::One => LoopStatus::Track,
    }
}

/// Map an MPRIS `Loop_Status` change back into an app action.
pub fn repeat_mode(status: LoopStatus) -> RepeatMode {
    match status {
        LoopStatus::None => RepeatMode::Off,
        LoopStatus::Playlist => RepeatMode::All,
        LoopStatus::Track => RepeatMode::One,
    }
}

/// Cloneable handle used by the main loop to publish state snapshots.
#[derive(Debug, Clone)]
pub struct MprisHandle {
    tx: watch::Sender<MprisState>,
}

impl MprisHandle {
    /// Publish the latest state. A `watch` channel keeps only the newest value,
    /// so a slow D-Bus server can never make the DE display stale progress.
    pub fn publish(&self, state: MprisState) {
        let _ = self.tx.send(state);
    }
}

/// Start the MPRIS server on a dedicated thread with a current-thread runtime.
///
/// `mpris_server`'s ready-to-use `Player` is `Rc`-based and therefore `!Send`;
/// running it on its own thread keeps it off the multi-threaded app runtime.
/// Returns `Ok(None)` when there is no session bus to connect to.
pub fn spawn(action_tx: mpsc::Sender<Action>) -> Result<Option<MprisHandle>> {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        tracing::info!("No DBUS_SESSION_BUS_ADDRESS; MPRIS is disabled");
        return Ok(None);
    }

    let (state_tx, state_rx) = watch::channel(MprisState::empty());
    let handle = MprisHandle { tx: state_tx };

    thread::Builder::new()
        .name("termify-mpris".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::warn!("Could not start MPRIS runtime: {err}");
                    return;
                }
            };

            runtime.block_on(async move {
                let player = match build_player(action_tx).await {
                    Ok(player) => player,
                    Err(err) => {
                        tracing::warn!("MPRIS unavailable: {err:#}");
                        return;
                    }
                };

                let server = player.run();
                tokio::pin!(server);

                let mut last: Option<MprisState> = None;
                let mut state_rx = state_rx;
                loop {
                    tokio::select! {
                        // The server task finishes when the player is dropped.
                        _ = &mut server => break,
                        changed = state_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            let state = state_rx.borrow_and_update().clone();
                            apply_state(&player, &state, last.as_ref()).await;
                            last = Some(state);
                        }
                    }
                }
                tracing::debug!("MPRIS server stopped");
            });
        })
        .context("spawning MPRIS thread")?;

    Ok(Some(handle))
}

async fn build_player(action_tx: mpsc::Sender<Action>) -> Result<Player> {
    // The D-Bus spec requires a unique bus name suffix; the PID prevents two
    // termify instances from fighting over the same well-known name.
    let suffix = format!("termify.instance{}", std::process::id());
    let player = Player::builder(&suffix)
        .identity("termify")
        .desktop_entry("termify")
        .supported_uri_schemes(["spotify"])
        .can_quit(true)
        .can_raise(false)
        .can_set_fullscreen(false)
        .has_track_list(false)
        .can_play(true)
        .can_pause(true)
        .can_seek(true)
        .can_go_next(true)
        .can_go_previous(true)
        .loop_status(LoopStatus::None)
        .build()
        .await
        .context("creating MPRIS player")?;

    let tx = action_tx.clone();
    player.connect_play_pause(move |_| {
        let _ = tx.try_send(Action::MprisPlayPause);
    });
    let tx = action_tx.clone();
    player.connect_play(move |_| {
        let _ = tx.try_send(Action::MprisPlay);
    });
    let tx = action_tx.clone();
    player.connect_pause(move |_| {
        let _ = tx.try_send(Action::MprisPause);
    });
    let tx = action_tx.clone();
    player.connect_stop(move |_| {
        // termify has no "stop" state; treat it as pause so Play can resume.
        let _ = tx.try_send(Action::MprisPause);
    });
    let tx = action_tx.clone();
    player.connect_next(move |_| {
        let _ = tx.try_send(Action::MprisNext);
    });
    let tx = action_tx.clone();
    player.connect_previous(move |_| {
        let _ = tx.try_send(Action::MprisPrev);
    });
    let tx = action_tx.clone();
    player.connect_seek(move |_, offset| {
        let _ = tx.try_send(Action::MprisSeek(offset.as_millis()));
    });
    let tx = action_tx.clone();
    player.connect_set_position(move |_, id, position| {
        let _ = tx.try_send(Action::MprisSetPosition {
            track: id.to_string(),
            position_ms: position.as_millis().max(0) as u64,
        });
    });
    let tx = action_tx.clone();
    player.connect_set_volume(move |_, volume| {
        let _ = tx.try_send(Action::MprisSetVolume(volume.clamp(0.0, 1.0)));
    });
    let tx = action_tx.clone();
    player.connect_set_shuffle(move |_, shuffle| {
        let _ = tx.try_send(Action::MprisSetShuffle(shuffle));
    });
    let tx = action_tx.clone();
    player.connect_set_loop_status(move |_, status| {
        let _ = tx.try_send(Action::MprisSetRepeat(repeat_mode(status)));
    });
    let tx = action_tx.clone();
    player.connect_open_uri(move |_, uri| {
        let _ = tx.try_send(Action::MprisOpenUri(uri.to_string()));
    });
    player.connect_quit(move |_| {
        let _ = action_tx.try_send(Action::MprisQuit);
    });

    Ok(player)
}

fn position_time(position: Duration) -> Time {
    Time::from_millis(i64::try_from(position.as_millis()).unwrap_or(i64::MAX))
}

async fn apply_state(player: &Player, state: &MprisState, last: Option<&MprisState>) {
    let metadata = match &state.track {
        Some(track) => {
            let id = TrackId::try_from(track_id(&track.uri).as_str())
                .unwrap_or(TrackId::NO_TRACK);
            let mut builder = Metadata::builder()
                .trackid(id)
                .title(track.title.clone())
                .artist([track.artist.clone()])
                .length(position_time(track.duration));
            if !track.album.is_empty() {
                builder = builder.album(track.album.clone());
            }
            if let Some(url) = &track.art_url {
                builder = builder.art_url(url.clone());
            }
            builder.build()
        }
        None => Metadata::new(),
    };
    let _ = player.set_metadata(metadata).await;

    let status = if state.playing {
        PlaybackStatus::Playing
    } else if state.track.is_some() {
        PlaybackStatus::Paused
    } else {
        PlaybackStatus::Stopped
    };
    let _ = player.set_playback_status(status).await;
    let _ = player.set_shuffle(state.shuffle).await;
    let _ = player.set_loop_status(loop_status(state.repeat)).await;
    let _ = player.set_volume(state.volume.clamp(0.0, 1.0)).await;

    let position = position_time(state.position);
    let previous_uri = last.and_then(|last| last.track.as_ref()).map(|track| track.uri.as_str());
    let current_uri = state.track.as_ref().map(|track| track.uri.as_str());
    if previous_uri.is_some() && previous_uri == current_uri {
        if let Some(last) = last {
            // Only announce deliberate jumps; normal 250 ms progress is handled
            // implicitly by clients through the Rate/Position properties.
            if (position - position_time(last.position)).abs() > Time::from_secs(2) {
                let _ = player.seeked(position).await;
            }
        }
    }
    // `set_position` deliberately does not emit PropertiesChanged (per spec).
    player.set_position(position);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_id_is_stable_valid_and_unique() {
        let a = track_id("spotify:track:abc123");
        assert_eq!(a, track_id("spotify:track:abc123"));
        assert_ne!(a, track_id("spotify:track:def456"));
        assert!(a.starts_with("/termify/track/"));
        assert!(TrackId::try_from(a.as_str()).is_ok());
    }

    #[test]
    fn empty_state_has_sensible_defaults() {
        let state = MprisState::empty();
        assert!(state.track.is_none());
        assert!(!state.playing);
        assert_eq!(state.position, Duration::ZERO);
        assert_eq!(state.volume, 1.0);
        assert_eq!(state.repeat, RepeatMode::Off);
    }

    #[test]
    fn repeat_maps_to_mpris_loop_status_and_back() {
        for mode in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(repeat_mode(loop_status(mode)), mode);
        }
        assert_eq!(loop_status(RepeatMode::One), LoopStatus::Track);
        assert_eq!(loop_status(RepeatMode::All), LoopStatus::Playlist);
    }
}
