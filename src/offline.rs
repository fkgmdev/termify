//! Offline download index and local playback of downloaded tracks.
//!
//! Downloads are stored as decrypted Ogg Vorbis or MP3 files in the application
//! cache directory. Each track's cover art is saved beside the audio, and its
//! lyrics (plus an English translation when available) are written to the
//! persistent `lyrics.json` cache. `downloads.json` maps Spotify URIs to those
//! files so playback continues with `--offline` and no network session at all.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use chrono::Local;
use librespot_audio::{AudioDecrypt, AudioFetchParams, AudioFile};
use librespot_core::audio_key::AudioKey;
use librespot_core::{FileId, Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioFiles, AudioItem};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::time::sleep;

use crate::ui::Track;

/// Version 2 adds downloaded-playlist and downloaded-album memberships. Version
/// 1 indexes are still readable and are upgraded on the next successful save.
const INDEX_VERSION: u8 = 2;
const LEGACY_INDEX_VERSION: u8 = 1;
const INDEX_FILE: &str = "downloads.json";
/// Append-only record of failed downloads, with no token or credential data.
const ERROR_LOG_FILE: &str = "download-errors.log";
/// Spotify puts a 167-byte normalisation header before Ogg Vorbis audio.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;
/// Audio-key requests can fail transiently during bulk downloads. Space them
/// out and retry with backoff instead of writing a file that cannot be decoded.
const KEY_REQUEST_DELAY: Duration = Duration::from_millis(250);
const KEY_REQUEST_ATTEMPTS: u32 = 4;
/// CDN reads can time out when Spotify throttles a long playlist download.
/// Retry the whole format once or twice before falling back to a lower bitrate.
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// A track stored on disk, with just enough metadata for the Downloads view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadEntry {
    pub track: Track,
    /// File name relative to the downloads directory.
    pub file: String,
    /// Saved cover image, if one was available. Older v2 indexes have no field.
    #[serde(default)]
    pub cover_file: Option<String>,
    pub size: u64,
    /// Unix timestamp, used only for display/sorting.
    pub downloaded_at: u64,
}

/// Optional headless helpers used while downloading a track. Cover art is
/// fetched with `reqwest` (no terminal needed) and lyrics go into the shared
/// cache so the same data is available in `--offline` mode.
#[derive(Debug, Clone, Default)]
pub struct DownloadAssets {
    pub http_client: Option<reqwest::Client>,
    pub lyrics_cache_file: Option<PathBuf>,
}

/// Each optional asset request is bounded. A stalled CDN or translation
/// endpoint must never keep a playlist download from reporting progress.
const ASSET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const ASSET_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ASSET_TOTAL_TIMEOUT: Duration = Duration::from_secs(25);
/// Bound parallel optional requests during bulk downloads so LRCLIB and the
/// translation service are not hammered by a whole playlist at once.
const MAX_PARALLEL_ASSET_TASKS: usize = 4;
static ASSET_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

fn asset_semaphore() -> &'static Semaphore {
    ASSET_SEMAPHORE.get_or_init(|| Semaphore::new(MAX_PARALLEL_ASSET_TASKS))
}

impl DownloadAssets {
    /// Client with explicit timeouts for optional cover/lyrics requests.
    pub fn new(lyrics_cache_file: Option<PathBuf>) -> Self {
        let http_client = reqwest::Client::builder()
            .connect_timeout(ASSET_CONNECT_TIMEOUT)
            .timeout(ASSET_REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http_client: Some(http_client),
            lyrics_cache_file,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u8,
    #[serde(default)]
    tracks: BTreeMap<String, DownloadEntry>,
    #[serde(default)]
    playlists: BTreeMap<String, DownloadedPlaylist>,
    /// Older v1/v2 indexes have no album memberships.
    #[serde(default)]
    albums: BTreeMap<String, DownloadedAlbum>,
}

/// A playlist whose tracks are available offline. Membership is stored by URI
/// so a track can belong to several playlists without duplicating audio files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadedPlaylist {
    pub id: String,
    pub name: String,
    pub track_uris: Vec<String>,
    pub downloaded_at: u64,
}

/// An album whose tracks are available offline. Like playlists, membership is
/// stored by URI so a track can belong to several groupings without duplicating
/// audio files. `artist` defaults so older indexes remain readable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadedAlbum {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: String,
    pub track_uris: Vec<String>,
    pub downloaded_at: u64,
}

/// Persistent index of downloaded tracks, playlists and albums. Missing or
/// corrupt files are dropped when the index is loaded so the UI never offers a
/// download that cannot be played.
#[derive(Debug, Clone)]
pub struct DownloadStore {
    dir: PathBuf,
    index_file: PathBuf,
    entries: BTreeMap<String, DownloadEntry>,
    playlists: BTreeMap<String, DownloadedPlaylist>,
    albums: BTreeMap<String, DownloadedAlbum>,
}

