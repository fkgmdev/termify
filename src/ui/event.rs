use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use tokio::sync::mpsc::Sender;
use tokio::time::{Duration, interval};

use super::app::Action;

/// Some Windows terminals (notably older ConPTY configurations) do not enable
/// VT input processing and report arrow keys as the three-byte fragment
/// `ESC [ A/B/C/D`. Crossterm normally combines this into `KeyCode::Up` etc.,
/// but when it does not we reassemble the fragment here. `Esc` is still sent
/// immediately so it can cancel the search/help overlays. A buffered `[` that
/// turns out not to be part of an arrow is replayed so editing keys are not
/// lost.
#[derive(Default)]
struct ArrowFragmentParser {
    after_esc: bool,
    after_esc_bracket: bool,
}

impl ArrowFragmentParser {
    fn normalize(&mut self, key: KeyEvent) -> Vec<KeyEvent> {
        if self.after_esc_bracket {
            self.after_esc_bracket = false;
            self.after_esc = false;
            if key.modifiers.is_empty() {
                let arrow = match key.code {
                    KeyCode::Char('A') => Some(KeyCode::Up),
                    KeyCode::Char('B') => Some(KeyCode::Down),
                    KeyCode::Char('C') => Some(KeyCode::Right),
                    KeyCode::Char('D') => Some(KeyCode::Left),
                    _ => None,
                };
                if let Some(code) = arrow {
                    return vec![KeyEvent::new(code, KeyModifiers::NONE)];
                }
            }
            // Not an arrow: replay the buffered '[' before the current key.
            let events = vec![
                KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
                key.clone(),
            ];
            if key.modifiers.is_empty() && key.code == KeyCode::Esc {
                self.after_esc = true;
            }
            return events;
        }

        if self.after_esc {
            self.after_esc = false;
            if key.modifiers.is_empty() && key.code == KeyCode::Char('[') {
                self.after_esc_bracket = true;
                return Vec::new();
            }
        }

        if key.modifiers.is_empty() && key.code == KeyCode::Esc {
            self.after_esc = true;
        }
        vec![key]
    }
}

/// Forward crossterm keyboard/resize events and a periodic tick into the
/// single Action channel owned by `App`.
pub async fn run(tx: Sender<Action>) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = interval(Duration::from_millis(250));
    let mut arrows = ArrowFragmentParser::default();

    loop {
        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        let mut closed = false;
                        for key in arrows.normalize(key) {
                            if tx.send(Action::Key(key)).await.is_err() {
                                closed = true;
                                break;
                            }
                        }
                        if closed {
                            break;
                        }
                    }
                    Some(Ok(Event::Resize(width, height))) => {
                        if tx.send(Action::Resize(width, height)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        tracing::error!("terminal event error: {err}");
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                if tx.send(Action::Tick).await.is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn reassembles_windows_terminal_arrow_fragments() {
        for (letter, arrow) in [
            ('A', KeyCode::Up),
            ('B', KeyCode::Down),
            ('C', KeyCode::Right),
            ('D', KeyCode::Left),
        ] {
            let mut parser = ArrowFragmentParser::default();
            assert_eq!(parser.normalize(key(KeyCode::Esc)), vec![key(KeyCode::Esc)]);
            assert!(parser.normalize(key(KeyCode::Char('['))).is_empty());
            assert_eq!(
                parser.normalize(key(KeyCode::Char(letter))),
                vec![key(arrow)]
            );
        }
    }

    #[test]
    fn non_arrow_bracket_is_replayed_and_plain_escape_passes_through() {
        let mut parser = ArrowFragmentParser::default();
        assert_eq!(parser.normalize(key(KeyCode::Esc)), vec![key(KeyCode::Esc)]);
        assert!(parser.normalize(key(KeyCode::Char('['))).is_empty());
        assert_eq!(
            parser.normalize(key(KeyCode::Char('x'))),
            vec![key(KeyCode::Char('[')), key(KeyCode::Char('x'))]
        );

        let mut parser = ArrowFragmentParser::default();
        assert_eq!(parser.normalize(key(KeyCode::Esc)), vec![key(KeyCode::Esc)]);
        assert_eq!(
            parser.normalize(key(KeyCode::Char('x'))),
            vec![key(KeyCode::Char('x'))]
        );
        assert_eq!(
            parser.normalize(key(KeyCode::Char('['))),
            vec![key(KeyCode::Char('['))]
        );
    }
}
