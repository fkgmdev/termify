use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Widget};
use ratatui_image::protocol::Protocol;

use super::app::{App, RepeatMode, Tab, format_duration, lyric_scroll_offset};
use crate::art::{self, CoverArt};

const CYAN: Color = Color::Cyan;
const YELLOW: Color = Color::Yellow;
const GRAY: Color = Color::Gray;
const DARK_GRAY: Color = Color::DarkGray;
const WHITE: Color = Color::White;
const RED: Color = Color::Red;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(art::HEADER_ROWS),
            Constraint::Min(5),
            Constraint::Length(art::FOOTER_ROWS),
        ])
        .split(frame.area());

    render_header(frame, chunks[0], app);
    render_main(frame, chunks[1], app);
    render_footer(frame, chunks[2], app);

    if app.show_help {
        render_help(frame, frame.area());
    }
    if app.playlist_picker.is_some() {
        render_playlist_picker(frame, frame.area(), app);
    }
    if app.prompt.is_some() {
        render_prompt(frame, frame.area(), app);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line<'static>> = Tab::ALL.iter().map(|tab| Line::from(tab.title())).collect();

    let selected = match app.tab {
        Tab::Search => 0,
        Tab::Albums => 1,
        Tab::Artists => 2,
        Tab::Playlists => 3,
        Tab::Queue => 4,
        Tab::Lyrics => 5,
        Tab::Downloads => 6,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" termify ", Style::default().fg(CYAN).bold()))
        .title_bottom(Line::from(" local Spotify client ").right_aligned());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(inner);

    let tabs = Tabs::new(titles)
        .select(selected)
        .highlight_style(Style::default().fg(YELLOW).add_modifier(Modifier::BOLD))
        .divider(" | ");
    frame.render_widget(tabs, header_chunks[0]);

    let status = Paragraph::new(app.status.clone())
        .style(Style::default().fg(GRAY))
        .alignment(Alignment::Right);
    frame.render_widget(status, header_chunks[1]);
}

fn render_main(frame: &mut Frame, area: Rect, app: &App) {
    match app.tab {
        Tab::Lyrics => {
            render_lyrics_pane(frame, area, app);
            return;
        }
        Tab::Queue => {
            render_queue_pane(frame, area, app);
            return;
        }
        _ => {}
    }

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_list_pane(frame, main_chunks[0], app);
    render_detail_pane(frame, main_chunks[1], app);
}

fn render_queue_pane(frame: &mut Frame, area: Rect, app: &App) {
    let mode = if app.shuffle { " (shuffled)" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Queue — {} tracks{mode} ", app.tracks.len()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Keep the current song's cover visible while browsing the queue too.
    let (art_area, text_area) = current_art_layout(frame, inner, app, 28, 4);
    if let Some(art_area) = art_area {
        render_cover(frame, art_area, &app.art, app.art_enabled);
    }

    let items: Vec<ListItem> = if app.play_order.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            " Queue is empty — search for tracks or load a playlist ",
            Style::default().fg(DARK_GRAY).italic(),
        )))]
    } else {
        app.play_order
            .iter()
            .enumerate()
            .map(|(position, &index)| {
                let Some(track) = app.tracks.get(index) else {
                    return ListItem::new(Line::from(" (missing track)"));
                };
                let is_current = app
                    .now_playing
                    .as_ref()
                    .is_some_and(|np| np.track.uri == track.uri);
                let style = if is_current {
                    Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(WHITE)
                };
                let marker = if is_current { "▶ " } else { "  " };
                ListItem::new(Line::from(Span::styled(
                    format!("{:>3}. {marker}{}", position + 1, track.display()),
                    style,
                )))
            })
            .collect()
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(text_area);

    let mut state = ListState::default();
    if !app.play_order.is_empty() {
        state.select(Some(
            app.queue_cursor.min(app.play_order.len().saturating_sub(1)),
        ));
    }
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, chunks[0], &mut state);

    let hint = Span::styled(
        " j/k move cursor · J/K move track up/down · Enter play · s shuffle ",
        Style::default().fg(DARK_GRAY),
    );
    frame.render_widget(Paragraph::new(Line::from(hint)), chunks[1]);
}