impl DownloadStore {
    /// Open (or create) the store under `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let index_file = Self::index_file(&dir);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating downloads directory {}", dir.display()))?;
        let mut store = Self {
            dir,
            index_file,
            entries: BTreeMap::new(),
            playlists: BTreeMap::new(),
            albums: BTreeMap::new(),
        };
        store.load()?;
        Ok(store)
    }

    /// Empty store used by `--demo` and tests, where no files should be written.
    pub fn disabled() -> Self {
        Self {
            dir: PathBuf::new(),
            index_file: PathBuf::new(),
            entries: BTreeMap::new(),
            playlists: BTreeMap::new(),
            albums: BTreeMap::new(),
        }
    }

    pub fn index_file(dir: &Path) -> PathBuf {
        dir.join(INDEX_FILE)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn get(&self, uri: &str) -> Option<&DownloadEntry> {
        self.entries.get(uri)
    }

    pub fn contains(&self, uri: &str) -> bool {
        self.entries.contains_key(uri)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Tracks in the Downloads view, most recently downloaded first.
    pub fn tracks(&self) -> Vec<Track> {
        let mut entries: Vec<&DownloadEntry> = self.entries.values().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.downloaded_at));
        entries
            .into_iter()
            .map(|entry| entry.track.clone())
            .collect()
    }

    /// Downloaded playlists, most recently saved first.
    pub fn playlists(&self) -> Vec<DownloadedPlaylist> {
        let mut playlists: Vec<&DownloadedPlaylist> = self.playlists.values().collect();
        playlists.sort_by_key(|playlist| std::cmp::Reverse(playlist.downloaded_at));
        playlists.into_iter().cloned().collect()
    }

    /// Saved tracks of a downloaded playlist, in playlist order. Tracks that
    /// were later deleted are left out rather than breaking the queue.
    pub fn playlist_tracks(&self, id: &str) -> Vec<Track> {
        let Some(playlist) = self.playlists.get(id) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        playlist
            .track_uris
            .iter()
            .filter_map(|uri| self.entries.get(uri))
            .filter(|entry| seen.insert(&entry.track.uri))
            .map(|entry| entry.track.clone())
            .collect()
    }

    /// Record that a playlist is available offline. Only URIs with a saved file
    /// are stored, so deleting a track automatically shrinks the playlist.
    pub fn insert_playlist(&mut self, id: &str, name: &str, uris: &[String]) -> Result<()> {
        let track_uris: Vec<String> = uris
            .iter()
            .filter(|uri| self.entries.contains_key(uri.as_str()))
            .cloned()
            .collect();
        if track_uris.is_empty() {
            return Ok(());
        }
        self.playlists.insert(
            id.to_string(),
            DownloadedPlaylist {
                id: id.to_string(),
                name: name.to_string(),
                track_uris,
                downloaded_at: now_timestamp(),
            },
        );
        self.save()
    }

    /// Forget an offline playlist grouping without deleting its audio files.
    pub fn remove_playlist(&mut self, id: &str) -> Result<()> {
        if self.playlists.remove(id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// Downloaded albums, ordered by name (then id) for a stable listing.
    pub fn albums(&self) -> Vec<DownloadedAlbum> {
        let mut albums: Vec<&DownloadedAlbum> = self.albums.values().collect();
        albums.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        albums.into_iter().cloned().collect()
    }

    /// Saved tracks of a downloaded album, in the order they were stored.
    /// Tracks that were later deleted are left out rather than breaking the
    /// queue.
    pub fn album_tracks(&self, id: &str) -> Vec<Track> {
        let Some(album) = self.albums.get(id) else {
            return Vec::new();
        };
        album
            .track_uris
            .iter()
            .filter_map(|uri| self.entries.get(uri))
            .map(|entry| entry.track.clone())
            .collect()
    }

    /// Record that an album is available offline. URIs are deduplicated while
    /// keeping their original order; the index is saved so the grouping
    /// survives a restart. Deleted tracks are pruned when the index is loaded.
    pub fn insert_album(
        &mut self,
        id: &str,
        name: &str,
        artist: &str,
        uris: Vec<String>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        let track_uris: Vec<String> = uris
            .into_iter()
            .filter(|uri| seen.insert(uri.clone()))
            .collect();
        self.albums.insert(
            id.to_string(),
            DownloadedAlbum {
                id: id.to_string(),
                name: name.to_string(),
                artist: artist.to_string(),
                track_uris,
                downloaded_at: now_timestamp(),
            },
        );
        self.save()
    }

    /// Forget an offline album grouping without deleting its audio files.
    pub fn remove_album(&mut self, id: &str) -> Result<()> {
        if self.albums.remove(id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// Full path of a stored audio file.
    pub fn audio_path(&self, entry: &DownloadEntry) -> PathBuf {
        self.dir.join(&entry.file)
    }

    /// Full path of a saved cover image, when one exists for this download.
    pub fn cover_path(&self, entry: &DownloadEntry) -> Option<PathBuf> {
        entry
            .cover_file
            .as_ref()
            .filter(|name| safe_cover_file_name(name))
            .map(|name| self.dir.join(name))
            .filter(|path| path.is_file())
    }

    /// Commit a completed download and persist the index atomically.
    pub fn insert(&mut self, entry: DownloadEntry) -> Result<()> {
        self.entries.insert(entry.track.uri.clone(), entry);
        self.save()
    }

    /// Attach a cover saved after the audio download finished. Used by the
    /// background asset task so playlist progress never waits on the network.
    pub fn set_cover(&mut self, uri: &str, cover_file: String) -> Result<()> {
        if !safe_cover_file_name(&cover_file) {
            return Ok(());
        }
        if let Some(entry) = self.entries.get_mut(uri) {
            entry.cover_file = Some(cover_file);
            self.save()?;
        }
        Ok(())
    }

    /// Remove a download from the index and delete its audio file. A missing
    /// file is treated as success so stale entries can always be cleaned up.
    pub fn remove(&mut self, uri: &str) -> Result<()> {
        if let Some(entry) = self.entries.remove(uri) {
            if let Err(err) = std::fs::remove_file(self.dir.join(&entry.file)) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    self.entries.insert(uri.to_string(), entry);
                    return Err(err).with_context(|| "deleting downloaded audio");
                }
            }
            if let Some(cover) = &entry.cover_file {
                if safe_cover_file_name(cover) {
                    let _ = std::fs::remove_file(self.dir.join(cover));
                }
            }
            self.save()?;
        }
        Ok(())
    }

    fn load(&mut self) -> Result<()> {
        if !self.index_file.exists() {
            return Ok(());
        }
        let contents = match std::fs::read_to_string(&self.index_file) {
            Ok(contents) => contents,
            Err(err) => {
                tracing::warn!(
                    "Ignoring unreadable download index {}: {err}",
                    self.index_file.display()
                );
                return Ok(());
            }
        };
        // A corrupt or future index must not prevent startup. Audio files are
        // left untouched; the next successful save rewrites a clean index.
        let file: IndexFile = match serde_json::from_str(&contents) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    "Ignoring unreadable download index {}: {err}",
                    self.index_file.display()
                );
                return Ok(());
            }
        };
        if !matches!(file.version, INDEX_VERSION | LEGACY_INDEX_VERSION) {
            tracing::warn!(
                "Ignoring download index {} with unsupported version {}",
                self.index_file.display(),
                file.version
            );
            return Ok(());
        }
        self.entries = file
            .tracks
            .into_iter()
            .filter(|(_, entry)| {
                safe_file_name(&entry.file)
                    && audio_file_header_valid(
                        &self.dir.join(&entry.file),
                        entry.file.ends_with(".ogg"),
                    )
            })
            .map(|(uri, mut entry)| {
                // A deleted or tampered cover must not break audio playback;
                // simply forget where it used to be.
                if let Some(cover) = &entry.cover_file {
                    if !safe_cover_file_name(cover) || !self.dir.join(cover).is_file() {
                        entry.cover_file = None;
                    }
                }
                (uri, entry)
            })
            .collect();
        // Playlists can only contain tracks that survived the check above.
        self.playlists = file
            .playlists
            .into_iter()
            .filter(|(_, playlist)| !playlist.track_uris.is_empty())
            .map(|(id, mut playlist)| {
                playlist
                    .track_uris
                    .retain(|uri| self.entries.contains_key(uri));
                (id, playlist)
            })
            .filter(|(_, playlist)| !playlist.track_uris.is_empty())
            .collect();
        // Albums follow the same rule: only tracks that survived validation
        // remain, and an album with nothing left is forgotten.
        self.albums = file
            .albums
            .into_iter()
            .filter(|(_, album)| !album.track_uris.is_empty())
            .map(|(id, mut album)| {
                album
                    .track_uris
                    .retain(|uri| self.entries.contains_key(uri));
                (id, album)
            })
            .filter(|(_, album)| !album.track_uris.is_empty())
            .collect();
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if self.dir.as_os_str().is_empty() {
            return Ok(());
        }
        let file = IndexFile {
            version: INDEX_VERSION,
            tracks: self.entries.clone(),
            playlists: self.playlists.clone(),
            albums: self.albums.clone(),
        };
        let json = serde_json::to_string_pretty(&file).context("encoding download index")?;
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let tmp = self.dir.join("downloads.json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &Self::index_file(&self.dir))
            .with_context(|| format!("replacing {}", self.index_file.display()))?;
        Ok(())
    }
}

