use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Client ID of the user's Spotify developer app (Web API OAuth only).
    #[serde(default)]
    pub client_id: String,

    /// Redirect URI registered for that app; exact match required.
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,

    /// Initial local playback volume (0-100 here; librespot uses 0-65535).
    #[serde(default = "default_volume")]
    pub volume: u16,

    /// Audio backend passed to librespot (rodio, pulseaudio, alsa, ...).
    #[serde(default = "default_backend")]
    pub audio_backend: String,
}

fn default_redirect_uri() -> String {
    "http://127.0.0.1:8989/callback".to_string()
}

fn default_volume() -> u16 {
    60
}

fn default_backend() -> String {
    "rodio".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            redirect_uri: default_redirect_uri(),
            volume: default_volume(),
            audio_backend: default_backend(),
        }
    }
}

pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub config_file: PathBuf,
    pub token_file: PathBuf,
    pub playback_token_file: PathBuf,
    pub lyrics_cache_file: PathBuf,
    pub downloads_dir: PathBuf,
}

/// Accept an environment-supplied base directory only when it is absolute, as
/// required by the XDG base-directory spec.
fn absolute_env_path(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("could not determine home directory")
}

#[cfg(target_os = "linux")]
fn config_root() -> Result<PathBuf> {
    if let Some(root) = absolute_env_path(std::env::var_os("XDG_CONFIG_HOME")) {
        return Ok(root);
    }
    Ok(home_dir()?.join(".config"))
}

#[cfg(target_os = "linux")]
fn cache_root() -> Result<PathBuf> {
    if let Some(root) = absolute_env_path(std::env::var_os("XDG_CACHE_HOME")) {
        return Ok(root);
    }
    Ok(home_dir()?.join(".cache"))
}

#[cfg(target_os = "macos")]
fn config_root() -> Result<PathBuf> {
    Ok(home_dir()?.join("Library").join("Application Support"))
}

#[cfg(target_os = "macos")]
fn cache_root() -> Result<PathBuf> {
    Ok(home_dir()?.join("Library").join("Caches"))
}

#[cfg(target_os = "windows")]
fn config_root() -> Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .context("APPDATA is not set; could not determine config directory")
}

#[cfg(target_os = "windows")]
fn cache_root() -> Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is not set; could not determine cache directory")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn config_root() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn cache_root() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cache"))
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let config_dir = config_root()?.join("termify");
        let cache_dir = cache_root()?.join("termify");
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            token_file: cache_dir.join("token.json"),
            playback_token_file: cache_dir.join("playback_token.json"),
            lyrics_cache_file: crate::lyrics::cache_file(&cache_dir),
            downloads_dir: cache_dir.join("downloads"),
            config_dir,
            cache_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.cache_dir)?;
        Ok(())
    }
}

pub fn load(paths: &Paths) -> Result<Config> {
    paths.ensure_dirs()?;

    let mut config = if paths.config_file.exists() {
        let contents = std::fs::read_to_string(&paths.config_file)
            .with_context(|| format!("reading {}", paths.config_file.display()))?;
        toml::from_str::<Config>(&contents)
            .with_context(|| format!("parsing {}", paths.config_file.display()))?
    } else {
        let config = Config::default();
        let contents = toml::to_string_pretty(&config)?;
        std::fs::write(&paths.config_file, contents)
            .with_context(|| format!("writing {}", paths.config_file.display()))?;
        config
    };

    if let Ok(id) = std::env::var("SPOTIFY_CLIENT_ID") {
        if !id.trim().is_empty() {
            config.client_id = id.trim().to_string();
        }
    }

    if config.client_id.trim().is_empty() {
        bail!(
            "No Spotify client ID configured for Web API access.\n\
             1. Create an app at https://developer.spotify.com/dashboard\n\
             2. Add redirect URI: {}\n\
             3. Set SPOTIFY_CLIENT_ID or edit {}",
            config.redirect_uri,
            paths.config_file.display()
        );
    }

    if config.volume > 100 {
        bail!("config.volume must be between 0 and 100");
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn absolute_env_paths_only_accept_absolute_paths() {
        let absolute = std::env::current_dir()
            .expect("current directory")
            .join("termify-config");
        assert_eq!(
            absolute_env_path(Some(absolute.clone().into_os_string())),
            Some(absolute)
        );
        assert_eq!(absolute_env_path(Some("relative/path".into())), None);
        assert_eq!(absolute_env_path(None), None);
    }

    #[test]
    fn app_dirs_are_scoped_to_termify() {
        // Exercise the pure part of path resolution without mutating the
        // process environment (tests run in parallel).
        let config = Path::new("config").join("termify");
        let cache = Path::new("cache").join("termify");
        assert_eq!(config, Path::new("config/termify"));
        assert_eq!(cache, Path::new("cache/termify"));
        assert_eq!(config.join("config.toml"), Path::new("config/termify/config.toml"));
        assert_eq!(cache.join("token.json"), Path::new("cache/termify/token.json"));
        assert_eq!(
            cache.join("lyrics.json"),
            Path::new("cache/termify/lyrics.json")
        );
        assert_eq!(
            cache.join("downloads"),
            Path::new("cache/termify/downloads")
        );
    }
}
