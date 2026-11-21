//! Terminal cover-art state and helpers.
//!
//! Fetching and decoding happen off the UI thread. The decoded image is turned
//! into a protocol for the current terminal size, so resizing can rebuild it
//! without another network request. Any failure is represented as a placeholder
//! state instead of propagating into the main loop.

use std::io::Cursor;

use anyhow::{Context, Result};
use image::DynamicImage;
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};

/// Shared layout constants so the art sizing and the renderer agree.
pub const HEADER_ROWS: u16 = 3;
pub const FOOTER_ROWS: u16 = 5;

/// Largest cover we draw. Terminal cells are roughly twice as tall as they are
/// wide, so 44x22 cells still looks like a square album cover.
pub const ART_COLS: u16 = 44;
pub const ART_ROWS: u16 = 22;

/// Rows kept for metadata/hints in the Details pane even when art is large.
const MIN_DETAIL_ROWS: u16 = 7;
/// Breathing room between the cover and the pane border.
const ART_MARGIN_COLS: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtStatus {
    Disabled,
    NoArt,
    Loading,
    Ready,
    Failed,
}

pub enum CoverArt {
    Disabled {
        reason: String,
    },
    NoArt,
    Loading {
        url: String,
    },
    /// Image downloaded but the terminal is too small to encode it yet.
    Deferred {
        url: String,
        image: DynamicImage,
    },
    Ready {
        url: String,
        image: DynamicImage,
        protocol: Protocol,
    },
    Failed {
        url: String,
        message: String,
    },
}

impl CoverArt {
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self::Disabled {
            reason: reason.into(),
        }
    }

    pub fn no_art() -> Self {
        Self::NoArt
    }

    pub fn loading(url: impl Into<String>) -> Self {
        Self::Loading { url: url.into() }
    }

    pub fn deferred(url: impl Into<String>, image: DynamicImage) -> Self {
        Self::Deferred {
            url: url.into(),
            image,
        }
    }

    pub fn ready(url: impl Into<String>, image: DynamicImage, protocol: Protocol) -> Self {
        Self::Ready {
            url: url.into(),
            image,
            protocol,
        }
    }

    pub fn failed(url: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failed {
            url: url.into(),
            message: message.into(),
        }
    }

    pub fn status(&self) -> ArtStatus {
        match self {
            Self::Disabled { .. } => ArtStatus::Disabled,
            Self::NoArt => ArtStatus::NoArt,
            Self::Loading { .. } | Self::Deferred { .. } => ArtStatus::Loading,
            Self::Ready { .. } => ArtStatus::Ready,
            Self::Failed { .. } => ArtStatus::Failed,
        }
    }

    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Loading { url }
            | Self::Deferred { url, .. }
            | Self::Ready { url, .. }
            | Self::Failed { url, .. } => Some(url),
            Self::Disabled { .. } | Self::NoArt => None,
        }
    }

    pub fn protocol(&self) -> Option<&Protocol> {
        match self {
            Self::Ready { protocol, .. } => Some(protocol),
            _ => None,
        }
    }

    /// Text shown in place of an image when there is nothing to draw. Graphic
    /// terminals still get this state while loading and on failures, so the
    /// Details pane never ends up blank or in an inconsistent state.
    pub fn placeholder_lines(&self) -> Vec<String> {
        match self {
            Self::Disabled { reason } => {
                vec!["Cover art hidden".to_string(), reason.clone()]
            }
            Self::NoArt => vec!["No cover art".to_string(), "for this track".to_string()],
            Self::Loading { .. } => vec!["Loading cover art...".to_string()],
            Self::Deferred { .. } => vec!["Enlarge terminal for cover art".to_string()],
            Self::Failed { message, .. } => {
                vec!["Cover art unavailable".to_string(), message.clone()]
            }
            Self::Ready { .. } => Vec::new(),
        }
    }
}

