//! Lyrics fetching via LRCLIB (free, no API key) with LRC timestamp parsing.
//! Results are cached in-memory by track URI in `App`.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::ui::Track;

const LRCLIB_GET_URL: &str = "https://lrclib.net/api/get";

#[derive(Debug, Clone)]
pub struct LyricLine {
    pub time: Duration,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub synced: bool,
    pub source: String,
}

#[derive(Debug, Deserialize)]
struct LrclibResponse {
    #[serde(default, rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
    #[serde(default, rename = "plainLyrics")]
    plain_lyrics: Option<String>,
}

/// Fetch lyrics for a track from LRCLIB. Returns `None` when no lyrics exist.
pub async fn fetch(client: &reqwest::Client, track: &Track) -> Result<Option<Lyrics>> {
    let duration_secs = track.duration.as_secs();
    let response = client
        .get(LRCLIB_GET_URL)
        .query(&[
            ("track_name", track.title.as_str()),
            ("artist_name", track.artist.as_str()),
            ("album_name", track.album.as_str()),
            ("duration", &duration_secs.to_string()),
        ])
        .header("User-Agent", "termify/0.1 (local Spotify TUI)")
        .send()
        .await
        .context("LRCLIB request failed")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response.error_for_status().context("LRCLIB returned an error")?;

    let body: LrclibResponse = response.json().await.context("parsing LRCLIB response")?;

    if let Some(synced) = body.synced_lyrics {
        let lines = parse_lrc(&synced);
        if !lines.is_empty() {
            return Ok(Some(Lyrics {
                lines,
                synced: true,
                source: "LRCLIB".to_string(),
            }));
        }
    }

    if let Some(plain) = body.plain_lyrics {
        let lines: Vec<LyricLine> = plain
            .lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(i, line)| LyricLine {
                time: Duration::from_secs(i as u64 * 5),
                text: line.trim().to_string(),
            })
            .collect();
        if !lines.is_empty() {
            return Ok(Some(Lyrics {
                lines,
                synced: false,
                source: "LRCLIB (unsynced)".to_string(),
            }));
        }
    }

    Ok(None)
}

/// Parse LRC-formatted text into timestamped lines. Handles `[mm:ss.xx]` and
/// `[mm:ss]` stamps, including multiple stamps on one line.
pub fn parse_lrc(input: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();

    for raw in input.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('[') && !line.contains(']') {
            continue;
        }

        // Split prefix timestamps from the lyric text.
        let mut rest = line;
        let mut stamps = Vec::new();
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else { break };
            let stamp = &rest[1..end];
            if let Some(duration) = parse_timestamp(stamp) {
                stamps.push(duration);
            }
            rest = rest[end + 1..].trim_start();
        }

        if stamps.is_empty() || rest.is_empty() {
            continue;
        }

        for time in stamps {
            lines.push(LyricLine {
                time,
                text: rest.to_string(),
            });
        }
    }

    lines.sort_by_key(|line| line.time);
    lines
}

fn parse_timestamp(stamp: &str) -> Option<Duration> {
    let (mins, secs) = stamp.split_once(':')?;
    let mins: u64 = mins.trim().parse().ok()?;
    let secs: f64 = secs.trim().parse().ok()?;
    if secs < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(mins as f64 * 60.0 + secs))
}

/// Find the index of the lyric line active at the given playback position.
pub fn active_line(lyrics: &Lyrics, position: Duration) -> Option<usize> {
    let mut active = None;
    for (i, line) in lyrics.lines.iter().enumerate() {
        if line.time <= position {
            active = Some(i);
        } else {
            break;
        }
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_lrc() {
        let lrc = "[00:00.00]First line\n[00:12.50]Second line\n[01:03]Third line";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].time, Duration::from_millis(12_500));
        assert_eq!(lines[2].time, Duration::from_secs(63));
    }

    #[test]
    fn parses_multiple_timestamps() {
        let lrc = "[00:10.00][00:20.00]Repeated line";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Repeated line");
        assert_eq!(lines[1].time, Duration::from_secs(20));
    }

    #[test]
    fn ignores_metadata_tags() {
        let lrc = "[ar:Artist]\n[ti:Title]\n[00:01.00]Lyric";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Lyric");
    }

    #[test]
    fn parses_lrclib_camel_case_fields() {
        let body = r#"{"syncedLyrics":"[00:01.00]Hi","plainLyrics":"Hi"}"#;
        let response: LrclibResponse = serde_json::from_str(body).unwrap();
        assert_eq!(response.synced_lyrics.as_deref(), Some("[00:01.00]Hi"));
        assert_eq!(response.plain_lyrics.as_deref(), Some("Hi"));
    }

    #[test]
    fn active_line_follows_position() {
        let lyrics = Lyrics {
            lines: vec![
                LyricLine { time: Duration::from_secs(0), text: "a".into() },
                LyricLine { time: Duration::from_secs(10), text: "b".into() },
                LyricLine { time: Duration::from_secs(20), text: "c".into() },
            ],
            synced: true,
            source: "test".into(),
        };
        assert_eq!(active_line(&lyrics, Duration::from_secs(5)), Some(0));
        assert_eq!(active_line(&lyrics, Duration::from_secs(15)), Some(1));
        assert_eq!(active_line(&lyrics, Duration::from_secs(25)), Some(2));
    }
}
