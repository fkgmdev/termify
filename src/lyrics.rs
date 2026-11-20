//! Lyrics fetching via LRCLIB (free, no API key) with LRC timestamp parsing.
//! Results are cached in-memory by track URI in `App`.
//!
//! LRCLIB's `/api/get` sometimes returns a translated entry instead of the
//! original-language one (notably for DECO*27's モニタリング). When the track
//! title uses a non-Latin script but the fetched lyrics do not, we fall back to
//! `/api/search` and pick the best metadata match that keeps the original
//! script. English translation on demand uses Google's public translate
//! endpoint; it is best-effort and cached per track by `App`.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::ui::Track;

const LRCLIB_GET_URL: &str = "https://lrclib.net/api/get";
const LRCLIB_SEARCH_URL: &str = "https://lrclib.net/api/search";
const TRANSLATE_URL: &str = "https://translate.googleapis.com/translate_a/single";
const USER_AGENT: &str = "termify/0.1 (local Spotify TUI)";
const TRANSLATE_BATCH_SIZE: usize = 10;

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

#[derive(Debug, Deserialize)]
struct LrclibSearchItem {
    #[serde(default, rename = "trackName")]
    track_name: String,
    #[serde(default, rename = "artistName")]
    artist_name: String,
    #[serde(default, rename = "albumName")]
    album_name: String,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default, rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
    #[serde(default, rename = "plainLyrics")]
    plain_lyrics: Option<String>,
}

/// True when the text uses CJK/kana/hangul characters. Used as a lightweight
/// signal that a track title or its lyrics are not in a Latin script.
fn uses_cjk_script(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c as u32,
            0x3040..=0x30FF // kana
                | 0x3400..=0x4DBF // CJK extension A
                | 0x4E00..=0x9FFF // CJK unified
                | 0xAC00..=0xD7AF // hangul
                | 0xF900..=0xFAFF // compatibility ideographs
        )
    })
}

fn lyrics_text(lyrics: &Lyrics) -> String {
    lyrics
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fetch lyrics for a track from LRCLIB. Returns `None` when no lyrics exist.
pub async fn fetch(client: &reqwest::Client, track: &Track) -> Result<Option<Lyrics>> {
    let primary = fetch_get(client, track).await?;

    // If the title has a non-Latin script but the lyrics came back in Latin
    // text, `/api/get` probably matched a translation. Prefer the original.
    let looks_translated = primary.as_ref().is_some_and(|lyrics| {
        uses_cjk_script(&track.title) && !uses_cjk_script(&lyrics_text(lyrics))
    });
    if looks_translated || primary.is_none() {
        if let Some(original) = fetch_original_from_search(client, track).await? {
            return Ok(Some(original));
        }
    }

    Ok(primary)
}

async fn fetch_get(client: &reqwest::Client, track: &Track) -> Result<Option<Lyrics>> {
    let duration_secs = track.duration.as_secs();
    let response = client
        .get(LRCLIB_GET_URL)
        .query(&[
            ("track_name", track.title.as_str()),
            ("artist_name", track.artist.as_str()),
            ("album_name", track.album.as_str()),
            ("duration", &duration_secs.to_string()),
        ])
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("LRCLIB request failed")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response.error_for_status().context("LRCLIB returned an error")?;
    let body: LrclibResponse = response.json().await.context("parsing LRCLIB response")?;

    Ok(lyrics_from_parts(
        body.synced_lyrics,
        body.plain_lyrics,
        "LRCLIB",
    ))
}

/// Search LRCLIB and choose the candidate that best matches the track's
/// metadata, preferring one that keeps the original script.
async fn fetch_original_from_search(
    client: &reqwest::Client,
    track: &Track,
) -> Result<Option<Lyrics>> {
    let response = client
        .get(LRCLIB_SEARCH_URL)
        .query(&[
            ("track_name", track.title.as_str()),
            ("artist_name", track.artist.as_str()),
        ])
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("LRCLIB search failed")?;
    let response = response
        .error_for_status()
        .context("LRCLIB search returned an error")?;
    let items: Vec<LrclibSearchItem> = response
        .json()
        .await
        .context("parsing LRCLIB search response")?;

    let want_cjk = uses_cjk_script(&track.title);
    let mut best: Option<(i32, Lyrics)> = None;
    for item in items {
        let score = score_candidate(&item, track);
        let Some(lyrics) = lyrics_from_parts(
            item.synced_lyrics,
            item.plain_lyrics,
            "LRCLIB (original)",
        ) else {
            continue;
        };
        if score < 0 || !wants_original_script(want_cjk, &lyrics) {
            continue;
        }
        if best.as_ref().is_none_or(|(best_score, _)| score > *best_score) {
            best = Some((score, lyrics));
        }
    }

    Ok(best.map(|(_, lyrics)| lyrics))
}

fn wants_original_script(want_cjk: bool, lyrics: &Lyrics) -> bool {
    uses_cjk_script(&lyrics_text(lyrics)) == want_cjk
}

fn score_candidate(item: &LrclibSearchItem, track: &Track) -> i32 {
    let mut score = 0;
    let title = item.track_name.to_lowercase();
    let wanted_title = track.title.to_lowercase();
    if title == wanted_title {
        score += 100;
    } else if title.contains(&wanted_title) || wanted_title.contains(&title) {
        score += 40;
    } else {
        score -= 100;
    }

    let artist = item.artist_name.to_lowercase();
    let wanted_artist = track.artist.to_lowercase();
    if artist.contains(&wanted_artist) || wanted_artist.contains(&artist) {
        score += 80;
    } else {
        score -= 80;
    }

    let album = item.album_name.to_lowercase();
    let wanted_album = track.album.to_lowercase();
    if album == wanted_album {
        score += 30;
    } else if !album.is_empty() && (album.contains(&wanted_album) || wanted_album.contains(&album)) {
        score += 15;
    }

    if let Some(duration) = item.duration {
        let diff = (duration - track.duration.as_secs_f64()).abs();
        if diff <= 2.0 {
            score += 50;
        } else if diff <= 5.0 {
            score += 20;
        } else if diff > 15.0 {
            score -= 50;
        }
    }
    score
}

fn lyrics_from_parts(
    synced: Option<String>,
    plain: Option<String>,
    source: &str,
) -> Option<Lyrics> {
    if let Some(synced) = synced {
        let lines = parse_lrc(&synced);
        if !lines.is_empty() {
            return Some(Lyrics {
                lines,
                synced: true,
                source: source.to_string(),
            });
        }
    }

    if let Some(plain) = plain {
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
            return Some(Lyrics {
                lines,
                synced: false,
                source: format!("{source} (unsynced)"),
            });
        }
    }

    None
}