fn render_lyrics_pane(frame: &mut Frame, area: Rect, app: &App) {
    let uri = app.now_playing.as_ref().map(|np| np.track.uri.as_str());
    let lyrics = uri.and_then(|uri| app.lyrics.get(uri));
    let track_title = app
        .now_playing
        .as_ref()
        .map(|np| np.track.title.clone())
        .unwrap_or_default();
    let mut title = match (&app.now_playing, lyrics) {
        (Some(np), Some(lyrics)) => format!(
            " Lyrics — {} ({}{}) ",
            np.track.title,
            lyrics.source,
            if lyrics.synced { "" } else { ", unsynced" }
        ),
        (Some(np), None) => format!(" Lyrics — {} ", np.track.title),
        _ => " Lyrics ".to_string(),
    };
    if app.translation_loading || app.translate_lyrics || !app.lyrics_auto_scroll {
        let mut flags = Vec::new();
        if app.translation_loading {
            flags.push("translating...");
        }
        if app.translate_lyrics {
            flags.push("translated");
        }
        if !app.lyrics_auto_scroll {
            flags.push("line select");
        }
        title = format!(" Lyrics — {track_title} [{}] ", flags.join(", "));
    }

    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Lyrics get their own large current-track cover, beside the text when the
    // terminal is wide enough and above it on narrower screens.
    let (art_area, text_area) = current_art_layout(frame, inner, app, 24, 4);
    if let Some(art_area) = art_area {
        render_cover(frame, art_area, &app.art, app.art_enabled);
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(text_area);
    let height = chunks[0].height as usize;

    let lines: Vec<Line> = if app.lyrics_loading {
        vec![Line::from(Span::styled(
            " Loading lyrics...",
            Style::default().fg(DARK_GRAY).italic(),
        ))]
    } else if let Some(lyrics) = lyrics {
        let position = app
            .now_playing
            .as_ref()
            .map(|np| np.position)
            .unwrap_or_default();
        let active = crate::lyrics::active_line(lyrics, position);
        let cursor = app.lyric_cursor.min(lyrics.lines.len().saturating_sub(1));
        let offset = lyric_scroll_offset(
            active,
            Some(cursor),
            lyrics.lines.len(),
            app.lyrics_auto_scroll,
            height,
        );
        let translated = if app.translate_lyrics {
            uri.and_then(|uri| app.translations.get(uri))
        } else {
            None
        };

        lyrics
            .lines
            .iter()
            .skip(offset)
            .take(height)
            .enumerate()
            .map(|(i, line)| {
                let absolute = offset + i;
                let is_cursor = Some(absolute) == Some(cursor);
                let style = if is_cursor {
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
                } else if Some(absolute) == active {
                    Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)
                } else if active.is_some_and(|a| absolute < a) {
                    Style::default().fg(DARK_GRAY)
                } else {
                    Style::default().fg(WHITE)
                };
                let prefix = if is_cursor {
                    "» "
                } else if Some(absolute) == active {
                    "▶ "
                } else {
                    "  "
                };
                let text = translated
                    .and_then(|lines| lines.get(absolute))
                    .map(String::as_str)
                    .unwrap_or(line.text.as_str());
                Line::from(Span::styled(format!("{prefix}{text}"), style))
            })
            .collect()
    } else {
        vec![
            Line::from(Span::styled(
                " No lyrics for this track.",
                Style::default().fg(DARK_GRAY).italic(),
            )),
            Line::from(Span::styled(
                " Lyrics are fetched from LRCLIB when you select a track.",
                Style::default().fg(DARK_GRAY).italic(),
            )),
        ]
    };

    frame.render_widget(Paragraph::new(lines), chunks[0]);
    let hint = Span::styled(
        " j/k select line · Enter jump to line · a follow playback ",
        Style::default().fg(DARK_GRAY),
    );
    frame.render_widget(Paragraph::new(Line::from(hint)), chunks[1]);
}

fn render_list_pane(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app.tab {
        Tab::Search => {
            if app.search_active {
                format!(" Search: {}_ ", app.search_query)
            } else if app.search_query.is_empty() {
                " Search (press /) ".to_string()
            } else {
                format!(" Search: {} ", app.search_query)
            }
        }
        Tab::Albums | Tab::Artists => {
            let (label, kind) = match app.tab {
                Tab::Albums => ("Albums", "Album"),
                _ => ("Artists", "Artist"),
            };
            if app.search_active {
                format!(" {kind} search: {}_ ", app.search_query)
            } else if app.search_query.is_empty() {
                format!(" {label} ")
            } else {
                format!(" {kind} search: {} ", app.search_query)
            }
        }
        Tab::Playlists => " Playlists ".to_string(),
        Tab::Queue => " Queue ".to_string(),
        Tab::Lyrics => " Lyrics ".to_string(),
        Tab::Downloads => format!(
            " Offline — {} tracks, {} playlists ",
            app.downloads.len(),
            app.offline_items
                .iter()
                .filter(|item| matches!(item, super::app::OfflineItem::Playlist { .. }))
                .count()
        ),
    };

    let items: Vec<ListItem> = if app.tab == Tab::Playlists {
        if app.playlists.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                " No playlists found ",
                Style::default().fg(DARK_GRAY).italic(),
            )))]
        } else {
            app.playlists
                .iter()
                .map(|playlist| {
                    let label = match app.playlist_downloads.get(&playlist.id) {
                        Some(progress) if progress.total > 0 => format!(
                            "{}  (offline {}/{})",
                            playlist.display(),
                            progress.completed,
                            progress.total
                        ),
                        Some(_) => format!("{}  (fetching offline tracks)", playlist.display()),
                        None => playlist.display(),
                    };
                    ListItem::new(Line::from(Span::styled(label, Style::default().fg(WHITE))))
                })
                .collect()
        }
    } else if app.tab == Tab::Albums {
        if app.albums.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                " No albums — press / to search ",
                Style::default().fg(DARK_GRAY).italic(),
            )))]
        } else {
            app.albums
                .iter()
                .map(|album| {
                    let label = match app.album_downloads.get(&album.id) {
                        Some(progress) if progress.total > 0 => format!(
                            "{} — {}  (offline {}/{})",
                            album.name, album.artist, progress.completed, progress.total
                        ),
                        Some(_) => format!(
                            "{} — {}  (fetching offline tracks)",
                            album.name, album.artist
                        ),
                        None => format!("{} — {}", album.name, album.artist),
                    };
                    ListItem::new(Line::from(Span::styled(label, Style::default().fg(WHITE))))
                })
                .collect()
        }
    } else if app.tab == Tab::Artists {
        if app.artists.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                " No artists — press / to search ",
                Style::default().fg(DARK_GRAY).italic(),
            )))]
        } else {
            app.artists
                .iter()
                .map(|artist| {
                    let label = format!("{}  [{}]", artist.name, short_genres(&artist.genres));
                    ListItem::new(Line::from(Span::styled(label, Style::default().fg(WHITE))))
                })
                .collect()
        }
    } else if app.tab == Tab::Downloads {
        if app.offline_items.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                " No downloads — press d on a track, playlist, or album ",
                Style::default().fg(DARK_GRAY).italic(),
            )))]
        } else {
            app.offline_items
                .iter()
                .map(|item| {
                    let (label, style) = match item {
                        super::app::OfflineItem::Album { name, count, .. } => (
                            format!("▸ {name}  [album · {count} tracks]  Enter to play"),
                            Style::default().fg(CYAN),
                        ),
                        super::app::OfflineItem::Playlist { name, count, .. } => (
                            format!("▸ {name}  [{count} tracks]  Enter to play"),
                            Style::default().fg(CYAN),
                        ),
                        super::app::OfflineItem::AllTracks => (
                            format!("▶ All downloaded tracks  [{}]", app.downloads.len()),
                            Style::default().fg(YELLOW),
                        ),
                        super::app::OfflineItem::Track { uri } => {
                            let track = app.downloads.get(uri).map(|entry| &entry.track);
                            let is_current = app
                                .now_playing
                                .as_ref()
                                .is_some_and(|np| &np.track.uri == uri);
                            let label = track.map_or_else(
                                || format!("  {uri}"),
                                |track| format!("  {}", track.display()),
                            );
                            let style = if is_current {
                                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(WHITE)
                            };
                            (label, style)
                        }
                    };
                    ListItem::new(Line::from(Span::styled(label, style)))
                })
                .collect()
        }
    } else if app.tracks.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            " No results — press / to search ",
            Style::default().fg(DARK_GRAY).italic(),
        )))]
    } else {
        app.tracks
            .iter()
            .map(|track| {
                let is_current = app
                    .now_playing
                    .as_ref()
                    .is_some_and(|np| np.track.uri == track.uri);
                let style = if is_current {
                    Style::default().fg(YELLOW)
                } else if app.downloads.contains(&track.uri) {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(WHITE)
                };
                let marker = if app.downloads.contains(&track.uri) {
                    "↓ "
                } else {
                    "  "
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{marker}{}", track.display()),
                    style,
                )))
            })
            .collect()
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut app.list_state.clone());
}