/// Area occupied by the Details pane's content (inside its borders).
pub fn detail_inner(terminal: Rect) -> Rect {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_ROWS),
            Constraint::Min(5),
            Constraint::Length(FOOTER_ROWS),
        ])
        .split(terminal);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);
    let pane = panes[1];
    Rect::new(
        pane.x.saturating_add(1),
        pane.y.saturating_add(1),
        pane.width.saturating_sub(2),
        pane.height.saturating_sub(2),
    )
}

/// Cover size for the current terminal. The image grows on tall/wide terminals
/// but never takes the rows needed for track metadata and hints. A zero size
/// means there is not enough room to show art at all.
pub fn cover_size(terminal: Rect) -> Size {
    let inner = detail_inner(terminal);
    if inner.width <= ART_MARGIN_COLS + 4 || inner.height <= MIN_DETAIL_ROWS {
        return Size::new(0, 0);
    }
    let rows = inner.height.saturating_sub(MIN_DETAIL_ROWS).min(ART_ROWS);
    let cols = inner
        .width
        .saturating_sub(ART_MARGIN_COLS)
        .min(rows.saturating_mul(2))
        .min(ART_COLS);
    let rows = rows.min(cols / 2);
    if cols < 4 || rows < 2 {
        Size::new(0, 0)
    } else {
        Size::new(cols, rows)
    }
}

/// Download and decode an image into a format-agnostic buffer.
pub async fn fetch_image(client: &reqwest::Client, url: &str) -> Result<DynamicImage> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching cover art from {url}"))?
        .error_for_status()
        .with_context(|| format!("cover art request failed for {url}"))?;
    let bytes = response
        .bytes()
        .await
        .context("reading cover art response")?;
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("detecting cover art format")?;
    reader.decode().context("decoding cover art")
}

/// Encode a decoded image for the detected terminal protocol at `size`. Any
/// protocol error is returned so the caller can show a text placeholder.
///
/// `Scale` (rather than `Fit`) is important: Spotify's 300px image has a
/// "natural" cell size of roughly 30x15, which would cap the cover well below
/// the space we reserved on larger terminals.
pub fn build_protocol(picker: &Picker, image: DynamicImage, size: Size) -> Result<Protocol> {
    anyhow::ensure!(
        size.width > 0 && size.height > 0,
        "no room to render cover art"
    );
    picker
        .new_protocol(image, size, Resize::Scale(None))
        .context("preparing cover art for terminal")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_states_are_explicit() {
        assert_eq!(CoverArt::no_art().status(), ArtStatus::NoArt);
        assert_eq!(
            CoverArt::loading("url").placeholder_lines(),
            vec!["Loading cover art..."]
        );
        assert_eq!(
            CoverArt::failed("url", "network down").placeholder_lines()[1],
            "network down"
        );
        assert_eq!(
            CoverArt::disabled("TERM=dumb").placeholder_lines()[0],
            "Cover art hidden"
        );
    }

    #[test]
    fn cover_scales_with_terminal_but_keeps_metadata_room() {
        let small = cover_size(Rect::new(0, 0, 100, 24));
        assert!(small.height <= 7);
        assert_eq!(small.width, small.height * 2);

        let large = cover_size(Rect::new(0, 0, 120, 40));
        assert_eq!(large, Size::new(44, 22));

        let narrow = cover_size(Rect::new(0, 0, 60, 40));
        assert!(narrow.width <= 26);
        assert_eq!(narrow.width, narrow.height * 2);

        let tiny = cover_size(Rect::new(0, 0, 40, 10));
        assert_eq!(tiny, Size::new(0, 0));
    }

    #[test]
    fn halfblocks_protocol_fits_requested_size() {
        let picker = Picker::halfblocks();
        let image: DynamicImage = image::RgbImage::new(300, 300).into();
        let size = Size::new(40, 20);
        let protocol = build_protocol(&picker, image, size).expect("halfblocks protocol");
        assert_eq!(protocol.size(), size);
    }
}
