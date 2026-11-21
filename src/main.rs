mod api;
mod art;
mod auth;
mod config;
mod lyrics;
mod mpris;
mod player;
mod ui;

use anyhow::Result;
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
    let paths = Paths::discover()?;

    // Demo mode bypasses config validation so the TUI shell can be exercised
    // before credentials are configured.
    let config = if cli.demo {
        Config::default()
    } else {
        config::load(&paths)?
    };

    match cli.command {
        Some(Command::Auth) => {
            auth::authenticated(&config, &paths, true).await?;
            println!("Web API token cached at {}", paths.token_file.display());
            auth::playback_access_token(&paths, false).await?;
            println!(
                "Playback token cached at {}",
                paths.playback_token_file.display()
            );
        }
        Some(Command::Play { uri }) => {
            let access_token = auth::playback_access_token(&paths, cli.relogin).await?;

            println!("Connecting to Spotify for local playback...");
            let local = player::LocalPlayer::connect(&access_token, &config, &paths).await?;
            local.play_uri(&uri)?;
            println!("Playing {uri} locally. Press Ctrl-C to stop.");
            tokio::signal::ctrl_c().await?;
        }
        Some(Command::Search { query }) => {
            let spotify = auth::authenticated(&config, &paths, cli.relogin).await?;
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
                run_tui(None, None).await?;
            } else {
                let spotify = auth::authenticated(&config, &paths, cli.relogin).await?;
                let access_token = auth::playback_access_token(&paths, cli.relogin).await?;

                let player = match player::LocalPlayer::connect(&access_token, &config, &paths).await
                {
                    Ok(p) => Some(p),
                    Err(err) => {
                        tracing::warn!("Local playback unavailable: {err}");
                        eprintln!("Warning: local playback unavailable ({err}); browsing still works.");
                        None
                    }
                };
                run_tui(Some(spotify), player).await?;
            }
        }
    }

    Ok(())
}

async fn run_tui(
    spotify: Option<rspotify::AuthCodePkceSpotify>,
    local_player: Option<player::LocalPlayer>,
) -> Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui_image::picker::Picker;
    use std::io::IsTerminal;

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

    // `Picker::from_query_stdio` enables raw mode while probing and then
    // disables it before returning. On Windows that restores processed input,
    // so Windows Terminal swallows ↑/↓ (and other navigation keys) instead of
    // delivering them to crossterm. Re-enter raw mode after the probe.
    enable_raw_mode()?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::channel(64);
    let mut app = ui::App::new(tx.clone(), picker, art_blocked_reason);

    if let Some(client) = spotify {
        app.api_client = Some(std::sync::Arc::new(client));
        app.status = "Signed in — loading your playlists...".to_string();
        app.load_playlists();
    } else {
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

    // Publish state to the desktop (MPRIS) and route global media keys back
    // into the same action channel. Absent D-Bus simply disables this.
    let mpris = match mpris::spawn(tx.clone()) {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!("Could not start MPRIS server: {err:#}");
            None
        }
    };
    if let Some(handle) = &mpris {
        handle.publish(app.mpris_state());
    }

    terminal.draw(|frame| ui::render::render(frame, &app))?;

    let result = async {
        while app.running {
            if let Some(action) = rx.recv().await {
                app.handle_action(action)?;
                if let Some(handle) = &mpris {
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
