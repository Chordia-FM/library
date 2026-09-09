//! Lyrics for Discord: the file's own tags, else the Hub's, shaped into pages.
//!
//! The scanner has stored `tracks.lyrics` since migration 0005. Tags carry either plain text or
//! LRC (`[mm:ss.xx]` per line, sometimes `[ar:]`/`[ti:]` metadata); the bot shows the words, so
//! timestamps and metadata are stripped, and the text is cut into pages that fit a text display
//! with room for the header. A file with no tags asks the Hub, which serves its cache or fetches
//! from the provider, the same as the web client.

use crate::catalog::{self, TrackRow};
use crate::http::AppState;

/// Discord's text display holds 4000 characters; the page stays under it with the header's share.
pub const PAGE_CHARS: usize = 3500;

/// The words for a track: the file's own tags first, else what the Hub has (its cache, or the
/// provider on a miss).
pub async fn text_for(state: &AppState, track: &TrackRow) -> Option<String> {
    if let Ok(Some(tagged)) = catalog::get_track_lyrics(&state.db, &track.id).await {
        return Some(tagged);
    }
    super::hub::lyrics(state, track).await
}

/// Lines of lyrics without LRC timestamps or metadata tags, blank runs collapsed to one.
pub fn lines(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let mut rest = line.trim_end();
        // Any number of leading `[..]` tags: timestamps `[01:23.45]`, metadata `[ar:Artist]`.
        let mut had_tag = false;
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            rest = stripped[end + 1..].trim_start();
            had_tag = true;
        }
        let text = rest.trim();
        if text.is_empty() {
            if had_tag {
                // A bare timestamp is an instrumental gap in LRC; keep it as a blank.
                continue;
            }
            if !out.last().is_some_and(String::is_empty) && !out.is_empty() {
                out.push(String::new());
            }
            continue;
        }
        out.push(text.to_string());
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

/// Pages of at most `max_chars`, split on line boundaries.
pub fn pages(lines: &[String], max_chars: usize) -> Vec<String> {
    let mut pages = Vec::new();
    let mut current = String::new();
    for line in lines {
        let extra = line.chars().count() + 1;
        if !current.is_empty() && current.chars().count() + extra > max_chars {
            pages.push(current.trim_end().to_string());
            current = String::new();
        }
        if extra > max_chars {
            // One absurdly long line: hard-cut it rather than refuse.
            let cut: String = line.chars().take(max_chars - 1).collect();
            pages.push(cut);
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        pages.push(current.trim_end().to_string());
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_is_stripped_to_words() {
        let raw = "[ar:Daft Punk]\n[ti:One More Time]\n[00:12.00]One more time\n[00:15.50]\n[00:18.00][00:40.00]We're gonna celebrate\n\n\n\nOh yeah\n";
        assert_eq!(
            lines(raw),
            vec!["One more time", "We're gonna celebrate", "", "Oh yeah"]
        );
    }

    #[test]
    fn plain_text_keeps_verses_apart() {
        let raw = "Verse one\nline two\n\nChorus\n";
        assert_eq!(lines(raw), vec!["Verse one", "line two", "", "Chorus"]);
        assert!(lines("   \n\n").is_empty());
    }

    #[test]
    fn pages_split_on_lines() {
        let ls: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let p = pages(&ls, 20);
        assert!(p.len() > 1);
        assert!(p.iter().all(|pg| pg.chars().count() <= 20));
        assert!(p[0].starts_with("line 0"));
        let joined: Vec<String> = p
            .iter()
            .flat_map(|pg| pg.lines().map(str::to_string))
            .collect();
        assert_eq!(joined, ls);
        let long = vec!["x".repeat(50)];
        assert_eq!(pages(&long, 10)[0].chars().count(), 9);
    }
}
