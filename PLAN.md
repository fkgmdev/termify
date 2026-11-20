# termify — implementation plan

A from-scratch Rust TUI Spotify client that **plays audio locally** through librespot,
with search, playlists and synced lyrics. Project name: `termify`.

This file is the living copy of the plan; it is updated as milestones are completed
or decisions change.

## Decisions

| Area | Decision |
| --- | --- |
| Playback | Local streaming via librespot 0.8 (no remote control of the official app) |
| Auth | Two tokens: user Client ID + PKCE for Web API; Spotify desktop Client ID + device pairing for local playback |
| Web API | `rspotify` 0.16 (`AuthCodePkceSpotify`, loaded from the PKCE token) |
| UI | `ratatui` 0.30 + `crossterm` 0.29 |
| Runtime | `tokio` (multi-thread), single `mpsc` Action channel into `App` |
| Audio backend | librespot-playback with rodio/ALSA (`SoftMixer` for volume) |
| Lyrics | LRCLIB (line-synced, no key) primary, original-language entry preferred; English translation via Google's public endpoint on demand. Cached in-memory by track URI. Genius unsynced fallback deferred to polish |
| Config/cache | `~/.config/termify/config.toml`, `~/.cache/termify/` (`token.json` for Web API, `playback_token.json` for playback, audio cache) |
| Redirect URI | `http://127.0.0.1:8989/callback` for PKCE; device pairing uses `https://spotify.com/pair` |

Lyrics note: Spotify's Web API has **no** lyrics endpoint. The internal `color-lyrics`
endpoint is deliberately out of scope for v1 (unofficial, needs an account cookie).
English translation uses Google's public translate endpoint (also unofficial, no
key) and is best-effort; failures keep the original lyrics visible.

## Architecture

```
src/
  main.rs            # CLI, tokio runtime, TUI wiring, shutdown
  config.rs          # config.toml (client id, redirect URI, volume, backend)
  auth.rs            # PKCE + device-pairing flows, token caches + refresh
  api.rs             # rspotify wrapper (search, playlists, liked songs)
  player.rs          # librespot Session + Player, playback controls, events
  lyrics.rs          # LRCLIB client, LRC parser, language preference, translation
  ui/
    mod.rs
    app.rs           # App state, Action enum, key handling, async helpers
    event.rs         # crossterm stream -> Action
    render.rs        # ratatui layout + widgets
```

Key events and background tasks (API calls, player events, lyrics fetches) all push
`Action`s into one channel. The main loop owns `App`, applies actions and redraws.
Playback progress is derived from a local clock anchored on a 250 ms tick.

## Milestones