/// Two genres plus an overflow count keep artist rows compact.
fn short_genres(genres: &[String]) -> String {
    if genres.is_empty() {
        return "—".to_string();
    }
    let summary = genres
        .iter()
        .take(2)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if genres.len() > 2 {
        format!("{summary} +{}", genres.len() - 2)
    } else {
        summary
    }
}

fn render_detail_pane(frame: &mut Frame, area: Rect, app: &App) {
    let title = if app.playlist_is_dirty() {
        " Details — unsaved playlist changes "
    } else {
        " Details "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Cover grows with the terminal (up to 44x22 cells) while always leaving
    // enough rows for metadata and hints. Disabled art reclaims the space.
    let art_height = if app.art_enabled {
        art::cover_size(frame.area()).height.min(inner.height)
    } else {
        0
    };
    let (art_area, text_area) = if art_height > 0 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(art_height), Constraint::Min(1)])
            .split(inner);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, inner)
    };

    if let Some(art_area) = art_area {
        let pane_art = match app.tab {
            Tab::Playlists => &app.playlist_art,
            Tab::Albums => &app.album_art,
            Tab::Artists => &app.artist_art,
            _ => &app.art,
        };
        render_cover(frame, art_area, pane_art, app.art_enabled);
    }

    let lines = if app.tab == Tab::Playlists {
        match app.list_state.selected().and_then(|i| app.playlists.get(i)) {
            Some(playlist) => {
                let visibility = match playlist.is_public {
                    Some(true) => "Public",
                    Some(false) => "Private",
                    None => "Unknown",
                };
                let collaboration = playlist.collaboration_label();
                let contributors = if playlist.contributors.is_empty() {
                    "—".to_string()
                } else {
                    playlist.contributors.join(", ")
                };
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled("Name:         ", Style::default().fg(DARK_GRAY)),
                        Span::styled(playlist.name.clone(), Style::default().fg(WHITE).bold()),
                    ]),
                    Line::from(vec![
                        Span::styled("Owner:        ", Style::default().fg(DARK_GRAY)),
                        Span::styled(
                            playlist.owner.clone().unwrap_or_else(|| "—".to_string()),
                            Style::default().fg(WHITE),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled("Visibility:   ", Style::default().fg(DARK_GRAY)),
                        Span::styled(visibility, Style::default().fg(WHITE)),
                        Span::raw("   "),
                        Span::styled("Collaborative: ", Style::default().fg(DARK_GRAY)),
                        Span::styled(collaboration, Style::default().fg(WHITE)),
                    ]),
                    Line::from(vec![
                        Span::styled("Tracks:       ", Style::default().fg(DARK_GRAY)),
                        Span::styled(
                            playlist
                                .track_count
                                .map(|count| count.to_string())
                                .unwrap_or_else(|| "—".to_string()),
                            Style::default().fg(WHITE),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled("Contributors: ", Style::default().fg(DARK_GRAY)),
                        Span::styled(contributors, Style::default().fg(GRAY)),
                    ]),
                ];
                if !playlist.description.trim().is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        &playlist.description,
                        Style::default().fg(GRAY),
                    )));
                }
                lines.push(Line::from(""));
                let hint = if app.playlist_is_dirty() {
                    "Enter load tracks · d download all · R rename · S save · D discard"
                } else {
                    "Enter load tracks · d download all · N new · R rename"
                };
                lines.push(Line::from(Span::styled(
                    hint,
                    Style::default().fg(DARK_GRAY).italic(),
                )));
                lines
            }
            None => vec![Line::from(Span::styled(
                "Select a playlist to see details",
                Style::default().fg(DARK_GRAY).italic(),
            ))],
        }
    } else if app.tab == Tab::Downloads {
        let selected = app
            .list_state
            .selected()
            .and_then(|i| app.offline_items.get(i))
            .cloned();
        let mut lines = match selected {
            Some(super::app::OfflineItem::Album { name, count, .. }) => {
                let active = app
                    .offline_queue
                    .as_ref()
                    .is_some_and(|queue| queue.name() == name);
                vec![
                    Line::from(vec![
                        Span::styled("Album:    ", Style::default().fg(DARK_GRAY)),
                        Span::styled(name, Style::default().fg(WHITE).bold()),
                    ]),
                    Line::from(vec![
                        Span::styled("Tracks:   ", Style::default().fg(DARK_GRAY)),
                        Span::styled(count.to_string(), Style::default().fg(WHITE)),
                        Span::raw("   "),
                        Span::styled("Loaded:   ", Style::default().fg(DARK_GRAY)),
                        Span::styled(
                            if active { "yes" } else { "no" },
                            Style::default().fg(if active { YELLOW } else { GRAY }),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Enter play whole album · j/k browse · n/p skip · r repeat",
                        Style::default().fg(DARK_GRAY).italic(),
                    )),
                ]
            }
            Some(super::app::OfflineItem::Playlist { name, count, .. }) => {
                let active = app
                    .offline_queue
                    .as_ref()
                    .is_some_and(|queue| queue.name() == name);
                vec![
                    Line::from(vec![
                        Span::styled("Playlist: ", Style::default().fg(DARK_GRAY)),
                        Span::styled(name, Style::default().fg(WHITE).bold()),
                    ]),
                    Line::from(vec![
                        Span::styled("Tracks:   ", Style::default().fg(DARK_GRAY)),
                        Span::styled(count.to_string(), Style::default().fg(WHITE)),
                        Span::raw("   "),
                        Span::styled("Loaded:   ", Style::default().fg(DARK_GRAY)),
                        Span::styled(
                            if active { "yes" } else { "no" },
                            Style::default().fg(if active { YELLOW } else { GRAY }),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Enter play whole playlist · j/k browse · n/p skip · r repeat",
                        Style::default().fg(DARK_GRAY).italic(),
                    )),
                ]
            }
            Some(super::app::OfflineItem::AllTracks) => vec![
                Line::from(Span::styled(
                    "All downloaded tracks",
                    Style::default().fg(WHITE).bold(),
                )),
                Line::from(vec![
                    Span::styled("Tracks: ", Style::default().fg(DARK_GRAY)),
                    Span::styled(app.downloads.len().to_string(), Style::default().fg(WHITE)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter play all offline tracks · n/p skip · r repeat",
                    Style::default().fg(DARK_GRAY).italic(),
                )),
            ],
            Some(super::app::OfflineItem::Track { uri }) => {
                if let Some(download) = app.downloads.get(&uri) {
                    let track = &download.track;
                    let size = download.size as f64 / (1024.0 * 1024.0);
                    let mode = if app.playback_mode == super::app::PlaybackMode::Offline {
                        "offline"
                    } else {
                        "online"
                    };
                    vec![
                        Line::from(vec![
                            Span::styled("Title:    ", Style::default().fg(DARK_GRAY)),
                            Span::styled(track.title.clone(), Style::default().fg(WHITE).bold()),
                        ]),
                        Line::from(vec![
                            Span::styled("Artist:   ", Style::default().fg(DARK_GRAY)),
                            Span::styled(track.artist.clone(), Style::default().fg(WHITE)),
                        ]),
                        Line::from(vec![
                            Span::styled("Length:   ", Style::default().fg(DARK_GRAY)),
                            Span::styled(
                                format_duration(track.duration),
                                Style::default().fg(WHITE),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled("Size:     ", Style::default().fg(DARK_GRAY)),
                            Span::styled(format!("{size:.1} MB"), Style::default().fg(GRAY)),
                            Span::raw("   "),
                            Span::styled("Playback: ", Style::default().fg(DARK_GRAY)),
                            Span::styled(mode, Style::default().fg(GRAY)),
                        ]),
                        Line::from(""),
                        Line::from(Span::styled(
                            "Enter play · D delete offline copy · --offline starts without network",
                            Style::default().fg(DARK_GRAY).italic(),
                        )),
                    ]
                } else {
                    vec![Line::from(Span::styled(
                        "This download is no longer available",
                        Style::default().fg(DARK_GRAY).italic(),
                    ))]
                }
            }
            None => vec![Line::from(Span::styled(
                "No downloads — press d on a track, playlist, or album to save it offline",
                Style::default().fg(DARK_GRAY).italic(),
            ))],
        };
        if !app.download_failures.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Recent download errors (full chain in download-errors.log):",
                Style::default().fg(RED),
            )));
            for failure in app.download_failures.iter().take(2) {
                lines.push(Line::from(Span::styled(
                    format!("{}: {}", failure.track, failure.message),
                    Style::default().fg(DARK_GRAY),
                )));
            }
        }
        lines
    } else if app.tab == Tab::Albums {
        match app.list_state.selected().and_then(|i| app.albums.get(i)) {
            Some(album) => {
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled("Name:        ", Style::default().fg(DARK_GRAY)),
                        Span::styled(album.name.clone(), Style::default().fg(WHITE).bold()),
                    ]),
                    Line::from(vec![
                        Span::styled("Artist:      ", Style::default().fg(DARK_GRAY)),
                        Span::styled(album.artist.clone(), Style::default().fg(WHITE)),
                    ]),
                ];
                if !album.artists.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("Artists:     ", Style::default().fg(DARK_GRAY)),
                        Span::styled(album.artists.join(", "), Style::default().fg(WHITE)),
                    ]));
                }
                lines.push(Line::from(vec![
                    Span::styled("Type:        ", Style::default().fg(DARK_GRAY)),
                    Span::styled(
                        album.album_type.as_deref().unwrap_or("—"),
                        Style::default().fg(WHITE),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Released:    ", Style::default().fg(DARK_GRAY)),
                    Span::styled(
                        album.release_date.as_deref().unwrap_or("—"),
                        Style::default().fg(WHITE),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("Tracks:      ", Style::default().fg(DARK_GRAY)),
                    Span::styled(
                        album
                            .track_count
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "—".to_string()),
                        Style::default().fg(WHITE),
                    ),
                ]));
                if let Some(progress) = app.album_downloads.get(&album.id) {
                    let status = if progress.total > 0 {
                        let failed = if progress.failed > 0 {
                            format!(", {} failed", progress.failed)
                        } else {
                            String::new()
                        };
                        format!("{}/{}{}", progress.completed, progress.total, failed)
                    } else {
                        "fetching tracks".to_string()
                    };
                    lines.push(Line::from(vec![
                        Span::styled("Offline:     ", Style::default().fg(DARK_GRAY)),
                        Span::styled(status, Style::default().fg(GRAY)),
                    ]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Enter: load tracks · d: download album",
                    Style::default().fg(DARK_GRAY).italic(),
                )));
                lines
            }
            None => vec![Line::from(Span::styled(
                "Select an album to see details",
                Style::default().fg(DARK_GRAY).italic(),
            ))],
        }
    } else if app.tab == Tab::Artists {
        match app.list_state.selected().and_then(|i| app.artists.get(i)) {
            Some(artist) => vec![
                Line::from(vec![
                    Span::styled("Name:   ", Style::default().fg(DARK_GRAY)),
                    Span::styled(artist.name.clone(), Style::default().fg(WHITE).bold()),
                ]),
                Line::from(vec![
                    Span::styled("Genres: ", Style::default().fg(DARK_GRAY)),
                    Span::styled(
                        if artist.genres.is_empty() {
                            "—".to_string()
                        } else {
                            artist.genres.join(", ")
                        },
                        Style::default().fg(GRAY),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Albums: ", Style::default().fg(DARK_GRAY)),
                    Span::styled(app.albums.len().to_string(), Style::default().fg(WHITE)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter: view albums",
                    Style::default().fg(DARK_GRAY).italic(),
                )),
            ],
            None => vec![Line::from(Span::styled(
                "Select an artist to see details",
                Style::default().fg(DARK_GRAY).italic(),
            ))],
        }
    } else {
        match app.list_state.selected().and_then(|i| app.tracks.get(i)) {
            Some(track) => vec![
                Line::from(vec![
                    Span::styled("Title:   ", Style::default().fg(DARK_GRAY)),
                    Span::styled(track.title.clone(), Style::default().fg(WHITE).bold()),
                ]),
                Line::from(vec![
                    Span::styled("Artist:  ", Style::default().fg(DARK_GRAY)),
                    Span::styled(track.artist.clone(), Style::default().fg(WHITE)),
                ]),
                Line::from(vec![
                    Span::styled("Album:   ", Style::default().fg(DARK_GRAY)),
                    Span::styled(track.album.clone(), Style::default().fg(WHITE)),
                ]),
                Line::from(vec![
                    Span::styled("Length:  ", Style::default().fg(DARK_GRAY)),
                    Span::styled(format_duration(track.duration), Style::default().fg(WHITE)),
                ]),
                Line::from(vec![
                    Span::styled("Added by:", Style::default().fg(DARK_GRAY)),
                    Span::styled(
                        track.added_by.clone().unwrap_or_else(|| "—".to_string()),
                        Style::default().fg(GRAY),
                    ),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    if app.current_playlist_id.is_some() {
                        "Enter play · J/K or [ ] reorder · A add · X remove · S save · D discard"
                    } else {
                        "Enter play · A add to playlist"
                    },
                    Style::default().fg(DARK_GRAY).italic(),
                )),
            ],
            None => vec![Line::from(Span::styled(
                "Select a track to see details",
                Style::default().fg(DARK_GRAY).italic(),
            ))],
        }
    };

    frame.render_widget(Paragraph::new(lines), text_area);
}

/// Find space for the current-track cover in a full-width pane. Side-by-side
/// keeps the lyrics/queue list tall; narrow terminals fall back to a stacked
/// cover, and very small terminals keep the full area for text.
fn current_art_layout(
    frame: &Frame,
    area: Rect,
    app: &App,
    min_text_cols: u16,
    min_text_rows: u16,
) -> (Option<Rect>, Rect) {
    if !app.art_enabled {
        return (None, area);
    }
    let size = art::cover_size(frame.area());
    if size.width == 0 || size.height == 0 {
        return (None, area);
    }
    let art_cols = size.width.saturating_add(2);
    if area.width >= art_cols.saturating_add(min_text_cols) && area.height >= min_text_rows {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(art_cols), Constraint::Min(min_text_cols)])
            .split(area);
        return (Some(chunks[0]), chunks[1]);
    }
    let art_rows = size.height.saturating_add(2);
    if area.width >= size.width && area.height >= art_rows.saturating_add(min_text_rows) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(art_rows), Constraint::Min(min_text_rows)])
            .split(area);
        return (Some(chunks[0]), chunks[1]);
    }
    (None, area)
}