/// Translate lyric lines to English. Batched to keep requests few while still
/// aligning each translated line with its original timestamp.
pub async fn translate_lines(
    client: &reqwest::Client,
    lines: &[LyricLine],
    target_lang: &str,
) -> Result<Vec<String>> {
    let mut translated = Vec::with_capacity(lines.len());
    for batch in lines.chunks(TRANSLATE_BATCH_SIZE) {
        let texts: Vec<&str> = batch.iter().map(|line| line.text.as_str()).collect();
        match translate_batch(client, &texts, target_lang).await {
            Ok(result) if result.len() == batch.len() => translated.extend(result),
            _ => {
                // Fall back to one request per line when the service groups
                // lines differently, so timing stays aligned.
                for text in texts {
                    let single = translate_batch(client, &[text], target_lang)
                        .await
                        .ok()
                        .and_then(|mut result| result.pop())
                        .unwrap_or_else(|| text.to_string());
                    translated.push(single);
                }
            }
        }
    }
    Ok(translated)
}

async fn translate_batch(
    client: &reqwest::Client,
    texts: &[&str],
    target_lang: &str,
) -> Result<Vec<String>> {
    let query = texts.join("\n");
    let response = client
        .get(TRANSLATE_URL)
        .query(&[
            ("client", "gtx"),
            ("sl", "auto"),
            ("tl", target_lang),
            ("dt", "t"),
            ("q", query.as_str()),
        ])
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("translation request failed")?
        .error_for_status()
        .context("translation service returned an error")?;
    let body: serde_json::Value = response
        .json()
        .await
        .context("parsing translation response")?;
    parse_translation_response(&body)
}

fn parse_translation_response(body: &serde_json::Value) -> Result<Vec<String>> {
    let segments = body
        .get(0)
        .and_then(|value| value.as_array())
        .context("unexpected translation response")?;
    let mut combined = String::new();
    for segment in segments {
        if let Some(text) = segment.get(0).and_then(|value| value.as_str()) {
            combined.push_str(text);
        }
    }
    Ok(combined
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect())
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
    fn detects_cjk_script() {
        assert!(uses_cjk_script("モニタリング"));
        assert!(uses_cjk_script("ねえあたし知ってるよ"));
        assert!(!uses_cjk_script("Knee Socks"));
    }

    #[test]
    fn parses_translation_segments() {
        let body: serde_json::Value = serde_json::from_str(
            r#"[[["Hey, I know\n","ねえあたし知ってるよ\n",null,null,3],["I know\n","きみがひとり\"XX\"してるの知ってるよ\n",null,null,3]],null,"ja"]"#,
        )
        .unwrap();
        let lines = parse_translation_response(&body).unwrap();
        assert_eq!(lines, vec!["Hey, I know", "I know"]);
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
