//! Song lyrics from LRCLIB (<https://lrclib.net>): a free, open database that needs no key.
//!
//! Synced lyrics come as LRC (`[mm:ss.xx] line`) and are parsed into timed lines so the
//! player can highlight the line that is playing. Tracks without a synced version fall back
//! to the plain text, which is shown without highlighting.

use super::connectors::http_client;

/// One lyric line. `ms` is 0 for unsynced lyrics.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct Lyrics {
    /// Timed lines (empty when nothing was found).
    pub lines: Vec<Line>,
    /// The lines carry timestamps and can be followed along.
    pub synced: bool,
    /// The song has no words (LRCLIB marks these as instrumental).
    pub instrumental: bool,
}

impl Lyrics {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && !self.instrumental
    }
}

/// Cache key for a song: the lyrics of "Artist – Title" are the same wherever it plays from.
pub fn key(artist: &str, title: &str) -> String {
    format!("{}|{}", artist.trim().to_lowercase(), title.trim().to_lowercase())
}

/// Build the lyrics from what LRCLIB stored (the two strings are what the cache keeps).
pub fn from_strings(synced: &str, plain: &str, instrumental: bool) -> Lyrics {
    if instrumental {
        return Lyrics { lines: vec![], synced: false, instrumental: true };
    }
    let timed = parse_lrc(synced);
    if !timed.is_empty() {
        return Lyrics { lines: timed, synced: true, instrumental: false };
    }
    let lines = plain
        .lines()
        .map(|l| Line { ms: 0, text: l.trim().to_string() })
        .collect::<Vec<_>>();
    // Drop trailing blank lines but keep the ones between verses.
    let end = lines.iter().rposition(|l| !l.text.is_empty()).map(|i| i + 1).unwrap_or(0);
    Lyrics { lines: lines[..end].to_vec(), synced: false, instrumental: false }
}

/// `[mm:ss.xx] text` -> timed lines. Metadata tags (`[ar:…]`) and untimed lines are skipped.
pub fn parse_lrc(text: &str) -> Vec<Line> {
    let mut out: Vec<Line> = vec![];
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps: Vec<i64> = vec![];
        // A line can carry several timestamps ("[00:12.00][01:30.00] chorus").
        while let Some(end) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
            let inside = &rest[1..end + 1];
            match parse_stamp(inside) {
                Some(ms) => stamps.push(ms),
                // A metadata tag: not a timestamp, so the line is not a lyric line.
                None => break,
            }
            rest = rest[end + 2..].trim_start();
        }
        for ms in stamps {
            out.push(Line { ms, text: rest.trim().to_string() });
        }
    }
    out.sort_by_key(|l| l.ms);
    out
}

/// "mm:ss.xx" / "mm:ss" / "h:mm:ss.xx" -> milliseconds.
fn parse_stamp(s: &str) -> Option<i64> {
    let mut total = 0f64;
    let mut parts = 0;
    for part in s.split(':') {
        let v: f64 = part.trim().replace(',', ".").parse().ok()?;
        if v < 0.0 {
            return None;
        }
        total = total * 60.0 + v;
        parts += 1;
    }
    (parts >= 2).then(|| (total * 1000.0).round() as i64)
}

/// The line that is playing at `pos_ms`, or None before the first one.
pub fn line_at(lines: &[Line], pos_ms: i64) -> Option<usize> {
    if lines.is_empty() || pos_ms < lines[0].ms {
        return None;
    }
    // The last line whose timestamp has passed.
    Some(lines.partition_point(|l| l.ms <= pos_ms).saturating_sub(1))
}

/// What LRCLIB answered, kept as-is so the cache can store it.
#[derive(Debug, Clone, Default)]
pub struct Found {
    pub synced: String,
    pub plain: String,
    pub instrumental: bool,
}

