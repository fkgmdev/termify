# termify

A terminal Spotify client written in Rust that **plays audio locally** through
librespot — it does not control the official Spotify app. Browse and search your
library, play music, and read synced lyrics, all from the terminal.

> Status: early development (milestones M0–M4 implemented, M5 polish underway).
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
- Playlist shuffling with a saved playback order (`s`)
- Queue tab showing the playback order, with track reordering (`J`/`K`) and
  play-from-queue (`Enter`)
- Synced lyrics via LRCLIB with active-line highlighting and automatic scrolling
- Lyric line selection (`j`/`k`) and jump-to-line seeking (`Enter`), with `a` to
  resume following playback
- Original-language lyrics preferred over translated LRCLIB entries; optional
  English translation (`t`), cached per track
- Ratatui TUI with a single async action loop and `--demo` mode
- Album cover art in the Details pane using Kitty/iTerm2/Sixel when available,
  with Unicode halfblocks and text placeholders as fallbacks; the same URL is
  published as MPRIS `mpris:artUrl` for desktop players
- MPRIS integration: the current track appears in the desktop media player, and
  global XF86 audio keys (play/pause, next, previous, volume, seek) work
  anywhere on the desktop through the window manager's MPRIS media-key routing

Not in v1: Spotify Connect, daemon mode, repeat mode, audio visualization.

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

2. Configure the Client ID via `SPOTIFY_CLIENT_ID` or in `config.toml` (created
   on first run). The config and cache roots are platform-specific:

   | OS | Config file | Cache directory |
   | --- | --- | --- |
   | Linux | `$XDG_CONFIG_HOME/termify/config.toml` (default `~/.config/termify/config.toml`) | `$XDG_CACHE_HOME/termify` (default `~/.cache/termify`) |
   | macOS | `~/Library/Application Support/termify/config.toml` | `~/Library/Caches/termify` |
   | Windows | `%APPDATA%\termify\config.toml` | `%LOCALAPPDATA%\termify\cache` |

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
| `J` / `K` | Move selected queue track down / up (Queue tab only) |
| `L` | Load liked songs |
| `N` | Create a new private playlist (Playlists tab) |
| `R` | Rename the selected playlist (staged when it is currently open) |
| `A` | Add the selected track to a playlist (staged when the target is open) |
| `X` | Remove the selected track from the currently open playlist (staged) |
| `J` / `K` | Move the selected track down / up in the currently open playlist (staged) |
| `[` / `]` | Same as `J` / `K` for playlist reordering |
| `S` | Save all staged playlist changes in one batch |
| `D` | Discard staged playlist changes |
| `s` | Shuffle on / off (affects next/prev and auto-advance) |
| `c` | Show / hide cover art (no effect when the terminal blocks images) |
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

When a track has album art, termify downloads the 300px Spotify image in the
background and draws it above the track metadata in the Details pane. It uses
the best terminal graphics protocol available (Kitty, iTerm2 or Sixel) and falls
back to Unicode halfblocks when the terminal has no graphics protocol. Fetch,
decode and protocol errors never interrupt playback: the pane simply shows a
text placeholder such as `Loading cover art...` or `Cover art unavailable`.

Set `TERMIFY_NO_ART=1` (or use `TERM=dumb`) to disable art entirely and reclaim
the space for metadata. `c` toggles art at runtime; the same album-art URL is
also published to the desktop through MPRIS.

## Desktop media player / global media keys

termify exports the standard [MPRIS](https://specifications.freedesktop.org/mpris-spec/latest/)
D-Bus interfaces (`org.mpris.MediaPlayer2` and `.Player`) under a per-process
bus name such as `org.mpris.MediaPlayer2.termify.instance1234`. As soon as a
track is selected, your desktop's media widget and OSD show its title, artist,
album, duration and playback position/state.

Media-key handling is delegated to the desktop environment: GNOME, KDE Plasma,
XFCE and friends already bind `XF86AudioPlay`, `XF86AudioNext`,
`XF86AudioPrev`, `XF86AudioRaiseVolume` and friends to the active MPRIS player.
No global key grab is needed, so termify does not fight your window manager.
Players like `playerctl` work too:

```
playerctl metadata
playerctl play-pause
playerctl next
playerctl position 30
playerctl volume 0.5
```

MPRIS is silently disabled when `DBUS_SESSION_BUS_ADDRESS` is unset (for
example over a plain SSH session); the TUI keeps working normally.

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
  lyrics.rs   # LRCLIB client, LRC parser, language preference, translation
  mpris.rs    # MPRIS D-Bus player: publishes state, forwards media-key controls
  ui/
    app.rs    # App state, Action enum, key handling, queue/shuffle/lyric-seek/translation
    event.rs  # crossterm stream + tick -> Action channel
    render.rs # ratatui layout and widgets
```

Events from the keyboard, MPRIS and background tasks (API calls, player events,
lyrics, cover art) all flow into one `mpsc` channel. `App` owns all state, applies actions
and redraws. After each action it publishes a state snapshot to the MPRIS thread,
which runs on its own current-thread Tokio runtime because `mpris-server`'s
`Player` is `Rc`-based and not `Send`. Playback progress is derived from a local
clock anchored on a 250 ms tick.

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
  (unofficial, no API key) and is cached per track for the session. If the
  service is unavailable the status bar shows the error and original lyrics stay
  visible.
- **Cover art**: if your terminal cannot show images you will see Unicode
  halfblocks or a text placeholder instead; nothing is written to disk. Set
  `TERMIFY_NO_ART=1` to skip terminal probing and disable art completely. A
  missing or failed art fetch only affects the placeholder text.
