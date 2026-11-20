use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use tokio::sync::mpsc::Sender;
use tokio::time::{Duration, interval};

use super::app::Action;

/// Forward crossterm keyboard/resize events and a periodic tick into the
/// single Action channel owned by `App`.
pub async fn run(tx: Sender<Action>) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = interval(Duration::from_millis(250));

    loop {
        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        if tx.send(Action::Key(key)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        if tx.send(Action::Render).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(err)) => return Err(err.into()),
                    Some(Ok(_)) => {} // focus, mouse, paste — ignore for now
                    None => break,
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