/// Look a song up: the exact match first, then a search. `duration_ms` narrows the search
/// down to the right version of a song; 0 means it is unknown.
pub fn fetch(artist: &str, title: &str, album: &str, duration_ms: i64) -> Option<Found> {
    let client = http_client();
    let secs = duration_ms / 1000;
    let mut query: Vec<(&str, String)> =
        vec![("artist_name", artist.to_string()), ("track_name", title.to_string())];
    if !album.is_empty() {
        query.push(("album_name", album.to_string()));
    }
    if secs > 0 {
        query.push(("duration", secs.to_string()));
    }
    if let Ok(resp) = client.get("https://lrclib.net/api/get").query(&query).send() {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<serde_json::Value>() {
                let found = found_from(&v);
                // An entry that carries no words at all is no answer; fall through to the
                // search, where another release of the song may have them.
                if found.instrumental || !found.synced.is_empty() || !found.plain.is_empty() {
                    return Some(found);
                }
            }
        }
    }
    // No exact match: search and take the closest duration.
    let resp = client
        .get("https://lrclib.net/api/search")
        .query(&[("artist_name", artist), ("track_name", title)])
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let list = resp.json::<Vec<serde_json::Value>>().ok()?;
    let best = list.iter().min_by_key(|v| {
        let d = (v["duration"].as_f64().unwrap_or(0.0) * 1000.0) as i64;
        if duration_ms > 0 { (d - duration_ms).abs() } else { 0 }
    })?;
    // A wildly different length is a different song, not another release of this one.
    let d = (best["duration"].as_f64().unwrap_or(0.0) * 1000.0) as i64;
    if duration_ms > 0 && d > 0 && (d - duration_ms).abs() > 20_000 {
        return None;
    }
    Some(found_from(best))
}

fn found_from(v: &serde_json::Value) -> Found {
    Found {
        synced: v["syncedLyrics"].as_str().unwrap_or("").to_string(),
        plain: v["plainLyrics"].as_str().unwrap_or("").to_string(),
        instrumental: v["instrumental"].as_bool().unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lrc_timestamps() {
        let lrc = "[ar:Someone]\n[00:12.50] first line\n[01:02.25] second\n[invalid] not timed";
        let lines = parse_lrc(lrc);
        assert_eq!(lines, vec![
            Line { ms: 12_500, text: "first line".into() },
            Line { ms: 62_250, text: "second".into() },
        ]);
    }

    #[test]
    fn repeated_timestamps_become_separate_lines() {
        let lines = parse_lrc("[00:10.00][00:40.00] chorus");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].ms, 10_000);
        assert_eq!(lines[1].ms, 40_000);
        assert_eq!(lines[1].text, "chorus");
    }

    #[test]
    fn picks_the_line_that_is_playing() {
        let lines = parse_lrc("[00:05.00] a\n[00:10.00] b\n[00:20.00] c");
        assert_eq!(line_at(&lines, 0), None);
        assert_eq!(line_at(&lines, 4_999), None);
        assert_eq!(line_at(&lines, 5_000), Some(0));
        assert_eq!(line_at(&lines, 19_999), Some(1));
        assert_eq!(line_at(&lines, 60_000), Some(2));
        assert_eq!(line_at(&[], 1_000), None);
    }

    #[test]
    fn plain_lyrics_are_untimed() {
        let l = from_strings("", "one\ntwo\n\n", false);
        assert!(!l.synced);
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[1].text, "two");
        // Synced wins when both are there.
        let both = from_strings("[00:01.00] hi", "hi\nthere", false);
        assert!(both.synced && both.lines.len() == 1);
        // An instrumental has no lines but is still an answer.
        let inst = from_strings("", "", true);
        assert!(inst.instrumental && !inst.is_empty());
    }

    /// Asks LRCLIB itself; run with `cargo test -- --ignored --nocapture lyrics`.
    #[test]
    #[ignore = "needs the internet"]
    fn fetches_a_known_song() {
        let found = fetch("a-ha", "Take On Me", "", 225_000).expect("LRCLIB has this one");
        let lyrics = from_strings(&found.synced, &found.plain, found.instrumental);
        println!("{} lines, synced: {}", lyrics.lines.len(), lyrics.synced);
        println!("first: {:?}", lyrics.lines.first());
        assert!(lyrics.lines.len() > 10);
        assert!(lyrics.lines.iter().any(|l| l.text.to_lowercase().contains("take on me")));
    }

    #[test]
    fn key_ignores_case_and_spacing() {
        assert_eq!(key(" Megadeth ", "Symphony Of Destruction"), key("megadeth", "symphony of destruction"));
    }
}