/// Stable, filesystem-safe name for a Spotify URI.
fn file_stem(uri: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in uri.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Only accept names written by `download_track` when reading the index, so a
/// tampered `downloads.json` cannot point playback or deletion outside the
/// downloads directory.
fn safe_file_name(name: &str) -> bool {
    let path = Path::new(name);
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    !name.contains(['/', '\\'])
        && stem.len() == 16
        && stem.chars().all(|c| c.is_ascii_hexdigit())
        && matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("ogg" | "mp3")
        )
}

/// Cover images use the same URI hash as the audio so they can be found without
/// trusting any metadata from the index beyond the safe file name.
fn cover_file_name(uri: &str) -> String {
    format!("{}.jpg", file_stem(uri))
}

/// Only accept cover names written by the download pipeline. The bytes are
/// self-describing (decoded by format sniffing), so one extension is enough.
fn safe_cover_file_name(name: &str) -> bool {
    let path = Path::new(name);
    !name.contains(['/', '\\'])
        && path.extension().and_then(|ext| ext.to_str()) == Some("jpg")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.len() == 16 && stem.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Check the first bytes of a stored file before offering it for playback.
/// A download decrypted with the wrong (or no) key starts with random bytes,
/// which decoders report as a long stream of "skipping junk" warnings.
///
/// `is_ogg` is explicit rather than inferred from the path extension because
/// validation also runs on `.part` files, whose extension is always `part`.
fn audio_file_header_valid(path: &Path, is_ogg: bool) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0u8; 4];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if is_ogg {
        return header == *b"OggS";
    }
    // MP3 files either carry an ID3 tag or start with an MPEG frame sync.
    header[0..3] == [b'I', b'D', b'3'][..] || (header[0] == 0xff && header[1] & 0xe0 == 0xe0)
}

/// Ask the same decoder used for offline playback to initialise the file. A
/// four-byte magic check alone lets roughly 1 in 2048 random wrong-key buffers
/// through for MP3, so verify the container before indexing it.
fn audio_file_decodes(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    Decoder::new(BufReader::new(file)).is_ok()
}

/// Playback backend for downloaded files. The output stream is kept alive for
/// as long as the player exists; `Sink` handles decode, volume and seeking.
pub struct OfflinePlayer {
    // Must outlive the sink, otherwise cpal shuts the device down mid-song.
    _stream: OutputStream,
    sink: Sink,
}

impl OfflinePlayer {
    pub fn new() -> Result<Self> {
        let stream = OutputStreamBuilder::open_default_stream().context("opening audio device")?;
        let sink = Sink::connect_new(stream.mixer());
        Ok(Self {
            _stream: stream,
            sink,
        })
    }

    /// Replace the current source with `path`, optionally starting at `position`.
    /// A failed seek leaves the track playing from the beginning rather than
    /// dropping playback entirely.
    pub fn play_file(&self, path: &Path, position: Duration) -> Result<()> {
        self.sink.clear();
        let file = File::open(path)
            .with_context(|| format!("opening downloaded audio {}", path.display()))?;
        let decoder = Decoder::new(BufReader::new(file))
            .with_context(|| format!("decoding downloaded audio {}", path.display()))?;
        self.sink.append(decoder);
        if !position.is_zero() && self.sink.try_seek(position).is_err() {
            tracing::warn!("Offline seek failed for {}", path.display());
        }
        self.sink.play();
        Ok(())
    }

    pub fn play(&self) {
        self.sink.play();
    }

    pub fn pause(&self) {
        self.sink.pause();
    }

    pub fn seek(&self, position: Duration) -> Result<()> {
        self.sink.try_seek(position).map_err(|err| {
            anyhow::anyhow!("could not seek downloaded audio to {position:?}: {err:?}")
        })
    }

    pub fn set_volume(&self, percent: u8) {
        let volume = f32::from(percent.min(100)) / 100.0;
        self.sink.set_volume(volume);
    }

    pub fn position(&self) -> Duration {
        self.sink.get_pos()
    }

    /// True once the current file has finished. The app uses this to advance
    /// the queue, just like librespot's `EndOfTrack` event for streaming.
    pub fn is_finished(&self) -> bool {
        self.sink.empty()
    }

    pub fn stop(&self) {
        self.sink.clear();
    }
}

/// Formats we can store and play back locally, in descending quality order.
const PREFERRED_FORMATS: [AudioFileFormat; 7] = [
    AudioFileFormat::OGG_VORBIS_320,
    AudioFileFormat::MP3_320,
    AudioFileFormat::MP3_256,
    AudioFileFormat::OGG_VORBIS_160,
    AudioFileFormat::MP3_160,
    AudioFileFormat::OGG_VORBIS_96,
    AudioFileFormat::MP3_96,
];

fn bytes_per_second(format: AudioFileFormat) -> usize {
    match format {
        AudioFileFormat::OGG_VORBIS_96 | AudioFileFormat::MP3_96 => 12 * 1024,
        AudioFileFormat::OGG_VORBIS_160 | AudioFileFormat::MP3_160 => 20 * 1024,
        AudioFileFormat::MP3_256 => 32 * 1024,
        _ => 40 * 1024,
    }
}

