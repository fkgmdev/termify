mod api;
mod art;
mod auth;
mod config;
mod discord;
mod lyrics;
mod mpris;
mod offline;
mod os_media;
mod player;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;

use crate::config::{Config, Paths};

#[derive(Debug, Parser)]
#[command(name = "termify", version, about = "A terminal Spotify client with local playback")]
struct Cli {
    /// Force a fresh Spotify login even if a cached token exists.
    #[arg(long)]
    relogin: bool,

    /// Launch the TUI with sample data (no Spotify credentials needed).
    #[arg(long)]
    demo: bool,

    /// Play only downloaded tracks, with no network or Spotify login.
    #[arg(long)]
    offline: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Authenticate with Spotify and cache the token.
    Auth,
    /// Streaming smoke test: play one track locally until Ctrl-C.
    Play { uri: String },
    /// Search tracks and print the results (useful for diagnosing API errors).
    Search { query: String },
    /// Download one track and print the full error chain if it fails.
    Download { uri: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    // Must run before any librespot player/decoder initialises the process-wide
    // AudioFetchParams with its 8-second default timeout.
    offline::configure_audio_fetch();
    let paths = Paths::discover()?;

    // Demo and offline modes bypass config validation so the TUI shell can be
    // exercised without credentials or a network connection.
    let config = if cli.demo || cli.offline {
        Config::default()
    } else {
        config::load(&paths)?
    };

    match cli.command {
        Some(Command::Auth) => {
            auth::authenticated(&config, &paths, true)
                .await
                .context("authenticating with Spotify Web API")?;
            println!("Web API token cached at {}", paths.token_file.display());
            auth::playback_access_token(&paths, false)
                .await
                .context("authenticating for local playback")?;
            println!(
                "Playback token cached at {}",
                paths.playback_token_file.display()
            );
        }
        Some(Command::Play { uri }) => {
            let access_token = auth::playback_access_token(&paths, cli.relogin)
                .await
                .context("authenticating for local playback")?;

            println!("Connecting to Spotify for local playback...");
            let local = player::LocalPlayer::connect(&access_token, &config, &paths).await?;
            local.play_uri(&uri)?;
            println!("Playing {uri} locally. Press Ctrl-C to stop.");
            tokio::signal::ctrl_c().await?;
        }
        Some(Command::Download { uri }) => {
            let access_token = auth::playback_access_token(&paths, cli.relogin)
                .await
                .context("authenticating for local playback")?;
            let local = player::LocalPlayer::connect(&access_token, &config, &paths)
                .await
                .context("connecting to Spotify for local playback")?;
            let mut downloads = offline::DownloadStore::open(&paths.downloads_dir)?;
            let track = crate::ui::Track {
                title: uri.clone(),
                artist: String::new(),
                album: String::new(),
                duration: std::time::Duration::ZERO,
                uri: uri.clone(),
                art_url: None,
                added_by: None,
            };
            let assets =
                offline::DownloadAssets::new(Some(paths.lyrics_cache_file.clone()));
            match offline::download_track_with_assets(&local.session, &track, downloads.dir(), &assets)
                .await
            {
                Ok(entry) => {
                    let size = entry.size;
                    downloads.insert(entry)?;
                    println!(
                        "Downloaded {uri} ({size} bytes) to {}",
                        downloads.dir().display()
                    );
                }
                Err(err) => {
                    eprintln!("Download failed for {uri}: {err:#}");
                    eprintln!(
                        "See {}",
                        offline::download_error_log(downloads.dir()).display()
                    );
                    std::process::exit(1);
                }
            }
        }
        Some(Command::Search { query }) => {
            let spotify = auth::authenticated(&config, &paths, cli.relogin)
                .await
                .context("authenticating with Spotify Web API")?;
            let tracks = api::search_tracks(&spotify, &query).await?;
            if tracks.is_empty() {
                println!("No tracks found for '{query}'");
            } else {
                for track in tracks {
                    println!("{} - {} - {}", track.uri, track.title, track.artist);
                }
            }
        }
        None => {
            if cli.demo {
                run_tui(None, None, &paths, &config, false, false).await?;
            } else if cli.offline {
                run_tui(None, None, &paths, &config, false, true).await?;
            } else {
                let spotify = auth::authenticated(&config, &paths, cli.relogin)
                    .await
                    .context("authenticating with Spotify Web API")?;
                let access_token = auth::playback_access_token(&paths, cli.relogin)
                    .await
                    .context("authenticating for local playback")?;

                let player = match player::LocalPlayer::connect(&access_token, &config, &paths).await
                {
                    Ok(p) => Some(p),
                    Err(err) => {
                        tracing::warn!("Local playback unavailable: {err}");
                        eprintln!("Warning: local playback unavailable ({err}); browsing still works.");
                        None
                    }
                };
                run_tui(Some(spotify), player, &paths, &config, false, false).await?;
            }
        }
    }

    Ok(())
}

async fn run_tui(
    spotify: Option<rspotify::AuthCodePkceSpotify>,
    local_player: Option<player::LocalPlayer>,
    paths: &config::Paths,
    config: &config::Config,
    demo: bool,
    offline: bool,
) -> Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        is_raw_mode_enabled,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui_image::picker::Picker;
    use std::io::IsTerminal;
    use std::time::Duration;

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Probe the terminal for graphics support after entering the alternate
    // screen but before starting the crossterm event stream, as ratatui-image
    // briefly writes to and reads from stdio while querying capabilities.
    let art_blocked_reason = if std::env::var_os("TERMIFY_NO_ART").is_some() {
        Some("TERMIFY_NO_ART is set".to_string())
    } else if std::env::var("TERM").as_deref() == Ok("dumb") {
        Some("TERM=dumb".to_string())
    } else {
        None
    };
    let can_query = art_blocked_reason.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let picker = if can_query {
        Picker::from_query_stdio().unwrap_or_else(|err| {
            tracing::warn!("Terminal image query failed, using halfblocks: {err}");
            Picker::halfblocks()
        })
    } else {
        Picker::halfblocks()
    };

