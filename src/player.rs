use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_core::{FileId, SpotifyId};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};
use librespot_playback::audio_backend;
use librespot_playback::config::{AudioFormat, PlayerConfig};
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEventChannel};

use crate::config::{Config, Paths};

/// librespot's default `Bitrate160` format order. The key preflight must ask for
/// the same file ID the player itself will pick, or a successful check would not
/// protect the actual load.
const PLAYBACK_FORMATS: [AudioFileFormat; 7] = [
    AudioFileFormat::OGG_VORBIS_160,
    AudioFileFormat::MP3_160,
    AudioFileFormat::OGG_VORBIS_96,
    AudioFileFormat::MP3_96,
    AudioFileFormat::MP3_256,
    AudioFileFormat::OGG_VORBIS_320,
    AudioFileFormat::MP3_320,
];
const METADATA_TIMEOUT: Duration = Duration::from_secs(15);

/// Check that Spotify will actually give us an audio key before asking
/// librespot to load a track.
///
/// librespot 0.8 gives up on a key request after 1.5 seconds and then feeds the
/// still-encrypted CDN bytes to Symphonia "to be safe". Ogg probing fails, but
/// the MP3 demuxer happily scans ciphertext and floods the log with "skipping
/// junk" warnings while producing no sound. Retrying the small key request here
/// (with the same backoff as downloads) both hides that transient case and lets
/// us report a permanent key refusal clearly instead of silently stalling.
pub async fn preflight_audio_key(session: &Session, uri: &str) -> Result<()> {
    let parsed =
        SpotifyUri::from_uri(uri).map_err(|err| anyhow::anyhow!("invalid Spotify URI: {err}"))?;
    let requested_id: SpotifyId = (&parsed)
        .try_into()
        .map_err(|err| anyhow::anyhow!("invalid track ID: {err}"))?;
    let item = tokio::time::timeout(METADATA_TIMEOUT, AudioItem::get_file(session, parsed))
        .await
        .with_context(|| format!("timed out reading metadata for {uri}"))?
        .with_context(|| format!("could not read track metadata for {uri}"))?;
    if let Err(reason) = &item.availability {
        bail!("track is not available for playback: {reason}");
    }
    // Metadata can resolve to a different ID (for example, an alternative
    // version of a track). Spotify's key service expects that ID.
    let track_id: SpotifyId = (&item.track_id).try_into().unwrap_or(requested_id);
    let file_id: FileId = PLAYBACK_FORMATS
        .iter()
        .find_map(|format| item.files.get(format).copied())
        .context("track has no audio format supported by librespot")?;
    crate::offline::request_audio_key(session, track_id, file_id, uri).await?;
    Ok(())
}

pub struct LocalPlayer {
    /// Kept alive so the streaming connection stays open for the player's lifetime.
    #[allow(dead_code)]
    pub session: Session,
    pub player: Arc<Player>,
    pub mixer: SoftMixer,
}

impl LocalPlayer {
    /// Connect a librespot session using the Web API access token. This is the
    /// go/no-go point for the whole local-playback approach.
    pub async fn connect(access_token: &str, config: &Config, paths: &Paths) -> Result<Self> {
        // librespot's login5/keymaster exchange needs its built-in desktop client
        // ID for audio keys; the custom ID is only for OAuth and the Web API.
        let session_config = SessionConfig {
            tmp_dir: paths.cache_dir.clone(),
            ..Default::default()
        };
        let cache = Cache::new::<PathBuf>(
            None,
            None,
            Some(paths.cache_dir.join("audio")),
            Some(512 * 1024 * 1024),
        )
        .context("creating librespot cache")?;

        let session = Session::new(session_config, Some(cache));
        let credentials = Credentials::with_access_token(access_token.to_string());
        session.connect(credentials, true).await?;
        tracing::info!(username = %session.username(), "librespot session connected");

        let mixer = SoftMixer::open(MixerConfig::default())?;
        mixer.set_volume((u32::from(config.volume) * u32::from(u16::MAX) / 100) as u16);

        let sink_builder = audio_backend::find(Some(config.audio_backend.clone()))
            .with_context(|| format!("unknown audio backend '{}'", config.audio_backend))?;
        let player = Player::new(
            PlayerConfig {
                // Ask librespot for periodic authoritative positions so a
                // buffer underrun can be corrected instead of drifting.
                position_update_interval: Some(std::time::Duration::from_secs(1)),
                ..PlayerConfig::default()
            },
            session.clone(),
            mixer.clone().get_soft_volume(),
            move || sink_builder(None, AudioFormat::default()),
        );

        Ok(Self {
            session,
            player,
            mixer,
        })
    }

    pub fn play_uri(&self, uri: &str) -> Result<()> {
        self.play_uri_at(uri, 0)
    }

    pub fn play_uri_at(&self, uri: &str, position_ms: u32) -> Result<()> {
        self.load_uri(uri, position_ms, true)
    }

    /// Load a track without starting it, for when the user paused while the
    /// audio key preflight was still running.
    pub fn play_uri_at_paused(&self, uri: &str, position_ms: u32) -> Result<()> {
        self.load_uri(uri, position_ms, false)
    }

    fn load_uri(&self, uri: &str, position_ms: u32, start_playing: bool) -> Result<()> {
        let parsed = SpotifyUri::from_uri(uri)
            .map_err(|err| anyhow::anyhow!("invalid Spotify URI: {err}"))?;
        anyhow::ensure!(parsed.is_playable(), "URI is not playable: {uri}");
        self.player.load(parsed, start_playing, position_ms);
        Ok(())
    }

    pub fn play(&self) {
        self.player.play();
    }

    pub fn pause(&self) {
        self.player.pause();
    }

    pub fn seek(&self, position_ms: u32) {
        self.player.seek(position_ms);
    }

    /// Cancel a load that never produced audio. Unlike `pause`, `stop` moves
    /// librespot out of its terminated `Loading` state so a later `play()` can
    /// actually work again.
    pub fn stop(&self) {
        self.player.stop();
    }

    pub fn set_volume(&self, percent: u16) {
        let percent = percent.min(100);
        self.mixer
            .set_volume((u32::from(percent) * u32::from(u16::MAX) / 100) as u16);
    }

    pub fn event_channel(&self) -> PlayerEventChannel {
        self.player.get_player_event_channel()
    }
}