/// A download error worth retrying: CDN timeouts, connection resets, and
/// throttling. Container/permission failures will fail identically on retry.
fn download_error_is_transient(error: &anyhow::Error) -> bool {
    let mut chain = error
        .chain()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    chain.make_ascii_lowercase();
    chain.contains("timeout")
        || chain.contains("timed out")
        || chain.contains("deadline expired")
        || chain.contains("connection")
        || chain.contains("rate limit")
}

/// True for key-service replies that are entitlement/compatibility failures
/// rather than transient rate limits. Retrying these only burns time.
fn audio_key_error_is_permanent(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("audio key error")
        || lower.contains("no audio key")
        || lower.contains("invalid_grant")
}

/// Request an audio key with spacing and exponential backoff. Spotify's key
/// service can answer `AesKeyError`/`Service unavailable` when a playlist
/// download fires many requests back-to-back; retrying avoids corrupt files.
async fn request_audio_key(
    session: &Session,
    track_id: SpotifyId,
    file_id: librespot_core::FileId,
    uri: &str,
) -> Result<AudioKey> {
    sleep(KEY_REQUEST_DELAY).await;
    let mut last_error = None;
    for attempt in 1..=KEY_REQUEST_ATTEMPTS {
        match session.audio_key().request(track_id, file_id).await {
            Ok(key) => return Ok(key),
            Err(err) => {
                let message = err.to_string();
                last_error = Some(message.clone());
                if attempt == KEY_REQUEST_ATTEMPTS || audio_key_error_is_permanent(&message) {
                    break;
                }
                let backoff = KEY_REQUEST_DELAY.saturating_mul(attempt);
                tracing::warn!(
                    "Audio key request for {uri} failed (attempt {attempt}/{KEY_REQUEST_ATTEMPTS}): {err}; retrying in {:?}",
                    backoff
                );
                sleep(backoff).await;
            }
        }
    }
    let detail = last_error.unwrap_or_else(|| "unknown error".to_string());
    if audio_key_error_is_permanent(&detail) {
        Err(anyhow!(
            "Spotify refused the audio key for {uri}: {detail}. This is a known \
             Spotify/librespot account restriction (Premium required; accounts created \
             after the 2026 DRM change may be ineligible) and cannot be fixed by retrying"
        ))
    } else {
        Err(anyhow!(
            "could not get audio key for {uri} after {KEY_REQUEST_ATTEMPTS} attempts: {detail}"
        ))
    }
}

/// Path of the append-only download failure log.
pub fn download_error_log(dir: &Path) -> PathBuf {
    dir.join(ERROR_LOG_FILE)
}

/// Record a full error chain without touching token caches. Failures are easy
/// to miss in the TUI status bar, especially during bulk downloads, so keep a
/// durable record next to the offline files.
fn record_download_failure(dir: &Path, track: &Track, error: &anyhow::Error) {
    let stamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!(
        "[{stamp}] {} ({}) — {} / {}: {error:#}\n",
        track.title, track.uri, track.artist, track.album
    );
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(download_error_log(dir))
    else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
}

/// Install tuned librespot fetch parameters once, before any player or decoder
/// can lock in the defaults with `AudioFetchParams::get()`. The default 8-second
/// timeout is too short for bulk downloads when Spotify's CDN slows down.
pub fn configure_audio_fetch() {
    // `AudioFetchParams` is a process-wide OnceLock; only the first call wins.
    let _ = AudioFetchParams::set(AudioFetchParams {
        minimum_download_size: 256 * 1024,
        download_timeout: Duration::from_secs(120),
        ..Default::default()
    });
}

/// Download one format into a temporary file and commit it only after its
/// container header has been verified.
async fn download_format(
    session: &Session,
    track_id: SpotifyId,
    file_id: FileId,
    format: AudioFileFormat,
    track: &Track,
    dir: &Path,
) -> Result<DownloadEntry> {
    let label = format!("{format:?}");
    // Ask for the key before opening the CDN stream. A failed key request would
    // otherwise leave a random-access stream loader running until session drop.
    let key = request_audio_key(session, track_id, file_id, &track.uri)
        .await
        .with_context(|| format!("requesting {label} audio key"))?;

    // A missing key means the bytes we would write are still encrypted. The
    // old fallback treated them as unencrypted audio, which produced files full
    // of junk that decoders could not play. Retry transient key-service failures
    // instead and never index an unverifiable file.
    let encrypted = AudioFile::open(session, file_id, bytes_per_second(format))
        .await
        .with_context(|| format!("opening {label} audio"))?;
    let controller = encrypted
        .get_stream_loader_controller()
        .context("starting audio download")?;
    controller.set_random_access_mode();
    // Ensure the background loader is stopped on every exit path, including
    // a timeout while reading the OGG header or an error in the blocking task.
    struct CloseOnDrop(librespot_audio::StreamLoaderController);
    impl Drop for CloseOnDrop {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _controller = CloseOnDrop(controller);
    let mut decrypted = AudioDecrypt::new(Some(key), encrypted);
    if AudioFiles::is_ogg_vorbis(format) {
        // Read rather than seek so the AES-CTR stream stays aligned even if a
        // future librespot version changes how `AudioDecrypt::seek` works.
        let mut header = [0u8; SPOTIFY_OGG_HEADER_END as usize];
        decrypted
            .read_exact(&mut header)
            .context("skipping Spotify OGG normalisation header")?;
    }

    let extension = if AudioFiles::is_mp3(format) {
        "mp3"
    } else {
        "ogg"
    };
    let stem = file_stem(&track.uri);
    let final_path = dir.join(format!("{stem}.{extension}"));
    let tmp_path = dir.join(format!("{stem}.{extension}.part"));
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating downloads directory {}", dir.display()))?;

    let mut output =
        File::create(&tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?;
    let write_result = tokio::task::spawn_blocking(move || -> Result<u64> {
        let mut buffer = vec![0u8; 128 * 1024];
        let mut total = 0u64;
        loop {
            let read = decrypted.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            total += read as u64;
        }
        output.flush()?;
        Ok(total)
    })
    .await
    .context("download task failed");

    let written = match write_result {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err).context("writing downloaded audio");
        }
        Err(err) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    };
    if written == 0 {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow!("downloaded audio was empty"));
    }
    // A wrong key produces bytes that look like audio to the file system but
    // not to a decoder. Verify both the magic and the container structure before
    // indexing the file.
    if !audio_file_header_valid(&tmp_path, AudioFiles::is_ogg_vorbis(format)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow!(
            "downloaded audio has an invalid {label} container header; audio key may be unavailable"
        ));
    }
    if !audio_file_decodes(&tmp_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow!(
            "downloaded {label} audio could not be decoded; audio key may be unavailable"
        ));
    }
    if let Err(err) = std::fs::rename(&tmp_path, &final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err).with_context(|| format!("storing download at {}", final_path.display()));
    }
    let size = std::fs::metadata(&final_path)?.len();

    Ok(DownloadEntry {
        track: track.clone(),
        file: final_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("download file name is not UTF-8")?
            .to_string(),
        cover_file: None,
        size,
        downloaded_at: now_timestamp(),
    })
}