    // `Picker::from_query_stdio` enables raw mode while probing and signals
    // completion from a background thread *before* that thread disables raw
    // mode again. Re-enabling immediately can therefore be undone a moment
    // later, leaving Windows Terminal / Kitty in cooked mode where ↑/↓ are
    // swallowed. Wait for the probe thread to finish restoring the terminal.
    for _ in 0..10 {
        if !is_raw_mode_enabled()? {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    enable_raw_mode()?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let terminal_size = terminal
        .size()
        .unwrap_or(ratatui::layout::Size::new(100, 30));

    let (tx, mut rx) = mpsc::channel(64);
    let downloads = offline::DownloadStore::open(&paths.downloads_dir)?;
    let mut app = ui::App::new_with_options(
        tx.clone(),
        picker,
        art_blocked_reason,
        ui::AppOptions {
            lyrics_cache_file: Some(paths.lyrics_cache_file.clone()),
            downloads: Some(downloads),
        },
    );
    app.set_terminal_size(terminal_size.width, terminal_size.height);

    if let Some(client) = spotify {
        app.api_client = Some(std::sync::Arc::new(client));
        app.status = "Signed in — loading your playlists...".to_string();
        app.load_playlists();
    } else if offline {
        app.set_offline_mode();
    } else if demo {
        app.load_demo();
    }

    if let Some(player) = local_player {
        let player = std::sync::Arc::new(player);
        app.player = Some(player.clone());

        let tx_player = tx.clone();
        tokio::spawn(async move {
            use librespot_playback::player::PlayerEvent;
            let mut events = player.event_channel();
            while let Some(event) = events.recv().await {
                let action = match event {
                    PlayerEvent::EndOfTrack { .. } => Some(ui::Action::PlayerEndOfTrack),
                    PlayerEvent::Playing { position_ms, .. } => {
                        Some(ui::Action::PlayerPlaying(position_ms))
                    }
                    PlayerEvent::Paused { position_ms, .. } => {
                        Some(ui::Action::PlayerPaused(position_ms))
                    }
                    PlayerEvent::Unavailable { .. } => Some(ui::Action::Error(
                        "Track unavailable for playback".to_string(),
                    )),
                    _ => None,
                };
                if let Some(action) = action {
                    let _ = tx_player.send(action).await;
                }
            }
        });
    }

    let event_task = tokio::spawn(ui::event::run(tx.clone()));

    // Publish state to the desktop media controls and route global media keys
    // back into the same action channel: MPRIS on Linux/BSD, Now Playing on
    // macOS, SMTC on Windows. An unavailable backend simply disables this.
    let media_controls: Option<os_media::MediaControlsHandle> = match os_media::spawn(tx.clone()) {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!("Could not start native media controls: {err:#}");
            None
        }
    };
    if let Some(handle) = &media_controls {
        handle.publish(app.mpris_state());
    }

    // Discord Rich Presence mirrors the same snapshot. It is optional: without
    // an application ID or a running Discord client this is a no-op.
    let discord = match discord::spawn(config) {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!("Could not start Discord Rich Presence: {err:#}");
            None
        }
    };
    if let Some(handle) = &discord {
        handle.publish(app.mpris_state());
    }

    terminal.draw(|frame| ui::render::render(frame, &app))?;

    let result = async {
        while app.running {
            if let Some(action) = rx.recv().await {
                app.handle_action(action)?;
                if let Some(handle) = &media_controls {
                    handle.publish(app.mpris_state());
                }
                if let Some(handle) = &discord {
                    handle.publish(app.mpris_state());
                }
                terminal.draw(|frame| ui::render::render(frame, &app))?;
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    event_task.abort();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}