### M0 — Scaffold + go/no-go spike (done)
- [x] Rust toolchain and ALSA dev libs verified
- [x] Plan written to this file
- [x] Cargo project skeleton, dependencies, config model, README
- [x] PKCE auth + token cache implemented; `auth` and `play <uri>` CLI commands wired
- [x] `cargo check --all-targets` and `cargo test` pass (zero warnings)
- [x] Pinned `vergen 9.0.6` to work around upstream `vergen-gitcl` incompatibility
- [x] Spotify app registered by user (Client ID supplied; redirect URI above)
- [x] Spike: authenticate + play one 30s track through librespot
      - Custom-client PKCE with `streaming` scope passed AP login but login5
        rejected the stored credentials (`BAD_REQUEST` with the custom client ID,
        `INVALID_CREDENTIALS` with librespot's desktop ID).
      - Switched to Spotify's documented device-pairing flow with the desktop
        client ID (librespot #1737 comments, PRs #1744/#1745).
      - **Live test passed:** Bohemian Rhapsody loaded and streamed locally via
        rodio/ALSA; no audio-key error. `play` ran until the 20s timeout.
      - Using the desktop Client ID for Web API calls caused persistent `429`.
        Split auth into a user-client PKCE token for Web API and a desktop
        device-pairing token for playback; both verified working.

### M1 — TUI shell (done)
- [x] Layout: header with tabs, list pane, detail pane, now-playing footer
- [x] Keybindings: `j/k`, `g/G`, `PgUp/PgDn`, `Enter`, `/`, `Space`, `n/p`, `←/→`, `+/-`, `l`, `Q`, `J/K`, `L`, `s`, `a`, `t`, `Tab`, `?`, `q`, `Ctrl-C`
      (Tab/Shift+Tab use crossterm's `Tab`/`BackTab` codes)
- [x] Config loading (client ID, redirect URI, volume, audio backend)
- [x] Single `mpsc` Action channel with crossterm event stream + 250 ms tick
- [x] Loading / error states, help overlay
- [x] `--demo` mode for credential-free UI testing (smoke-tested)

### M2 — Library & search (done)
- [x] Search tracks via rspotify (top 10 results; verified live with PKCE token)
- [x] Current user's playlists; load playlist tracks on Enter
- [x] Liked songs (`L`)
- [x] Async API tasks with loading/status feedback in UI

### M3 — Playback integration (done)
- [x] librespot session connected with the device-pairing playback token at TUI startup
- [x] Play selected track, pause/resume, next/prev, seek, volume via SoftMixer
- [x] Player event channel wired: `EndOfTrack` auto-advance, `Playing`/`Paused` sync
- [x] Graceful degradation: browse-only mode if audio session fails
- [x] Validate real playback with user credentials (see M0 spike)

### M4 — Lyrics page (done)
- [x] LRCLIB client with `track_name`/`artist_name`/`album_name`/`duration` lookup
- [x] LRC parser (mm:ss.xx, mm:ss, multiple stamps, metadata tags)
- [x] Active-line highlight (`▶`) following playback position
- [x] Follow mode keeps the active line near the middle of the pane; `j/k`
      select a line (`»`) and disable following, `a` re-enables it
- [x] Jump to selected lyric line: `Enter` seeks playback to that line's
      timestamp and re-enables follow mode; `g/G` and `PgUp/PgDn` also move the
      selection, and the current playback line stays marked with `▶`
- [x] Plain-lyrics fallback, "no lyrics" state, in-memory cache by track URI
- [x] Unit tests for LRC parsing, active-line tracking, scroll offsets, script
      detection, translation parsing, queue reordering and lyric-line seek/key
      handling (14 passing)
- [x] Fixed LRCLIB camelCase field mapping (`syncedLyrics`/`plainLyrics`) that
      made every lookup report "no lyrics"; verified with "Knee Socks"
- [x] Prefer original-language LRCLIB entry when `/api/get` returns a translation
      for a non-Latin title; verified with DECO*27's モニタリング (Japanese first
      line instead of English)
- [x] Optional English translation (`t`) via Google's public translate endpoint,
      batched per 10 lines, cached per track; original remains available by
      toggling back

### M5 — Polish (in progress)
- [ ] Genius unsynced fallback (needs optional free API token)
- [ ] Disk-persisted lyrics cache
- [x] Token refresh persistence verified in practice (expired cache refreshed,
      playback succeeded, new token cached with `0600`)
- [x] Playlist shuffling (`s`): Fisher-Yates playback order, current track first,
      applied to `n/p` and end-of-track auto-advance; next/prev wrap and visit
      every track exactly once per cycle (unit tested)
- [x] Queue tab (`Q`): full-pane view of `play_order` with the current track
      marked; `j/k` move the cursor, `J/K` swap a track with its neighbour,
      `Enter` plays it, and the cursor follows the current track. Reordering is
      reset when shuffle is toggled or a new track list is loaded (unit tested)
- [ ] Repeat mode
- [ ] Integration tests with a mock HTTP layer

## Deferred (not in v1)

Spotify Connect device registration, cover art, daemon mode, media keys, CLI
scripting, audio visualization, desktop notifications.

## User-provided credentials needed

- **Now:** Spotify Client ID from https://developer.spotify.com/dashboard with
  redirect URI `http://127.0.0.1:8989/callback`; approve PKCE in the browser and
  the device-pairing code at https://spotify.com/pair when `termify auth` asks.
- **Optional later:** Genius API token if the unsynced fallback needs the official API.