/// Fill in metadata that the CLI download command cannot know from the URI
/// alone. The librespot session already fetched full track metadata to resolve
/// audio files, so this adds no request. Values supplied by the Web API (for
/// example from a playlist view) are never overwritten.
fn enriched_track(track: &Track, item: &AudioItem) -> Track {
    let mut enriched = track.clone();
    if enriched.title.is_empty() || enriched.title == enriched.uri {
        enriched.title = item.name.clone();
    }
    if enriched.artist.is_empty() {
        enriched.artist = match &item.unique_fields {
            librespot_metadata::audio::UniqueFields::Track { artists, .. } => artists
                .0
                .iter()
                .take(3)
                .map(|artist| artist.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
            librespot_metadata::audio::UniqueFields::Episode { show_name, .. } => show_name.clone(),
            librespot_metadata::audio::UniqueFields::Local { artists, .. } => {
                artists.clone().unwrap_or_default()
            }
        };
    }
    if enriched.album.is_empty() {
        enriched.album = match &item.unique_fields {
            librespot_metadata::audio::UniqueFields::Track { album, .. } => album.clone(),
            librespot_metadata::audio::UniqueFields::Episode { show_name, .. } => show_name.clone(),
            librespot_metadata::audio::UniqueFields::Local { album, .. } => {
                album.clone().unwrap_or_default()
            }
        };
    }
    if enriched.duration.is_zero() {
        enriched.duration = Duration::from_millis(u64::from(item.duration_ms));
    }
    if enriched.art_url.is_none() {
        // Match the Web API's preference: the 300px variant when available,
        // otherwise the largest cover Spotify lists for the track.
        enriched.art_url = item
            .covers
            .iter()
            .find(|cover| cover.width == 300)
            .or_else(|| item.covers.iter().max_by_key(|cover| cover.width))
            .map(|cover| cover.url.clone());
    }
    enriched
}

/// Save a track's cover image next to its audio. The bytes are decoded once to
/// reject HTML error pages before they become a "cover" that breaks rendering.
async fn save_cover(client: &reqwest::Client, uri: &str, url: &str, dir: &Path) -> Result<()> {
    let bytes = crate::art::fetch_bytes(client, url).await?;
    crate::art::decode_bytes(&bytes).context("downloaded cover is not an image")?;
    let name = cover_file_name(uri);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating downloads directory {}", dir.display()))?;
    let tmp = dir.join(format!("{name}.part"));
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(err) = std::fs::rename(&tmp, dir.join(&name)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("storing cover {}", name));
    }
    Ok(())
}

/// Fetch cover art and lyrics for a downloaded track. Both are best-effort: a
/// missing cover or an unavailable translation service must never turn a
/// successful audio download into a failure. The whole asset pass is bounded so
/// a stalled endpoint cannot hold up bulk download progress.
pub async fn download_assets_for(
    track: &Track,
    dir: &Path,
    assets: &DownloadAssets,
) -> Option<String> {
    match tokio::time::timeout(
        ASSET_TOTAL_TIMEOUT,
        download_assets_inner(track, dir, assets),
    )
    .await
    {
        Ok(cover) => cover,
        Err(_) => {
            tracing::warn!(
                "Timed out saving optional assets for {}; keeping the audio download",
                track.uri
            );
            None
        }
    }
}

async fn download_assets_inner(
    track: &Track,
    dir: &Path,
    assets: &DownloadAssets,
) -> Option<String> {
    let _permit = asset_semaphore().acquire().await.ok();
    let cover = async {
        let (Some(client), Some(url)) = (&assets.http_client, track.art_url.as_deref()) else {
            return None;
        };
        match save_cover(client, &track.uri, url, dir).await {
            Ok(()) => Some(cover_file_name(&track.uri)),
            Err(err) => {
                tracing::warn!("Could not save cover for {}: {err:#}", track.uri);
                None
            }
        }
    };
    let lyrics = async {
        let (Some(client), Some(path)) = (&assets.http_client, &assets.lyrics_cache_file) else {
            return;
        };
        match crate::lyrics::fetch_for_download(client, track).await {
            Ok((lyrics, translation)) => {
                if let Err(err) = crate::lyrics::save_downloaded(
                    path,
                    &track.uri,
                    lyrics.as_ref(),
                    translation.as_deref(),
                ) {
                    tracing::warn!("Could not save lyrics for {}: {err:#}", track.uri);
                }
            }
            Err(err) => tracing::warn!("Could not fetch lyrics for {}: {err:#}", track.uri),
        }
    };
    let (cover_file, ()) = tokio::join!(cover, lyrics);
    cover_file
}

/// Download a track without cover art or lyrics. Kept for callers that only
/// need the audio (for example tests); the app and CLI use
/// [`download_track_with_assets`].
#[allow(dead_code)]
pub async fn download_track(session: &Session, track: &Track, dir: &Path) -> Result<DownloadEntry> {
    download_track_with_assets(session, track, dir, &DownloadAssets::default()).await
}

