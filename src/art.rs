//! Terminal cover-art state and helpers.
//!
//! Fetching and decoding happen off the UI thread. The decoded image is turned
//! into a fixed-size `ratatui-image` protocol once, so rendering on every frame
//! stays cheap. Any failure is represented as a placeholder state instead of
//! propagating into the main loop.

use std::io::Cursor;

use anyhow::{Context, Result};
use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};

/// Cell size reserved for the cover in the Details pane. A square album cover
/// maps neatly onto this area with the usual ~1:2 terminal cell ratio.
pub const ART_COLS: u16 = 16;
pub const ART_ROWS: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtStatus {
    Disabled,
    NoArt,
    Loading,
    Ready,
    Failed,
}

pub enum CoverArt {
    Disabled { reason: String },
    NoArt,
    Loading { url: String },
    Ready { url: String, protocol: Protocol },
    Failed { url: String, message: String },
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

    pub fn ready(url: impl Into<String>, protocol: Protocol) -> Self {
        Self::Ready {
            url: url.into(),
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
            Self::Loading { .. } => ArtStatus::Loading,
            Self::Ready { .. } => ArtStatus::Ready,
            Self::Failed { .. } => ArtStatus::Failed,
        }
    }

    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Loading { url } | Self::Ready { url, .. } | Self::Failed { url, .. } => Some(url),
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
            Self::Failed { message, .. } => {
                vec!["Cover art unavailable".to_string(), message.clone()]
            }
            Self::Ready { .. } => Vec::new(),
        }
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

/// Encode a decoded image for the detected terminal protocol. Resizing is
/// bounded by the art area reserved in the layout, and any protocol error is
/// returned so the caller can show a text placeholder instead.
pub fn build_protocol(picker: &Picker, image: DynamicImage) -> Result<Protocol> {
    let size = Size::new(ART_COLS, ART_ROWS);
    picker
        .new_protocol(image, size, Resize::Fit(None))
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
    fn halfblocks_protocol_fits_reserved_area() {
        let picker = Picker::halfblocks();
        let image: DynamicImage = image::RgbImage::new(300, 300).into();
        let protocol = build_protocol(&picker, image).expect("halfblocks protocol");
        let size = protocol.size();
        assert!(size.width <= ART_COLS);
        assert!(size.height <= ART_ROWS);
    }
}