/// Draw a protocol at its natural size, centred in `area`. Returns false when
/// there is no image (or it does not fit), leaving the caller to show text.
fn render_image(frame: &mut Frame, area: Rect, protocol: Option<&Protocol>) -> bool {
    let Some(protocol) = protocol else {
        return false;
    };
    let size = protocol.size();
    if size.width == 0 || size.height == 0 || size.width > area.width || size.height > area.height {
        return false;
    }
    let x = area.x + (area.width - size.width) / 2;
    let y = area.y + (area.height - size.height) / 2;
    frame.render_widget(
        ratatui_image::Image::new(protocol).allow_clipping(true),
        Rect::new(x, y, size.width, size.height),
    );
    true
}

fn render_cover(frame: &mut Frame, area: Rect, art: &CoverArt, art_enabled: bool) {
    if render_image(frame, area, art.protocol()) {
        return;
    }
    render_art_placeholder(frame, area, art, art_enabled);
}

fn render_art_placeholder(frame: &mut Frame, area: Rect, art: &CoverArt, art_enabled: bool) {
    let mut lines: Vec<Line> = art
        .placeholder_lines()
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::default().fg(DARK_GRAY).italic())))
        .collect();
    let hint = if art_enabled {
        " [c] hide art"
    } else {
        " [c] show art"
    };
    if area.height > lines.len() as u16 + 1 {
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(DARK_GRAY),
        )));
    }
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