/// Download a track through the authenticated librespot session and write a
/// plain (decrypted) audio file into `dir`. Ogg files have Spotify's extra
/// normalisation header removed so generic decoders can read them.
///
/// When `assets` is configured, the track's cover and lyrics (plus an English
/// translation where available) are saved alongside the audio so they work
/// without a network connection.
///
/// The actual copy runs on a blocking thread because `AudioFile::read` waits
/// on the background downloader rather than yielding to tokio. If the best
/// format fails (for example, a key service rejection), lower-quality formats
/// are tried before giving up so one bad variant cannot block offline listening.
pub async fn download_track_with_assets(
    session: &Session,
    track: &Track,
    dir: &Path,
    assets: &DownloadAssets,
) -> Result<DownloadEntry> {
    // Fetch params are installed once at startup (see `configure_audio_fetch`)
    // before any player or decoder can lock in the defaults.
    let uri =
        SpotifyUri::from_uri(&track.uri).map_err(|err| anyhow!("invalid Spotify URI: {err}"))?;
    let requested_id: SpotifyId = (&uri)
        .try_into()
        .map_err(|err| anyhow!("invalid track ID: {err}"))?;
    let item = AudioItem::get_file(session, uri.clone())
        .await
        .context("could not read track metadata for download")?;
    if let Err(reason) = &item.availability {
        let error = anyhow!("track is not available for download: {reason}");
        record_download_failure(dir, &track, &error);
        return Err(error);
    }
    // Metadata may resolve an alternative track ID; that is the ID the key
    // service expects, not necessarily the URI the client started with.
    let track_id: SpotifyId = (&item.track_id).try_into().unwrap_or(requested_id);
    // CLI downloads start with just a URI. Fill in what librespot already knows
    // so the index, cover lookup and LRCLIB query all have real metadata.
    let track = enriched_track(track, &item);

    let mut last_error = None;
    let mut attempted = false;
    for format in PREFERRED_FORMATS {
        let Some(file_id) = item.files.get(&format).copied() else {
            continue;
        };
        attempted = true;
        let mut format_result = None;
        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            match download_format(session, track_id, file_id, format, &track, dir).await {
                Ok(entry) => {
                    format_result = Some(Ok(entry));
                    break;
                }
                Err(err) => {
                    let retryable = download_error_is_transient(&err);
                    if retryable && attempt < DOWNLOAD_ATTEMPTS {
                        tracing::warn!(
                            "Transient download failure for {} as {format:?} (attempt {attempt}/{DOWNLOAD_ATTEMPTS}): {err:#}; retrying",
                            track.uri
                        );
                        sleep(Duration::from_secs(attempt as u64)).await;
                        continue;
                    }
                    format_result = Some(Err(err));
                    break;
                }
            }
        }
        match format_result.expect("download loop always produces a result") {
            Ok(mut entry) => {
                if let Some(cover) = download_assets_for(&track, dir, assets).await {
                    entry.cover_file = Some(cover);
                }
                return Ok(entry);
            }
            Err(err) => {
                tracing::warn!("Could not download {} as {format:?}: {err:#}", track.uri);
                last_error = Some(err.context(format!("{format:?} audio")));
            }
        }
    }

    let error = match last_error {
        Some(err) => err.context("no supported audio format could be downloaded"),
        None if attempted => anyhow!("track lists formats but none could be prepared"),
        None => anyhow!("track has no supported download format"),
    };
    record_download_failure(dir, &track, &error);
    Err(error)
}

