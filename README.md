# termify

A terminal Spotify client written in Rust that **plays audio locally** through
librespot — it does not control the official Spotify app. Browse and search your
library, play music, and read synced lyrics, all from the terminal.

> Status: early development (M0–M6 implemented; M5 polish and M6 live verification underway).
> See [PLAN.md](PLAN.md) for the full roadmap and current progress.

## Features (v1 scope)

- Web API via your own Spotify app (Authorization Code + PKCE, Client ID only)
- Local audio streaming via librespot 0.8 (rodio/ALSA on Linux), authenticated
  with Spotify's desktop client ID through the device-pairing flow
- Two separate cached tokens so Web API rate limits do not affect playback
- Search tracks, browse playlists, load playlist tracks and liked songs
- Playlist editing: create/rename private playlists, add tracks from search,
  remove tracks, and reorder tracks; re-login is required once for the new
  `playlist-modify-private`/`playlist-modify-public` scopes
- Playlist details: owner, public/collaborative status, contributors and the
  user who added each track (from Spotify's per-item `added_by` field)
- Playback controls: play/pause, next/prev, seek, volume, end-of-track auto-advance
- Repeat modes (`r`): off, playlist and one. Playlist repeat restarts the whole
  playlist when its last track ends, using the current shuffle order, and is
  exposed as MPRIS `Loop_Status` on Linux
- Offline downloads: save tracks through the librespot session, list them on the
  Offline tab, play single tracks or a whole downloaded playlist with rodio, and
  launch with `--offline` without a login
- Press `d` on the Playlists tab to download every track of a playlist; already
  saved and in-flight tracks are skipped, with progress shown in the list
- Downloaded copies are preferred automatically, so selecting a saved track from
  search or a playlist also plays from disk; delete copies with `D`
- Persistent v2 `downloads.json` index with hashed filenames, playlist
  memberships, atomic writes and stale-entry cleanup; audio-key failures are
  retried with backoff and bad files are never indexed
- Playlist shuffling with a saved playback order (`s`)
- Queue tab showing the playback order, with track reordering (`J`/`K`) and
  play-from-queue (`Enter`)
- Synced lyrics via LRCLIB with active-line highlighting and automatic scrolling
- Lyric line selection (`j`/`k`) and jump-to-line seeking (`Enter`), with `a` to
  resume following playback
- Original-language lyrics preferred over translated LRCLIB entries; optional
  English translation (`t`), cached per track
- Downloads also save cover art, lyrics and an English translation when
  available, so downloaded tracks keep their artwork and translated lyrics in
  `--offline` mode
- Lyrics cached in memory and in `lyrics.json`, including known misses, so a
  restart does not repeat LRCLIB requests
- Ratatui TUI with a single async action loop and `--demo` mode
- Current-song cover art that follows you everywhere: an 8x4 thumbnail in the
  Now Playing footer on every tab, plus a large adaptive cover in the Lyrics,
  Queue and Details panes (up to 44x22 cells). Kitty/iTerm2/Sixel are used when
  available, with Unicode halfblocks and text placeholders as fallbacks;
  resizing rebuilds the image without a refetch. Album covers are also published
  as MPRIS `mpris:artUrl`, and the Playlists tab keeps its own selected-playlist
  cover in the Details pane
- Native desktop media controls: MPRIS on Linux/BSD, Now Playing and remote
  commands on macOS, and System Media Transport Controls on Windows. Play/pause,
  next/previous, seek and volume events route into the same action loop as the
  keyboard

Not in v1: Spotify Connect, daemon mode, audio visualization.

## Requirements

- Rust 1.85+ (tested with 1.99; librespot 0.8 uses edition 2024)
- A **Spotify Premium** account
- ALSA development headers and pkg-config (Linux):
  - Debian/Ubuntu: `sudo apt install libasound2-dev pkg-config`
  - Fedora: `sudo dnf install alsa-lib-devel pkgconf-pkg-config`
- A Spotify developer app (Client ID + one loopback redirect URI)
- A browser to approve PKCE login and a device to approve pairing at https://spotify.com/pair

## Setup

1. Create an app at https://developer.spotify.com/dashboard and add this exact
   redirect URI:

   ```
   http://127.0.0.1:8989/callback
   ```

2. Configure the Client ID via `SPOTIFY_CLIENT_ID` or in `config.toml` (created
   on first run). The config and cache roots are platform-specific:

   | OS | Config file | Cache directory |
   | --- | --- | --- |
   | Linux | `$XDG_CONFIG_HOME/termify/config.toml` (default `~/.config/termify/config.toml`) | `$XDG_CACHE_HOME/termify` (default `~/.cache/termify`) |
   | macOS | `~/Library/Application Support/termify/config.toml` | `~/Library/Caches/termify` |
   | Windows | `%APPDATA%\termify\config.toml` | `%LOCALAPPDATA%\termify` |

   ```toml
   client_id = "your-client-id-here"
   redirect_uri = "http://127.0.0.1:8989/callback"
   volume = 60
   audio_backend = "rodio"
   ```

3. Authenticate:

   ```bash
   cargo run -- auth
   ```

   This runs two flows:
   - PKCE in your browser for the Web API; token cached as `token.json` in the
     cache directory above.
   - Device pairing at https://spotify.com/pair for local playback; token cached
     as `playback_token.json` in the same directory.

   Both caches are written with `0600` permissions.

4. Launch the TUI:

   ```bash
   cargo run
   ```

## CLI

```
termify                 # launch the TUI (requires auth first)
termify --demo          # TUI with sample data, no credentials needed
termify --offline       # play only downloaded tracks, no login or network
termify auth            # force a fresh Web API login (reuse playback token)
termify search <query>  # search tracks and print results
termify play <uri>      # smoke test: play one track locally until Ctrl-C
termify download <uri>  # save one track and print the full error chain on failure
termify --relogin       # force re-login for both Web API and playback
```

## Keybindings

| Key | Action |
| --- | --- |
| `j` / `↓` | Move down (select playlist on Playlists tab; lyric line on Lyrics tab; cursor on Queue tab) |
| `k` / `↑` | Move up (select playlist on Playlists tab; lyric line on Lyrics tab; cursor on Queue tab) |
| `g` / `G` | First / last item (first/last lyric line on Lyrics tab) |
| `PgUp` / `PgDn` | Page up / down |
| `Enter` | Select track to play (offline copy when saved); open playlist; play a whole offline playlist/all saved tracks; play queue item; jump to selected lyric line |
| `/` | Search tracks |
| `Tab` / `Shift+Tab` | Next / previous tab |
| `Space` | Play / pause |
| `n` / `p` | Next / previous track |
| `←` / `→` | Seek −5s / +5s |
| `+` / `-` | Volume up / down |
| `l` | Lyrics tab |
| `Q` | Queue tab |
| `o` | Offline / downloads tab |
| `d` | Download the selected track; download all tracks on the Playlists tab |
| `D` | On the Offline tab: delete the selected track's copy, or forget the selected offline playlist (files kept); elsewhere: discard staged playlist changes |
| `J` / `K` | Move selected queue track down / up (Queue tab only) |
| `L` | Load liked songs |
| `N` | Create a new private playlist (Playlists tab) |
| `R` | Rename the selected playlist (staged when it is currently open) |
| `A` | Add the selected track to a playlist (staged when the target is open) |
| `X` | Remove the selected track from the currently open playlist (staged) |
| `J` / `K` | Move the selected track down / up in the currently open playlist (staged) |
| `[` / `]` | Same as `J` / `K` for playlist reordering |
| `S` | Save all staged playlist changes in one batch |
| `s` | Shuffle on / off (affects next/prev and auto-advance) |
| `r` | Repeat off / playlist / one; playlist repeat restarts the shuffled order too (MPRIS Loop Status) |
| `c` | Show / hide cover art (no effect when the terminal blocks images) |
| `a` | Lyrics follow playback on / off (re-syncs the selected line when re-enabled) |
| `t` | Translate lyrics to English (on the Lyrics tab) |
| `?` | Help overlay |
| `q` / `Ctrl-C` | Quit |

The Queue tab shows the order used by `n`/`p` and auto-advance. `J`/`K` swap the
selected track with its neighbour and the cursor follows it; `Enter` starts that
track. Toggling shuffle or loading a new track list rebuilds the order, so manual
reordering is reset in those cases.

`r` cycles repeat through off → playlist → one. While a playlist is open,
playlist repeat explicitly restarts that playlist from the first entry of the
current playback order when the last track ends. With shuffle enabled that means
the whole shuffled cycle repeats; other track lists simply wrap from the last
track to the first. Repeat-one reloads the current track from the beginning, and
with repeat off the player stops at the end of the queue. Manual `n`/`p` still
wrap regardless of the repeat mode. The current mode also appears in the footer
and is the MPRIS `Loop_Status` property on Linux.

On the Lyrics tab, `j`/`k` (and `g`/`G`, `PgUp`/`PgDn`) select a line with `»`,
which disables follow mode. `Enter` seeks playback to that line's timestamp and
re-enables following; `a` manually re-syncs the selection to the line currently
playing. The active playback line stays marked with `▶`.

Fetched lyrics are stored in `lyrics.json` in the cache directory listed under
Setup. Both successful results and tracks with no lyrics are cached; a corrupt
or outdated cache is ignored and rewritten after the next successful lookup.
English translations are cached with the lyrics they belong to. Downloading a
track also writes its lyrics and translation to this file, so `t` works without
a network connection for saved tracks; tracks fetched online without a download
still regenerate their translation when the app restarts.

## Offline listening

### Where downloads are saved

Downloads live in the `downloads/` folder under the cache directory from the
Setup table:

- Linux: `$XDG_CACHE_HOME/termify/downloads` (default `~/.cache/termify/downloads`)
- macOS: `~/Library/Caches/termify/downloads`
- Windows: `%LOCALAPPDATA%\termify\downloads`

The v2 index is `downloads.json` inside that `downloads/` folder
(`~/.cache/termify/downloads/downloads.json`, for example). It maps Spotify URIs
to hashed `.ogg`/`.mp3` filenames, an optional matching `.jpg` cover file, and
records which playlists each track belongs to. Lyrics and translations live in
`lyrics.json` beside the `downloads/` folder, shared with online playback.
Writes are atomic, names are validated when the index is read, and stale entries
(missing files or invalid audio headers) disappear on the next launch without
touching your other audio files.

### What quality they are

Online streaming uses librespot's default premium bitrate: **Ogg Vorbis at
160 kbps**, decoded to 16-bit PCM for the audio device. Downloads request the
best file Spotify offers for each track, in this order:
`OGG_VORBIS_320` → `MP3_320` → `MP3_256` → `OGG_VORBIS_160` → `MP3_160` →
`OGG_VORBIS_96` → `MP3_96`. They are stored as-is after decryption (Spotify's
extra 167-byte Ogg header is stripped); termify does not transcode or lower the
quality. A track only gets 320 kbps if Spotify has that format for it.

### Is it safe to delete them?

Yes. Downloaded files are **not DRM-encrypted** — they are ordinary Ogg Vorbis
or MP3 audio files, with a plain image file for the saved cover. Use `D` on the
Offline tab to remove the selected track's audio, cover and index entry together;
on a downloaded-playlist row, `D` only forgets the playlist grouping and keeps
the files. Lyrics entries in `lyrics.json` are shared with online playback and
are not deleted with a track. You can also delete files manually: the next launch
drops index entries whose audio is gone or whose headers are invalid, and forgets
missing covers. No tokens or credentials are stored in `downloads/`; token caches
stay in the parent cache directory with `0600` permissions, so protect the whole
cache directory as you would any media library.

### Downloading and playing offline

When local playback is connected, select any track and press `d`. termify fetches
the audio through the authenticated librespot session, decrypts it and writes a
`.part` file that is renamed only after a complete copy. Cover art and lyrics are
then fetched in a separate background task — with short network timeouts — so a
slow CDN or translation service never stalls playlist progress. Those extras are
best-effort, so a missing cover or a translation-service outage never fails the
audio download. Audio-key requests are
spaced out and retried with backoff on transient failures such as
`Service unavailable` / rate limits; permanent key refusals are reported
immediately instead of being retried. termify never assumes a failed key means
the file is unencrypted. The key is requested before the CDN stream is opened so
failed attempts cannot leave a download task running, and each file is checked
with the same decoder used for playback before it is indexed — not just a magic
header. This prevents the old "skipping junk" / corrupt audio playback. The
librespot CDN timeout is raised from its 8-second default to 120 seconds at
startup, and transient timeouts retry the whole download up to three times before
a lower-quality format is tried.

If the preferred format fails, termify now tries the remaining supported
formats before giving up, so a key rejection or bad variant for one file does
not necessarily block the track. Every failed attempt is appended to
`download-errors.log` beside `downloads.json`, and the Offline tab shows a short
"Recent download errors" section. For a one-off diagnosis outside the TUI, run
`termify download <uri>`; it prints the full error chain and the log path.
The log contains track names/URIs and error messages only — never tokens.

On the Playlists tab, `d` fetches the selected playlist's saved tracks and
downloads all of them sequentially, with a 400 ms pause between tracks to avoid
key-service bursts. Tracks already on disk or in flight are skipped, duplicates
within the playlist are collapsed, and the list shows `offline n/total` progress
while it runs. The final status reports how many tracks were saved and how many
failed; a failed track does not stop the rest of the playlist.

Press `o` (or `Tab`) to open the Offline tab. Downloaded playlists appear first,
then an **All downloaded tracks** row, then individual tracks. `Enter` on a
playlist plays its saved tracks in playlist order; `Enter` on **All downloaded
tracks** plays the whole library; `Enter` on a track plays that copy. `n`/`p`,
shuffle and repeat work on the loaded offline queue, so playlist repeat keeps
cycling after the last track. Saved tracks have a `↓` marker in other lists and
are preferred automatically, even when selected from search results or a
playlist. Downloads require a working local-playback session (the same
device-pairing token used for streaming); browsing without one still works, but
cannot fetch new audio.

`termify --offline` skips config validation, Web API login and the playback
session, then starts directly in the Offline tab. It uses rodio to play only
saved files, so it needs an audio device but no network connection. Saved
lyrics, translations and cover art for downloaded tracks are loaded from disk;
tracks without saved copies fall back to text placeholders. API browsing stays
unavailable without a login; desktop media controls continue to work with the
local player.

## Playlists and collaborators

Opening a playlist with `Enter` shows its owner, visibility, collaborative state
and the distinct contributors inferred from the tracks' `added_by` field; each
track in the Details pane also shows who added it. Spotify's Web API no longer
exposes a full collaborator list, so the contributor list is built from those
per-track attributions (older items may be attributed to the owner or omitted).

Spotify does not always set the `collaborative` flag even when several people
have added tracks. termify treats more than one distinct contributor as a shared
playlist and labels the Details pane `Collaborative: Yes (inferred)`. Anonymous
user IDs are never shown: names come from `display_name` when available, the
signed-in user is shown as `You`, and other anonymous IDs become
`Collaborator 1`, `Collaborator 2`, and so on.

On the Playlists tab, `N` creates a private playlist immediately. `R` renames
the selected playlist, but when that playlist is currently open the rename is
staged instead. With tracks loaded, `A` opens a playlist picker: choosing the
open playlist stages the addition (attributed to `You`), while other playlists
are updated immediately. `X` removes the selected track from the currently open
playlist, and `J`/`K` or `[`/`]` move it toward the end/start with the cursor
following it. Renaming, adding, removing and reordering the open playlist are
all local until you press `S`; `D` discards them. Reopening a playlist with
staged edits restores the local draft.

`S` sends one batch: the rename only when the name actually changed, one remove
call for changed URIs, one add call for added URIs, and the minimal sequence of
reorder calls needed for the final order. Repeated keyboard reorders therefore
cost one request at save time instead of one per keystroke, and retained tracks
keep their original `added_by` attribution.

Playlist edits need the `playlist-modify-private` and `playlist-modify-public`
scopes. Cached Web API tokens without them are rejected at startup and termify
reopens the browser login (refresh tokens cannot add scopes). Run
`termify auth` to force this once after upgrading.

## Cover art

Termify keeps the current track's album art visible wherever you are. A small
8x4-cell thumbnail sits in the Now Playing footer on every tab, so the cover is
never lost while browsing playlists or downloads. The full-size cover appears in
the Details pane, and the Lyrics and Queue tabs give it a dedicated area beside
the text (or above it on narrow terminals). Press `l` for lyrics: the same album
cover stays on screen next to the synced lines.

The image is the 300px Spotify art, fetched in the background. Downloaded
tracks use the cover saved next to their audio instead, which also works with
`--offline`. The large cover
grows with the terminal up to 44x22 cells (about 88x22 pixels at the usual 1:2
cell ratio) while always leaving enough rows for the surrounding text; on small
terminals it stays compact or steps aside entirely. Resizing re-encodes the
already-downloaded image at the new size, including the footer thumbnail. It
uses the best terminal graphics protocol available (Kitty, iTerm2 or Sixel) and
falls back to Unicode halfblocks when the terminal has no graphics protocol.
Fetch, decode and protocol errors never interrupt playback: the art area simply
shows a text placeholder such as `Loading cover art...` or `Cover art
unavailable`.

The Playlists tab still shows the selected playlist's own image in the Details
pane. That cover is kept separate from the current song's art, so moving through
playlist rows never replaces the thumbnail in the footer or the cover on the
Lyrics tab. Opening a playlist switches to its tracks as before.

Set `TERMIFY_NO_ART=1` (or use `TERM=dumb`) to disable all art areas and reclaim
the space for text. `c` toggles art at runtime; the same album-art URL is also
published to the desktop through MPRIS.

## Desktop media player / global media keys

termify publishes the current track and accepts media-control events on every
supported desktop:

- **Linux/BSD:** the standard [MPRIS](https://specifications.freedesktop.org/mpris-spec/latest/)
  D-Bus interfaces (`org.mpris.MediaPlayer2` and `.Player`) under a per-process
  name such as `org.mpris.MediaPlayer2.termify.instance1234`. GNOME, KDE Plasma,
  XFCE and friends route `XF86AudioPlay`, `XF86AudioNext`, `XF86AudioPrev` and
  friends to the active MPRIS player; no global key grab is needed. `playerctl`
  works too:

  ```
  playerctl metadata
  playerctl play-pause
  playerctl next
  playerctl position 30
  playerctl volume 0.5
  ```

  MPRIS is silently disabled when `DBUS_SESSION_BUS_ADDRESS` is unset (for
  example over a plain SSH session); the TUI keeps working normally.
- **macOS:** track metadata, artwork and playback state go to Now Playing, and
  Control Center / Touch Bar / keyboard media keys are handled through
  `MPRemoteCommandCenter`.
- **Windows:** title, artist, album, artwork and progress go to System Media
  Transport Controls, so the volume mixer and keyboard media keys see termify
  like any other media app. SMTC needs a window handle, so termify creates a
  tiny hidden Win32 message window on the media thread; nothing is displayed.

In all three cases, play/pause, next/previous, seek/set-position, volume and
open-URI events are translated into the same `Action`s used by the keyboard, so
they also work with downloaded tracks in `--offline` mode. Shuffle is
publishable/controllable on macOS and exposed as MPRIS `Loop_Status` on Linux.

## Architecture

Single binary, modular source:

```
src/
  main.rs     # CLI + tokio runtime, TUI wiring, shutdown
  config.rs   # config.toml, platform-specific paths, env overrides
  art.rs      # cover-art state, fetch/decode, terminal protocol helpers
  auth.rs     # PKCE + device-pairing flows, token caches + refresh
  api.rs      # rspotify wrapper (search, playlists, liked songs)
  player.rs   # librespot session, Player, mixer, playback controls
  offline.rs  # download/decrypt audio, v2 disk index, rodio playback for saved tracks
  lyrics.rs   # LRCLIB client, LRC parser, language preference, translation, disk cache
  mpris.rs    # MPRIS D-Bus player and shared media-state/types
  os_media.rs # platform dispatcher: MPRIS on Linux, Now Playing/SMTC on macOS/Windows
  ui/
    app.rs    # App state, Action enum, keys, queue/shuffle/repeat/lyrics/cover/downloads
    event.rs  # crossterm stream + tick -> Action channel
    render.rs # ratatui layout and widgets
```

Events from the keyboard, native media controls and background tasks (API calls,
player events, downloads, lyrics, cover art) all flow into one `mpsc` channel.
`App` owns all state, applies actions and redraws. After each action it publishes
a state snapshot to the active media thread: MPRIS runs on its own current-thread
Tokio runtime because `mpris-server`'s `Player` is `Rc`-based and not `Send`,
while macOS/Windows use `souvlaki` on a dedicated thread (with a hidden Win32
message window on Windows). Online progress is derived from a local clock anchored
on a 250 ms tick; offline progress comes from the rodio sink.

## Notes / troubleshooting

- **`Bad credentials` / login5 `INVALID_CREDENTIALS`**: since Spotify's 2026-08
  backend change, third-party client IDs can use the Web API but cannot obtain
  the private credentials librespot needs for audio keys. `termify` therefore
  keeps two tokens: your Client ID (PKCE) for the Web API, and Spotify's desktop
  client ID (device pairing) for local playback. If you have an old single token,
  re-run `termify auth`.
- **`http error: status code 400 Bad Request` at startup**: a cached token can no
  longer be refreshed, usually because it was issued to a different Client ID or
  redirect URI. termify now ignores that cache and starts a fresh login. If it
  repeats, confirm your Client ID and that the redirect URI is exactly
  `http://127.0.0.1:8989/callback`, then run `termify --relogin`.
- **`429 API rate limit exceeded`**: the Web API uses your own Client ID to avoid
  sharing Spotify's heavily rate-limited desktop client. If it still happens,
  wait for `Retry-After` and retry; the TUI status bar now shows this clearly.
- **Audio key errors (`0 1`, `0 2`, `Service unavailable`, `no audio key`)**: a
  missing key means the downloaded bytes are still encrypted, so termify no
  longer writes them. Transient failures are spaced out and retried with
  exponential backoff; permanent refusals (`audio key error 0 1`, `invalid_grant`)
  are reported immediately because retrying cannot help. Files are verified with
  the playback decoder before indexing, and termify falls back to lower-quality
  formats. If every track fails, check that the account is Premium and that
  `termify play spotify:track:<id>` streams normally; a 2026 Spotify DRM change
  means accounts created after a cutoff cannot obtain keys for librespot at all
  (a known upstream limitation with no workaround). The desktop pairing token
  must be refreshed with `termify --relogin` after switching accounts. Run
  `termify download spotify:track:<id>` to print the exact failing stage.
- **vergen pin**: `librespot-core 0.8.0` depends on `vergen-gitcl 1.0.8`, which
  resolves to an incompatible `vergen 9.1.0`. `Cargo.lock` pins `vergen 9.0.6`.
  Do not delete `Cargo.lock` without re-applying `cargo update -p vergen --precise 9.0.6`.
- **Token file permissions**: both token caches are written with mode `0600`.
- **Offline downloads**: saved audio is plain, unencrypted Ogg/MP3 in the cache
  directory's `downloads/` folder (see "Where downloads are saved" above), with
  an optional plain `.jpg` cover fetched from the track metadata. A
  failed download leaves no `.part` file, and an unreadable or outdated
  `downloads.json` is ignored rather than deleting audio. Failed attempts are
  appended to `download-errors.log` in the same folder and summarized in the
  Offline tab; share that log (it contains no tokens) when reporting a problem.
  If `termify --offline` reports no tracks, check that folder; deleting its files
  also removes the corresponding entries on the next launch.
- **Arrow keys in Kitty / Windows Terminal**: the cover-art probe signals
  completion before its background thread has restored the terminal, so termify
  waits for raw mode to be disabled before re-entering it. It also reassembles
  `ESC [ A/B/C/D` fragments, so `↑`/`↓` navigate lists and lyrics in Kitty,
  Windows Terminal and ConPTY hosts without VT input processing.
- **Lyrics**: LRCLIB is a free third-party service with no API key; availability
  may vary. If its exact-match lookup returns a translated entry for a non-Latin
  track, termify searches LRCLIB for the original-language entry (verified with
  DECO*27's モニタリング). Genius fallback is planned but not implemented yet.
- **Translation**: English translation uses Google's public translate endpoint
  (unofficial, no API key). It is cached per track in `lyrics.json`, including
  for downloaded tracks so it works offline. If the service is unavailable the
  status bar shows the error and original lyrics stay visible.
- **Cover art**: if your terminal cannot show images you will see Unicode
  halfblocks or a text placeholder instead; terminal protocols are not written
  to disk, but downloads do save a plain `.jpg` cover. Set `TERMIFY_NO_ART=1` to
  skip terminal probing and disable art completely. A missing or failed art
  fetch only affects the placeholder text.
