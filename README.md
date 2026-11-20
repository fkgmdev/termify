# termify

A terminal Spotify client written in Rust that **plays audio locally** through
librespot — it does not control the official Spotify app. Browse and search your
library, play music, and read synced lyrics, all from the terminal.

> Status: early development (milestones M0–M4 implemented). See [PLAN.md](PLAN.md)
> for the full roadmap and current progress.

## Features (v1 scope)

- Web API via your own Spotify app (Authorization Code + PKCE, Client ID only)
- Local audio streaming via librespot 0.8 (rodio/ALSA on Linux), authenticated
  with Spotify's desktop client ID through the device-pairing flow
- Two separate cached tokens so Web API rate limits do not affect playback
- Search tracks, browse playlists, load playlist tracks and liked songs
- Playback controls: play/pause, next/prev, seek, volume, end-of-track auto-advance
- Playlist shuffling with a saved playback order (`s`)
- Queue tab showing the playback order, with track reordering (`J`/`K`) and
  play-from-queue (`Enter`)
- Synced lyrics via LRCLIB with active-line highlighting and automatic scrolling
- Lyric line selection (`j`/`k`) and jump-to-line seeking (`Enter`), with `a` to
  resume following playback
- Original-language lyrics preferred over translated LRCLIB entries; optional
  English translation (`t`), cached per track
- Ratatui TUI with a single async action loop and `--demo` mode

Not in v1: Spotify Connect, cover art, daemon mode, media keys, repeat mode,
audio visualization.

## Requirements

- Rust 1.81+ (tested with 1.99)
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

2. Configure the Client ID via `SPOTIFY_CLIENT_ID` or in
   `~/.config/termify/config.toml` (created on first run):

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
   - PKCE in your browser for the Web API; token cached at
     `~/.cache/termify/token.json`.
   - Device pairing at https://spotify.com/pair for local playback; token cached
     at `~/.cache/termify/playback_token.json`.

   Both caches are written with `0600` permissions.

4. Launch the TUI:

   ```bash
   cargo run
   ```

## CLI

```
termify                 # launch the TUI (requires auth first)
termify --demo          # TUI with sample data, no credentials needed
termify auth            # force a fresh Web API login (reuse playback token)
termify search <query>  # search tracks and print results
termify play <uri>      # smoke test: play one track locally until Ctrl-C
termify --relogin       # force re-login for both Web API and playback
```

## Keybindings

| Key | Action |
| --- | --- |
| `j` / `↓` | Move down (select lyric line on Lyrics tab; move cursor on Queue tab) |
| `k` / `↑` | Move up (select lyric line on Lyrics tab; move cursor on Queue tab) |
| `g` / `G` | First / last item (first/last lyric line on Lyrics tab) |
| `PgUp` / `PgDn` | Page up / down |
| `Enter` | Select track to play; open playlist; play queue track; jump to selected lyric line |
| `/` | Search tracks |
| `Tab` / `Shift+Tab` | Next / previous tab |
| `Space` | Play / pause |
| `n` / `p` | Next / previous track |
| `←` / `→` | Seek −5s / +5s |
| `+` / `-` | Volume up / down |
| `l` | Lyrics tab |
| `Q` | Queue tab |
| `J` / `K` | Move selected queue track down / up |
| `L` | Load liked songs |
| `s` | Shuffle on / off (affects next/prev and auto-advance) |
| `a` | Lyrics follow playback on / off (re-syncs the selected line when re-enabled) |
| `t` | Translate lyrics to English (on the Lyrics tab) |
| `?` | Help overlay |
| `q` / `Ctrl-C` | Quit |

The Queue tab shows the order used by `n`/`p` and auto-advance. `J`/`K` swap the
selected track with its neighbour and the cursor follows it; `Enter` starts that
track. Toggling shuffle or loading a new track list rebuilds the order, so manual
reordering is reset in those cases.

On the Lyrics tab, `j`/`k` (and `g`/`G`, `PgUp`/`PgDn`) select a line with `»`,
which disables follow mode. `Enter` seeks playback to that line's timestamp and
re-enables following; `a` manually re-syncs the selection to the line currently
playing. The active playback line stays marked with `▶`.

## Architecture

Single binary, modular source:

```
src/
  main.rs     # CLI + tokio runtime, TUI wiring, shutdown
  config.rs   # config.toml, paths, env overrides
  auth.rs     # PKCE + device-pairing flows, token caches + refresh
  api.rs      # rspotify wrapper (search, playlists, liked songs)
  player.rs   # librespot session, Player, mixer, playback controls
  lyrics.rs   # LRCLIB client, LRC parser, language preference, translation
  ui/
    app.rs    # App state, Action enum, key handling, queue/shuffle/lyric-seek/translation
    event.rs  # crossterm stream + tick -> Action channel
    render.rs # ratatui layout and widgets
```

Events from the keyboard and background tasks (API calls, player events, lyrics)
all flow into one `mpsc` channel. `App` owns all state, applies actions and redraws.
Playback progress is derived from a local clock anchored on a 250 ms tick.

## Notes / troubleshooting

- **`Bad credentials` / login5 `INVALID_CREDENTIALS`**: since Spotify's 2026-08
  backend change, third-party client IDs can use the Web API but cannot obtain
  the private credentials librespot needs for audio keys. `termify` therefore
  keeps two tokens: your Client ID (PKCE) for the Web API, and Spotify's desktop
  client ID (device pairing) for local playback. If you have an old single token,
  re-run `termify auth`.
- **`429 API rate limit exceeded`**: the Web API uses your own Client ID to avoid
  sharing Spotify's heavily rate-limited desktop client. If it still happens,
  wait for `Retry-After` and retry; the TUI status bar now shows this clearly.
- **Audio key error 0 1**: some newer Spotify accounts are rejected for audio key
  delivery. This has been validated as working for the development account with
  the device-pairing flow; `termify play spotify:track:<id>` remains the quick
  check for other accounts.
- **vergen pin**: `librespot-core 0.8.0` depends on `vergen-gitcl 1.0.8`, which
  resolves to an incompatible `vergen 9.1.0`. `Cargo.lock` pins `vergen 9.0.6`.
  Do not delete `Cargo.lock` without re-applying `cargo update -p vergen --precise 9.0.6`.
- **Token file permissions**: both token caches are written with mode `0600`.
- **Lyrics**: LRCLIB is a free third-party service with no API key; availability
  may vary. If its exact-match lookup returns a translated entry for a non-Latin
  track, termify searches LRCLIB for the original-language entry (verified with
  DECO*27's モニタリング). Genius fallback is planned but not implemented yet.
- **Translation**: English translation uses Google's public translate endpoint
  (unofficial, no API key) and is cached per track for the session. If the
  service is unavailable the status bar shows the error and original lyrics stay
  visible.
