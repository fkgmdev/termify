use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Widget};

use super::app::{App, Tab, format_duration};

const CYAN: Color = Color::Cyan;
const YELLOW: Color = Color::Yellow;
const GRAY: Color = Color::Gray;
const DARK_GRAY: Color = Color::DarkGray;
const WHITE: Color = Color::White;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),    // main area
            Constraint::Length(5), // now playing footer
        ])
        .split(frame.area());

    render_header(frame, chunks[0], app);
    render_main(frame, chunks[1], app);
    render_footer(frame, chunks[2], app);

    if app.show_help {
        render_help(frame, frame.area());
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line<'static>> = Tab::ALL
        .iter()
        .map(|tab| Line::from(tab.title()))
        .collect();

    let selected = match app.tab {
        Tab::Search => 0,
        Tab::Playlists => 1,
        Tab::Lyrics => 2,
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
    if app.tab == Tab::Lyrics {
        render_lyrics_pane(frame, area, app);
        return;
    }

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_list_pane(frame, main_chunks[0], app);
    render_detail_pane(frame, main_chunks[1], app);
}

fn render_lyrics_pane(frame: &mut Frame, area: Rect, app: &App) {
    let uri = app.now_playing.as_ref().map(|np| np.track.uri.as_str());
    let lyrics = uri.and_then(|uri| app.lyrics.get(uri));
    let title = match (&app.now_playing, lyrics) {
        (Some(np), Some(lyrics)) => format!(
            " Lyrics — {} ({}{}) ",
            np.track.title,
            lyrics.source,
            if lyrics.synced { "" } else { ", unsynced" }
        ),
        (Some(np), None) => format!(" Lyrics — {} ", np.track.title),
        _ => " Lyrics ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

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
        let mut offset = app.lyrics_scroll;
        let max_offset = lyrics.lines.len().saturating_sub(inner.height as usize / 2);
        offset = offset.min(max_offset);

        lyrics
            .lines
            .iter()
            .skip(offset)
            .take(inner.height as usize)
            .enumerate()
            .map(|(i, line)| {
                let absolute = offset + i;
                let style = if Some(absolute) == active {
                    Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)
                } else if active.is_some_and(|a| absolute < a) {
                    Style::default().fg(DARK_GRAY)
                } else {
                    Style::default().fg(WHITE)
                };
                let prefix = if Some(absolute) == active { "▶ " } else { "  " };
                Line::from(Span::styled(
                    format!("{prefix}{}", line.text),
                    style,
                ))
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

    frame.render_widget(Paragraph::new(lines), inner);
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
        Tab::Playlists => " Playlists ".to_string(),
        Tab::Lyrics => " Lyrics ".to_string(),
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
                    ListItem::new(Line::from(Span::styled(
                        playlist.display(),
                        Style::default().fg(WHITE),
                    )))
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
                } else {
                    Style::default().fg(WHITE)
                };
                ListItem::new(Line::from(Span::styled(track.display(), style)))
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

fn render_detail_pane(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" Details ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = if app.tab == Tab::Playlists {
        match app.list_state.selected().and_then(|i| app.playlists.get(i)) {
            Some(playlist) => vec![
                Line::from(vec![
                    Span::styled("Name:  ", Style::default().fg(DARK_GRAY)),
                    Span::styled(playlist.name.clone(), Style::default().fg(WHITE).bold()),
                ]),
                Line::from(vec![
                    Span::styled("ID:    ", Style::default().fg(DARK_GRAY)),
                    Span::styled(playlist.id.clone(), Style::default().fg(GRAY)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    &playlist.description,
                    Style::default().fg(GRAY),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Press Enter to load tracks",
                    Style::default().fg(DARK_GRAY).italic(),
                )),
            ],
            None => vec![Line::from(Span::styled(
                "Select a playlist to see details",
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
            Line::from(""),
            Line::from(Span::styled(
                "Press Enter to select for playback",
                Style::default().fg(DARK_GRAY).italic(),
            )),
        ],
        None => vec![Line::from(Span::styled(
            "Select a track to see details",
            Style::default().fg(DARK_GRAY).italic(),
        ))],
        }
    };

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Now Playing ")
        .style(Style::default().fg(CYAN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // track name
            Constraint::Length(1), // progress bar
            Constraint::Length(1), // keybindings
        ])
        .split(inner);

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
        Span::raw("search"),
        Span::styled(" [l] ", Style::default().fg(YELLOW)),
        Span::raw("lyrics"),
        Span::styled(" [?] ", Style::default().fg(YELLOW)),
        Span::raw("help"),
        Span::styled(" [q] ", Style::default().fg(YELLOW)),
        Span::raw("quit"),
        Span::styled(format!("  {state}"), Style::default().fg(DARK_GRAY)),
    ]);
    frame.render_widget(Paragraph::new(keys).style(Style::default().fg(GRAY)), chunks[2]);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup_width = area.width.min(60);
    let popup_height = area.height.min(18);
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup = Rect::new(x, y, popup_width, popup_height);

    let help_text = Text::from(vec![
        Line::from(Span::styled(" termify keybindings ", Style::default().fg(CYAN).bold())),
        Line::from(""),
        Line::from("  j / ↓       Move down"),
        Line::from("  k / ↑       Move up"),
        Line::from("  g / G       First / last item"),
        Line::from("  PgUp/PgDn   Page up / down"),
        Line::from("  Enter       Select track"),
        Line::from("  /           Search"),
        Line::from("  Tab         Next tab (Shift+Tab prev)"),
        Line::from("  Space       Play / pause"),
        Line::from("  n / p       Next / previous track"),
        Line::from("  ← / →       Seek back / forward 5s"),
        Line::from("  + / -       Volume up / down"),
        Line::from("  l           Lyrics tab"),
        Line::from("  ?           Toggle this help"),
        Line::from("  q / Ctrl-C  Quit"),
        Line::from(""),
        Line::from(Span::styled(" Press ? / Esc / Enter to close ", Style::default().fg(DARK_GRAY).italic())),
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