/// Current Unix timestamp, used for download index entries.
pub fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "termify-offline-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn entry(uri: &str, extension: &str) -> DownloadEntry {
        DownloadEntry {
            track: Track {
                title: "Song".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                duration: Duration::from_secs(120),
                uri: uri.into(),
                art_url: None,
                added_by: None,
            },
            file: format!("{}.{extension}", file_stem(uri)),
            cover_file: None,
            size: 42,
            downloaded_at: 1,
        }
    }

    #[test]
    fn file_names_are_stable_and_safe() {
        let stem = file_stem("spotify:track:abc");
        assert_eq!(stem, file_stem("spotify:track:abc"));
        assert_ne!(stem, file_stem("spotify:track:abd"));
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(safe_file_name(&format!("{stem}.ogg")));
        assert!(safe_file_name(&format!("{stem}.mp3")));
        assert!(!safe_file_name("../outside.ogg"));
        assert!(!safe_file_name("present.ogg"));
        assert!(!safe_file_name(&format!("{stem}.flac")));

        let cover = cover_file_name("spotify:track:abc");
        assert_eq!(cover, format!("{stem}.jpg"));
        assert!(safe_cover_file_name(&cover));
        assert!(!safe_cover_file_name("../outside.jpg"));
        assert!(!safe_cover_file_name(&format!("{stem}.png")));
        assert!(!safe_cover_file_name("short.jpg"));
    }

    #[test]
    fn store_persists_entries_and_drops_missing_files() {
        let dir = temp_dir("store");
        let mut store = DownloadStore::open(&dir).expect("open store");
        let present = entry("spotify:track:present", "ogg");
        let missing = entry("spotify:track:missing", "ogg");
        std::fs::write(dir.join(&present.file), b"OggS").expect("write audio");
        store.insert(present.clone()).expect("insert present");
        store.insert(missing).expect("insert missing");
        assert_eq!(store.len(), 2);

        let reopened = DownloadStore::open(&dir).expect("reopen store");
        assert!(reopened.contains("spotify:track:present"));
        assert!(!reopened.contains("spotify:track:missing"));
        assert_eq!(
            reopened.audio_path(reopened.get("spotify:track:present").unwrap()),
            dir.join(&present.file)
        );

        store.remove("spotify:track:present").expect("remove");
        assert!(!dir.join(&present.file).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_tracks_and_removes_saved_covers() {
        let dir = temp_dir("covers");
        let mut store = DownloadStore::open(&dir).expect("open store");
        let mut saved = entry("spotify:track:cover", "ogg");
        saved.cover_file = Some(cover_file_name(&saved.track.uri));
        std::fs::write(dir.join(&saved.file), b"OggS").expect("write audio");
        std::fs::write(
            dir.join(saved.cover_file.as_ref().unwrap()),
            b"\xff\xd8\xff",
        )
        .expect("write cover");
        store.insert(saved.clone()).expect("insert");

        let mut reopened = DownloadStore::open(&dir).expect("reopen store");
        assert_eq!(
            reopened
                .cover_path(reopened.get(&saved.track.uri).unwrap())
                .as_deref(),
            Some(dir.join(saved.cover_file.as_ref().unwrap()).as_path())
        );

        reopened.remove(&saved.track.uri).expect("remove");
        assert!(!dir.join(saved.file).exists());
        assert!(!dir.join(saved.cover_file.as_ref().unwrap()).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cover_can_be_attached_after_indexing() {
        let dir = temp_dir("late-cover");
        let mut store = DownloadStore::open(&dir).expect("open store");
        let saved = entry("spotify:track:late", "ogg");
        std::fs::write(dir.join(&saved.file), b"OggS").expect("write audio");
        store.insert(saved.clone()).expect("insert");
        let name = cover_file_name(&saved.track.uri);
        std::fs::write(dir.join(&name), b"\xff\xd8\xff").expect("write cover");
        store
            .set_cover(&saved.track.uri, name.clone())
            .expect("attach cover");
        assert_eq!(
            DownloadStore::open(&dir)
                .expect("reopen")
                .get(&saved.track.uri)
                .and_then(|entry| entry.cover_file.clone()),
            Some(name)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_cover_reference_is_forgotten_without_dropping_audio() {
        let dir = temp_dir("stale-cover");
        std::fs::create_dir_all(&dir).expect("create dir");
        let mut saved = entry("spotify:track:stale", "ogg");
        saved.cover_file = Some(cover_file_name(&saved.track.uri));
        std::fs::write(dir.join(&saved.file), b"OggS").expect("write audio");
        let mut tracks = serde_json::Map::new();
        tracks.insert(
            saved.track.uri.clone(),
            serde_json::to_value(&saved).expect("encode entry"),
        );
        std::fs::write(
            DownloadStore::index_file(&dir),
            serde_json::to_string(&serde_json::json!({"version": 2, "tracks": tracks}))
                .expect("encode index"),
        )
        .expect("write index");

        let store = DownloadStore::open(&dir).expect("open store");
        let entry = store.get(&saved.track.uri).expect("audio kept");
        assert!(entry.cover_file.is_none());
        assert!(store.cover_path(entry).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_ignores_unknown_index_version() {
        let dir = temp_dir("version");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(
            DownloadStore::index_file(&dir),
            r#"{"version":99,"tracks":{}}"#,
        )
        .expect("write index");
        let store = DownloadStore::open(&dir).expect("open store");
        assert!(store.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_ignores_corrupt_index() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(DownloadStore::index_file(&dir), b"not json").expect("write index");
        let store = DownloadStore::open(&dir).expect("open store");
        assert!(store.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_persists_playlists_and_omits_deleted_tracks() {
        let dir = temp_dir("playlists");
        let mut store = DownloadStore::open(&dir).expect("open store");
        let first = entry("spotify:track:first", "ogg");
        let second = entry("spotify:track:second", "mp3");
        std::fs::write(dir.join(&first.file), b"OggS").expect("write first");
        std::fs::write(dir.join(&second.file), b"ID3\x04").expect("write second");
        store.insert(first.clone()).expect("insert first");
        store.insert(second.clone()).expect("insert second");
        store
            .insert_playlist(
                "playlist-1",
                "Road Trip",
                &[
                    first.track.uri.clone(),
                    second.track.uri.clone(),
                    "spotify:track:missing".to_string(),
                ],
            )
            .expect("insert playlist");

        let playlists = DownloadStore::open(&dir).expect("reopen store").playlists();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].name, "Road Trip");
        assert_eq!(playlists[0].track_uris.len(), 2);

        store.remove(&first.track.uri).expect("remove first");
        let reopened = DownloadStore::open(&dir).expect("reopen after delete");
        let tracks = reopened.playlist_tracks("playlist-1");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].uri, second.track.uri);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_persists_albums_and_omits_deleted_tracks() {
        let dir = temp_dir("albums");
        let mut store = DownloadStore::open(&dir).expect("open store");
        let first = entry("spotify:track:album-first", "ogg");
        let second = entry("spotify:track:album-second", "mp3");
        std::fs::write(dir.join(&first.file), b"OggS").expect("write first");
        std::fs::write(dir.join(&second.file), b"ID3\x04").expect("write second");
        store.insert(first.clone()).expect("insert first");
        store.insert(second.clone()).expect("insert second");
        store
            .insert_album(
                "album-1",
                "Greatest Hits",
                "Artist",
                vec![
                    first.track.uri.clone(),
                    first.track.uri.clone(),
                    second.track.uri.clone(),
                    "spotify:track:missing".to_string(),
                ],
            )
            .expect("insert album");

        let reopened = DownloadStore::open(&dir).expect("reopen store");
        let albums = reopened.albums();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].id, "album-1");
        assert_eq!(albums[0].name, "Greatest Hits");
        assert_eq!(albums[0].artist, "Artist");
        // Duplicate and unavailable URIs are not kept in the saved grouping.
        assert_eq!(
            albums[0].track_uris,
            vec![first.track.uri.clone(), second.track.uri.clone()]
        );
        let tracks = reopened.album_tracks("album-1");
        assert_eq!(
            tracks
                .iter()
                .map(|track| track.uri.as_str())
                .collect::<Vec<_>>(),
            vec![first.track.uri.as_str(), second.track.uri.as_str()]
        );

        store.remove(&first.track.uri).expect("remove first");
        let reopened = DownloadStore::open(&dir).expect("reopen after delete");
        assert_eq!(
            reopened.albums()[0].track_uris,
            vec![second.track.uri.clone()]
        );
        let tracks = reopened.album_tracks("album-1");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].uri, second.track.uri);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn album_membership_is_pruned_without_deleting_audio() {
        let dir = temp_dir("album-prune");
        std::fs::create_dir_all(&dir).expect("create dir");
        let saved = entry("spotify:track:album-survivor", "ogg");
        let missing = entry("spotify:track:album-gone", "ogg");
        std::fs::write(dir.join(&saved.file), b"OggS").expect("write survivor");
        let mut tracks = serde_json::Map::new();
        tracks.insert(
            saved.track.uri.clone(),
            serde_json::to_value(&saved).expect("encode saved track"),
        );
        tracks.insert(
            missing.track.uri.clone(),
            serde_json::to_value(&missing).expect("encode missing track"),
        );
        let album = DownloadedAlbum {
            id: "album-2".to_string(),
            name: "Mixed".to_string(),
            artist: "Artist".to_string(),
            track_uris: vec![missing.track.uri.clone(), saved.track.uri.clone()],
            downloaded_at: 1,
        };
        let empty_album = DownloadedAlbum {
            id: "album-empty".to_string(),
            name: "Gone".to_string(),
            artist: String::new(),
            track_uris: vec![missing.track.uri.clone()],
            downloaded_at: 1,
        };
        let mut albums = serde_json::Map::new();
        albums.insert(
            album.id.clone(),
            serde_json::to_value(&album).expect("encode album"),
        );
        albums.insert(
            empty_album.id.clone(),
            serde_json::to_value(&empty_album).expect("encode empty album"),
        );
        let index = serde_json::json!({
            "version": 2,
            "tracks": tracks,
            "albums": albums,
        });
        std::fs::write(
            DownloadStore::index_file(&dir),
            serde_json::to_string(&index).expect("encode index"),
        )
        .expect("write index");

        let store = DownloadStore::open(&dir).expect("open store");
        let albums = store.albums();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].id, "album-2");
        assert_eq!(albums[0].track_uris, vec![saved.track.uri.clone()]);
        assert_eq!(store.album_tracks("album-2").len(), 1);
        assert!(store.album_tracks("album-empty").is_empty());
        // Pruning only changes the index; the surviving audio stays on disk.
        assert!(dir.join(&saved.file).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_index_without_albums_field_loads() {
        let dir = temp_dir("no-albums");
        std::fs::create_dir_all(&dir).expect("create dir");
        let saved = entry("spotify:track:no-albums", "ogg");
        std::fs::write(dir.join(&saved.file), b"OggS").expect("write audio");
        let mut tracks = serde_json::Map::new();
        tracks.insert(
            saved.track.uri.clone(),
            serde_json::to_value(&saved).expect("encode track"),
        );
        std::fs::write(
            DownloadStore::index_file(&dir),
            serde_json::to_string(&serde_json::json!({"version": 2, "tracks": tracks}))
                .expect("encode index"),
        )
        .expect("write index");

        let mut store = DownloadStore::open(&dir).expect("open store");
        assert!(store.contains(&saved.track.uri));
        assert!(store.albums().is_empty());
        // Names, not ids, decide the listing order.
        store
            .insert_album("album-a", "Zebra", "Artist", vec![saved.track.uri.clone()])
            .expect("insert zebra album");
        store
            .insert_album("album-z", "Apple", "Artist", vec![saved.track.uri.clone()])
            .expect("insert apple album");
        assert_eq!(
            store
                .albums()
                .iter()
                .map(|album| album.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Apple", "Zebra"]
        );
        let reopened = DownloadStore::open(&dir).expect("reopen store");
        assert_eq!(reopened.albums().len(), 2);
        assert_eq!(reopened.album_tracks("album-z").len(), 1);
        store.remove_album("album-a").expect("remove album");
        assert_eq!(
            DownloadStore::open(&dir)
                .expect("reopen after removal")
                .albums()
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_are_appended_to_the_download_error_log() {
        let dir = temp_dir("error-log");
        let track = entry("spotify:track:failed", "ogg").track;
        let error = anyhow!("outer failure").context("inner failure");
        record_download_failure(&dir, &track, &error);

        let contents = std::fs::read_to_string(download_error_log(&dir)).expect("read error log");
        assert!(contents.contains("spotify:track:failed"));
        assert!(contents.contains("inner failure"));
        assert!(contents.contains("outer failure"));
        assert!(!contents.contains("access_token"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_download_errors_are_recognised() {
        assert!(download_error_is_transient(&anyhow!(
            "skipping header: Deadline expired before operation could complete"
        )));
        assert!(download_error_is_transient(
            &anyhow!("connection reset").context("writing audio")
        ));
        assert!(!download_error_is_transient(&anyhow!(
            "invalid container header"
        )));
        assert!(!download_error_is_transient(&anyhow!(
            "Spotify refused the audio key"
        )));
    }

    #[test]
    fn permanent_audio_key_errors_are_recognised() {
        assert!(audio_key_error_is_permanent("audio key error 0 1"));
        assert!(audio_key_error_is_permanent(
            "Service unavailable { audio key error }"
        ));
        assert!(audio_key_error_is_permanent("no audio key available"));
        assert!(audio_key_error_is_permanent("invalid_grant"));
        assert!(!audio_key_error_is_permanent("audio key response timeout"));
        assert!(!audio_key_error_is_permanent("channel disconnected"));
    }

    #[test]
    fn decode_check_rejects_random_bytes_that_pass_the_magic_check() {
        let dir = temp_dir("decode-check");
        std::fs::create_dir_all(&dir).expect("create dir");
        let bogus_mp3 = dir.join("bogus.mp3");
        // This passes the 4-byte MP3 frame magic but is not decodable audio.
        std::fs::write(&bogus_mp3, [0xff, 0xfb, 0x90, 0x00]).expect("write bogus mp3");
        assert!(audio_file_header_valid(&bogus_mp3, false));
        assert!(!audio_file_decodes(&bogus_mp3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn header_check_uses_explicit_format_even_for_part_files() {
        let dir = temp_dir("part-header");
        std::fs::create_dir_all(&dir).expect("create dir");
        let ogg_part = dir.join("track.ogg.part");
        std::fs::write(&ogg_part, b"OggS data").expect("write ogg part");
        // The extension is `part`, so inferring the format from the path would
        // apply MP3 rules and reject valid Ogg data.
        assert!(audio_file_header_valid(&ogg_part, true));
        assert!(!audio_file_header_valid(&ogg_part, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn header_check_rejects_files_decrypted_without_a_key() {
        let dir = temp_dir("header");
        std::fs::create_dir_all(&dir).expect("create dir");
        let ogg = dir.join("good.ogg");
        let bad = dir.join("bad.mp3");
        let id3 = dir.join("good.mp3");
        let frame = dir.join("frame.mp3");
        std::fs::write(&ogg, b"OggS data").expect("write ogg");
        std::fs::write(&bad, b"random bytes").expect("write bad mp3");
        std::fs::write(&id3, b"ID3\x04 data").expect("write id3");
        std::fs::write(&frame, [0xff, 0xfb, 0x90, 0x00]).expect("write frame");
        assert!(audio_file_header_valid(&ogg, true));
        assert!(!audio_file_header_valid(&bad, false));
        assert!(audio_file_header_valid(&id3, false));
        assert!(audio_file_header_valid(&frame, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_index_without_playlists_is_upgraded() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).expect("create dir");
        let track = entry("spotify:track:legacy", "ogg");
        std::fs::write(dir.join(&track.file), b"OggS").expect("write audio");
        let mut tracks = serde_json::Map::new();
        tracks.insert(
            track.track.uri.clone(),
            serde_json::to_value(&track).expect("encode track"),
        );
        let index = serde_json::json!({
            "version": 1,
            "tracks": tracks,
        });
        std::fs::write(
            DownloadStore::index_file(&dir),
            serde_json::to_string(&index).expect("encode index"),
        )
        .expect("write index");
        let mut store = DownloadStore::open(&dir).expect("open store");
        assert!(store.contains(&track.track.uri));
        assert!(store.playlists().is_empty());
        assert!(store.albums().is_empty());
        store
            .insert_playlist("playlist-1", "Legacy", &[track.track.uri.clone()])
            .expect("insert playlist");
        let contents =
            std::fs::read_to_string(DownloadStore::index_file(&dir)).expect("read index");
        assert!(contents.contains("\"version\": 2"));
        assert!(contents.contains("\"albums\": {}"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