/// Persistent, compact now-playing art in the footer. Shown on every tab so the
/// current track's cover is never lost while browsing playlists or downloads.
fn render_footer_art(frame: &mut Frame, area: Rect, app: &App) {
    if render_image(frame, area, app.art.thumbnail()) {
        return;
    }
    let label = app.art.compact_placeholder();
    if label.is_empty() {
        return;
    }
    let padding = area.height.saturating_sub(1) / 2;
    let mut lines = vec![Line::from(""); padding as usize];
    lines.push(Line::from(Span::styled(
        label,
        Style::default().fg(DARK_GRAY).italic(),
    )));
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Now Playing ")
        .style(Style::default().fg(CYAN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    const MIN_TEXT_COLS: u16 = 40;
    let thumb_cols = art::THUMB_COLS.saturating_add(2);
    let (thumb_area, text_area) = if app.art_enabled
        && inner.width >= thumb_cols.saturating_add(MIN_TEXT_COLS)
        && inner.height >= art::THUMB_ROWS
    {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(thumb_cols),
                Constraint::Min(MIN_TEXT_COLS),
            ])
            .split(inner);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, inner)
    };
    if let Some(thumb_area) = thumb_area {
        render_footer_art(frame, thumb_area, app);
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // track name
            Constraint::Length(1), // progress bar
            Constraint::Min(1),    // keybindings (absorbs the extra thumbnail row)
        ])
        .split(text_area);

    let (track_text, progress, volume, playing) = match &app.now_playing {
        Some(np) => {
            let icon = if np.playing { "▶" } else { "❚❚" };
            (
                format!("{icon} {} — {}", np.track.title, np.track.artist),
                np.position.as_secs_f64() / np.track.duration.as_secs_f64().max(1.0),
                np.volume,
                np.playing,
            )
        }
        None => ("No track selected".to_string(), 0.0, 0, false),
    };

    frame.render_widget(
        Paragraph::new(track_text).style(Style::default().fg(WHITE).bold()),
        chunks[0],
    );

    let bar_width = chunks[1].width.saturating_sub(20) as f64;
    let filled = (progress * bar_width).round() as usize;
    let bar: String = "▓".repeat(filled) + &"░".repeat(bar_width as usize - filled);
    let pos = app
        .now_playing
        .as_ref()
        .map(|np| format_duration(np.position))
        .unwrap_or_else(|| "00:00".to_string());
    let dur = app
        .now_playing
        .as_ref()
        .map(|np| format_duration(np.track.duration))
        .unwrap_or_else(|| "00:00".to_string());
    let progress_line = Line::from(vec![
        Span::styled(pos, Style::default().fg(GRAY)),
        Span::styled(format!(" {bar} "), Style::default().fg(CYAN)),
        Span::styled(dur, Style::default().fg(GRAY)),
        Span::styled(format!("   Vol: {volume}%"), Style::default().fg(YELLOW)),
    ]);
    frame.render_widget(Paragraph::new(progress_line), chunks[1]);

    let state = if playing { "PLAYING" } else { "PAUSED" };
    let keys = Line::from(vec![
        Span::styled(" [Space] ", Style::default().fg(YELLOW)),
        Span::raw("play/pause"),
        Span::styled(" [n/p] ", Style::default().fg(YELLOW)),
        Span::raw("next/prev"),
        Span::styled(" [←/→] ", Style::default().fg(YELLOW)),
        Span::raw("seek"),
        Span::styled(" [+/-] ", Style::default().fg(YELLOW)),
        Span::raw("vol"),
        Span::styled(" [/] ", Style::default().fg(YELLOW)),
        Span::raw(match app.tab {
            Tab::Albums => "search albums",
            Tab::Artists => "search artists",
            _ => "search tracks",
        }),
        Span::styled(" [l] ", Style::default().fg(YELLOW)),
        Span::raw("lyrics"),
        Span::styled(" [c] ", Style::default().fg(YELLOW)),
        Span::raw(if app.art_enabled { "art ON" } else { "art" }),
        Span::styled(" [Q] ", Style::default().fg(YELLOW)),
        Span::raw("queue"),
        Span::styled(" [s] ", Style::default().fg(YELLOW)),
        Span::raw(if app.shuffle { "shuffle ON" } else { "shuffle" }),
        Span::styled(" [r] ", Style::default().fg(YELLOW)),
        Span::raw(match app.repeat {
            RepeatMode::Off => "repeat",
            RepeatMode::All => "repeat LIST",
            RepeatMode::One => "repeat ONE",
        }),
        Span::styled(" [t] ", Style::default().fg(YELLOW)),
        Span::raw(if app.translate_lyrics {
            "translate ON"
        } else {
            "translate"
        }),
        Span::styled(" [a] ", Style::default().fg(YELLOW)),
        Span::raw("follow"),
        Span::styled(" [o] ", Style::default().fg(YELLOW)),
        Span::raw("offline"),
        Span::styled(" [d] ", Style::default().fg(YELLOW)),
        Span::raw(match app.tab {
            Tab::Playlists => "download playlist",
            Tab::Albums => "download album",
            _ => "download track",
        }),
        Span::styled(" [Enter] ", Style::default().fg(YELLOW)),
        Span::raw("jump"),
        Span::styled(" [?] ", Style::default().fg(YELLOW)),
        Span::raw("help"),
        Span::styled(" [q] ", Style::default().fg(YELLOW)),
        Span::raw("quit"),
        Span::styled(format!("  {state}"), Style::default().fg(DARK_GRAY)),
    ]);
    frame.render_widget(
        Paragraph::new(keys).style(Style::default().fg(GRAY)),
        chunks[2],
    );
}

