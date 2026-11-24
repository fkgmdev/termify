use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_playback::audio_backend;
use librespot_playback::config::{AudioFormat, PlayerConfig};
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEventChannel};

use crate::config::{Config, Paths};

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
        let parsed = SpotifyUri::from_uri(uri)
            .map_err(|err| anyhow::anyhow!("invalid Spotify URI: {err}"))?;
        anyhow::ensure!(parsed.is_playable(), "URI is not playable: {uri}");
        self.player.load(parsed, true, position_ms);
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

    pub fn set_volume(&self, percent: u16) {
        let percent = percent.min(100);
        self.mixer
            .set_volume((u32::from(percent) * u32::from(u16::MAX) / 100) as u16);
    }

    pub fn event_channel(&self) -> PlayerEventChannel {
        self.player.get_player_event_channel()
    }
}