/// Centered overlay for the "add selected track to playlist" picker.
fn render_playlist_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = &app.playlist_picker else {
        return;
    };
    let popup_width = area.width.min(64);
    let popup_height = area.height.min(app.playlists.len() as u16 + 7);
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup = Rect::new(x, y, popup_width, popup_height);
    let inner_height = popup.height.saturating_sub(5) as usize;

    let mut lines = vec![
        Line::from(vec![Span::styled(
            format!(" Add '{}' to playlist ", picker.track.title),
            Style::default().fg(CYAN).bold(),
        )]),
        Line::from(""),
    ];
    let start = picker.cursor.saturating_sub(inner_height.saturating_sub(1));
    for (offset, playlist) in app
        .playlists
        .iter()
        .enumerate()
        .skip(start)
        .take(inner_height)
    {
        let selected = offset == picker.cursor;
        let style = if selected {
            Style::default().fg(Color::Black).bg(CYAN)
        } else {
            Style::default().fg(WHITE)
        };
        lines.push(Line::from(Span::styled(
            format!(" {}{}", if selected { "> " } else { "  " }, playlist.name),
            style,
        )));
    }
    if app.playlists.is_empty() {
        lines.push(Line::from(Span::styled(
            " No playlists available ",
            Style::default().fg(DARK_GRAY).italic(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " j/k or ↑/↓ select · Enter add · Esc cancel ",
        Style::default().fg(DARK_GRAY),
    )));

    let widget =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Playlists "));
    frame.render_widget(ratatui::widgets::Clear, popup);
    Widget::render(widget, popup, frame.buffer_mut());
}

/// Centered text input for creating or renaming a playlist.
fn render_prompt(frame: &mut Frame, area: Rect, app: &App) {
    let Some(prompt) = &app.prompt else {
        return;
    };
    let label = prompt.label();
    let popup_width = area.width.min(64);
    let popup_height = area.height.min(9);
    let popup = Rect::new(
        (area.width.saturating_sub(popup_width)) / 2,
        (area.height.saturating_sub(popup_height)) / 2,
        popup_width,
        popup_height,
    );
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(label, Style::default().fg(WHITE).bold())),
        Line::from(""),
        Line::from(Span::styled(
            format!("> {}_", app.prompt_input),
            Style::default().fg(CYAN),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Enter confirm · Esc cancel ",
            Style::default().fg(DARK_GRAY),
        )),
    ];
    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Edit playlist "),
    );
    frame.render_widget(ratatui::widgets::Clear, popup);
    Widget::render(widget, popup, frame.buffer_mut());
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup_width = area.width.min(64);
    let popup_height = area.height.min(42);
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup = Rect::new(x, y, popup_width, popup_height);

    let help_text = Text::from(vec![
        Line::from(Span::styled(
            " termify keybindings ",
            Style::default().fg(CYAN).bold(),
        )),
        Line::from(""),
        Line::from("  j / ↓       Move down (select lyric line on Lyrics tab)"),
        Line::from("  k / ↑       Move up (select lyric line on Lyrics tab)"),
        Line::from("  g / G       First / last item"),
        Line::from("  PgUp/PgDn   Page up / down"),
        Line::from("  Enter       Select track"),
        Line::from("  /           Contextual search (tracks / albums / artists)"),
        Line::from("  Tab         Next tab (Shift+Tab prev)"),
        Line::from("  Space       Play / pause"),
        Line::from("  n / p       Next / previous track"),
        Line::from("  ← / →       Seek back / forward 5s"),
        Line::from("  + / -       Volume up / down"),
        Line::from("  l           Lyrics tab"),
        Line::from("  Q           Queue tab"),
        Line::from("  o           Offline / downloads tab"),
        Line::from("  d           Download track / playlist / album"),
        Line::from("  D           Delete offline copy (Offline tab)"),
        Line::from("  J / K       Move queue track down / up (Queue tab)"),
        Line::from("  Enter       Play queue track / jump to selected lyric"),
        Line::from("  N           New private playlist (Playlists tab)"),
        Line::from("  R           Rename selected playlist (open playlist is staged)"),
        Line::from("  A           Add selected track to a playlist"),
        Line::from("  X           Remove selected track from open playlist (staged)"),
        Line::from("  J/K, [ / ]  Move selected track in open playlist (staged)"),
        Line::from("  S           Save all staged playlist changes at once"),
        Line::from("  D           Discard staged playlist changes"),
        Line::from("  s           Shuffle on / off"),
        Line::from("  r           Repeat off / playlist / one (shuffle-aware; MPRIS Loop Status)"),
        Line::from("  c           Show / hide cover art (falls back to text when unsupported)"),
        Line::from("  P           Toggle Discord Rich Presence"),
        Line::from("  a           Lyrics follow playback on / off"),
        Line::from("  t           Translate lyrics to English (lyrics tab)"),
        Line::from("  ?           Toggle this help"),
        Line::from("  q / Ctrl-C  Quit"),
        Line::from(""),
        Line::from(Span::styled(
            " Press ? / Esc / Enter to close ",
            Style::default().fg(DARK_GRAY).italic(),
        )),
    ]);

    let widget = Paragraph::new(help_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Help ")
            .style(Style::default().fg(WHITE)),
    );
    frame.render_widget(ratatui::widgets::Clear, popup);
    Widget::render(widget, popup, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::art::{self, CoverArt};
    use crate::ui::app::{
        Album, App, Artist, DownloadFailure, Playlist, PlaylistDownload, PromptKind, Track,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui_image::picker::Picker;
    use tokio::sync::mpsc;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = App::new(tx, Picker::halfblocks(), None);
        app.load_demo();
        app
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn detail_pane_renders_both_ready_art_and_placeholder() {
        let mut app = test_app();
        app.art = CoverArt::no_art();

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();

        let image: image::DynamicImage = image::RgbImage::new(64, 64).into();
        let (cover, thumbnail) = art::build_protocols(
            &Picker::halfblocks(),
            image.clone(),
            ratatui::layout::Size::new(14, 7),
        )
        .unwrap();
        app.art = CoverArt::ready("https://example.com/cover.jpg", image, cover, thumbnail);
        terminal.draw(|frame| render(frame, &app)).unwrap();
    }

    #[test]
    fn lyrics_and_queue_tabs_render_current_track_art() {
        let mut app = test_app();
        app.tab = Tab::Lyrics;
        app.art = CoverArt::no_art();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("No cover art"),
            "lyrics pane should reserve cover space"
        );
        assert!(text.contains("No lyrics for this track."));

        let image: image::DynamicImage = image::RgbImage::new(64, 64).into();
        let (cover, thumbnail) = art::build_protocols(
            &Picker::halfblocks(),
            image.clone(),
            art::cover_size(Rect::new(0, 0, 100, 30)),
        )
        .unwrap();
        app.art = CoverArt::ready("https://example.com/cover.jpg", image, cover, thumbnail);
        terminal.draw(|frame| render(frame, &app)).unwrap();
        assert!(buffer_text(&terminal).contains("No lyrics for this track."));

        app.tab = Tab::Queue;
        terminal.draw(|frame| render(frame, &app)).unwrap();
        assert!(buffer_text(&terminal).contains("Queue"));
    }

    #[test]
    fn downloads_tab_shows_recent_failures() {
        let mut app = test_app();
        app.tab = Tab::Downloads;
        app.download_failures.push(DownloadFailure {
            track: "Song — Artist".to_string(),
            message: "audio key error".to_string(),
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Recent download errors"));
        assert!(text.contains("audio key error"));
    }

    #[test]
    fn details_show_playlist_collaborators_and_track_attribution() {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = App::new(tx, Picker::halfblocks(), None);
        app.art_enabled = false;
        app.tab = Tab::Playlists;
        app.playlists = vec![Playlist {
            id: "id-1".into(),
            name: "Road Trip".into(),
            description: "Weekend playlist".into(),
            track_count: Some(2),
            owner: Some("Alice".into()),
            collaborative: true,
            is_public: Some(false),
            contributors: vec!["Alice".into(), "Bob".into()],
            art_url: None,
            snapshot_id: "snapshot".into(),
        }];
        app.list_state.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Owner:"));
        assert!(text.contains("Alice"));
        assert!(text.contains("Contributors:"));
        assert!(text.contains("Alice, Bob"));
        assert!(text.contains("Collaborative:"));
        assert!(text.contains("Private"));

        app.tab = Tab::Search;
        app.tracks = vec![Track {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: std::time::Duration::from_secs(120),
            uri: "spotify:track:song".into(),
            art_url: None,
            added_by: Some("Bob".into()),
        }];
        app.list_state.select(Some(0));
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Added by:"));
        assert!(text.contains("Bob"));
    }

    #[test]
    fn details_infers_shared_playlist_from_contributors() {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = App::new(tx, Picker::halfblocks(), None);
        app.art_enabled = false;
        app.tab = Tab::Playlists;
        app.playlists = vec![Playlist {
            id: "id-shared".into(),
            name: "Shared".into(),
            description: String::new(),
            track_count: Some(2),
            owner: Some("Alice".into()),
            collaborative: false,
            is_public: Some(false),
            contributors: vec!["Alice".into(), "Bob".into()],
            art_url: None,
            snapshot_id: "snapshot".into(),
        }];
        app.list_state.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Yes (inferred)"));
    }

    #[test]
    fn edit_overlays_render() {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = App::new(tx, Picker::halfblocks(), None);
        app.art_enabled = false;
        app.prompt = Some(PromptKind::NewPlaylist);
        app.prompt_input = "Mixtape".into();

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("New playlist name"));
        assert!(text.contains("Mixtape"));

        app.prompt = None;
        app.playlist_picker = Some(crate::ui::app::PlaylistPicker {
            cursor: 0,
            track: Track {
                title: "Song".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                duration: std::time::Duration::from_secs(120),
                uri: "spotify:track:song".into(),
                art_url: None,
                added_by: None,
            },
        });
        app.playlists = vec![Playlist {
            id: "id-1".into(),
            name: "Road Trip".into(),
            description: String::new(),
            track_count: None,
            owner: None,
            collaborative: false,
            is_public: None,
            contributors: Vec::new(),
            art_url: None,
            snapshot_id: String::new(),
        }];
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Add 'Song' to playlist"));
        assert!(text.contains("Road Trip"));
    }

    #[test]
    fn album_and_artist_lists_and_details_render() {
        let mut app = test_app();
        app.art_enabled = false;
        app.tab = Tab::Albums;
        app.albums = vec![Album {
            id: "album-1".into(),
            name: "Blue Hour".into(),
            artist: "Nova".into(),
            artists: vec!["Nova".into(), "Guest".into()],
            album_type: Some("album".into()),
            release_date: Some("2024-05-01".into()),
            track_count: Some(11),
            art_url: None,
        }];
        app.album_downloads.insert(
            "album-1".into(),
            PlaylistDownload {
                name: "Blue Hour".into(),
                total: 5,
                completed: 2,
                ..Default::default()
            },
        );
        app.list_state.select(Some(0));

        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Albums"));
        assert!(text.contains("Blue Hour — Nova"));
        assert!(text.contains("offline 2/5"));
        assert!(text.contains("Nova, Guest"));
        assert!(text.contains("2024-05-01"));
        assert!(text.contains("Enter: load tracks · d: download album"));

        app.tab = Tab::Artists;
        app.artists = vec![Artist {
            id: "artist-1".into(),
            name: "Nova".into(),
            genres: vec!["indie".into(), "electronic".into(), "ambient".into()],
            art_url: None,
        }];
        app.list_state.select(Some(0));
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Artists"));
        assert!(text.contains("indie, electronic +1"));
        assert!(text.contains("Genres:"));
        assert!(text.contains("indie, electronic, ambient"));
        assert!(text.contains("Enter: view albums"));
    }

    #[test]
    fn album_and_artist_details_use_tab_specific_art() {
        let mut app = test_app();
        app.tab = Tab::Albums;
        app.albums = vec![Album {
            id: "album-1".into(),
            name: "Blue Hour".into(),
            artist: "Nova".into(),
            artists: Vec::new(),
            album_type: None,
            release_date: None,
            track_count: None,
            art_url: None,
        }];
        app.list_state.select(Some(0));
        app.art = CoverArt::disabled("track art marker");
        app.album_art = CoverArt::disabled("album art marker");
        app.artist_art = CoverArt::disabled("artist art marker");

        let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("album art marker"));
        assert!(!text.contains("track art marker"));

        app.tab = Tab::Artists;
        app.artists = vec![Artist {
            id: "artist-1".into(),
            name: "Nova".into(),
            genres: Vec::new(),
            art_url: None,
        }];
        app.list_state.select(Some(0));
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("artist art marker"));
        assert!(!text.contains("album art marker"));
    }

    #[test]
    fn contextual_search_and_download_hints_render() {
        let mut app = test_app();
        app.art_enabled = false;
        app.tab = Tab::Albums;
        let mut terminal = Terminal::new(TestBackend::new(200, 36)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("search albums"));
        assert!(text.contains("download album"));

        app.show_help = true;
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Contextual search (tracks / albums / artists)"));
        assert!(text.contains("Download track / playlist / album"));
    }
}
