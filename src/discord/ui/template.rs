//! The template language every layout text is written in, and the catalogue of what it can say.
//!
//! A template is Discord markdown with variables in braces: `{track}`, `{channel}`,
//! `{progress_bar:12}`, `{emoji:listening}`. A variable is a lowercase name, optionally followed
//! by a colon and an argument. Anything else in braces is left exactly as written, so a typo is
//! visible instead of silently blank, and a JSON snippet or a `{}` in a title survives.
//!
//! Rendering is forgiving about what is missing: a variable that has nothing to say (no album,
//! nobody asked, no web client to link to) renders empty, and then the middle-dot separators
//! around it are tidied, so `{played_for} · {requested_by}` reads well whether one, both or
//! neither is known. A `-#` line left with nothing after it disappears.
//!
//! The catalogue ([`VARIABLES`]) is what the dashboard's autocomplete offers; the descriptions
//! are written for the person typing the template.

use serde::Serialize;

use crate::discord::emoji::Icon;

/// One variable the dashboard can offer, with where it applies.
#[derive(Debug, Clone, Serialize)]
pub struct VariableInfo {
    pub name: String,
    pub description: &'static str,
    /// Views the variable means something in; empty = every view. Uses the wire names
    /// (`now_playing`, `queue`, …); `item` marks the list line of `queue` / `history`.
    pub scopes: &'static [&'static str],
    /// A `{name:arg}` form, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg: Option<ArgInfo>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ArgInfo {
    pub label: &'static str,
    pub min: u8,
    pub max: u8,
    pub default: u8,
}

const ANY: &[&str] = &[];
const TRACK: &[&str] = &["now_playing", "queued", "item"];
const PLAYING: &[&str] = &["now_playing"];
const BAR: Option<ArgInfo> = Some(ArgInfo {
    label: "segments",
    min: 4,
    max: 20,
    default: 12,
});

/// The catalogue, in the order the dashboard lists it.
pub fn variables() -> Vec<VariableInfo> {
    fn v(
        name: &str,
        description: &'static str,
        scopes: &'static [&'static str],
        arg: Option<ArgInfo>,
    ) -> VariableInfo {
        VariableInfo {
            name: name.to_string(),
            description,
            scopes,
            arg,
        }
    }
    let mut out = vec![
        v(
            "icon",
            "This message's icon (play, pause, a note, a wave…)",
            ANY,
            None,
        ),
        v(
            "heading",
            "This message's title, e.g. \"Now playing\" or \"Added to queue\"",
            ANY,
            None,
        ),
        v("bot", "The bot's name", ANY, None),
        v(
            "channel",
            "The voice channel, as a clickable mention",
            ANY,
            None,
        ),
        v("channel_name", "The voice channel's name, plain", ANY, None),
        v(
            "listeners",
            "How many people are in the voice channel",
            ANY,
            None,
        ),
        v(
            "queue_count",
            "How many tracks are queued, as a number",
            ANY,
            None,
        ),
        v("queue_tracks", "\"3 tracks\"", ANY, None),
        v(
            "queue_duration",
            "How long the queue runs, e.g. 12:34",
            ANY,
            None,
        ),
        v("volume", "The volume as a number, e.g. 80", ANY, None),
        v("volume_bar", "The volume as a bar of segments", ANY, BAR),
        v("loop", "The loop mode: off, track or queue", ANY, None),
        v("shuffle", "Shuffle: on or off", ANY, None),
        v("autoplay", "Autoplay: on or off", ANY, None),
        v(
            "meta",
            "Who asked, the queue count, the volume and the modes, in one line",
            PLAYING,
            None,
        ),
        v(
            "now_playing_line",
            "What is playing, one line, or \"Nothing playing\"",
            ANY,
            None,
        ),
        v(
            "track",
            "The track: bold title, then artist and album, linked",
            TRACK,
            None,
        ),
        v(
            "track_line",
            "The track on one line: bold title · artist, linked",
            TRACK,
            None,
        ),
        v("title", "The track's title, plain", TRACK, None),
        v("artist", "The track's artist, plain", TRACK, None),
        v("album", "The track's album, plain", TRACK, None),
        v(
            "title_link",
            "The web client address for the track",
            TRACK,
            None,
        ),
        v(
            "artist_link",
            "The web client address for the artist",
            TRACK,
            None,
        ),
        v(
            "album_link",
            "The web client address for the album",
            TRACK,
            None,
        ),
        v("duration", "The track's length, e.g. 3:45", TRACK, None),
        v(
            "position",
            "How far into the track, e.g. 1:05",
            PLAYING,
            None,
        ),
        v(
            "progress_bar",
            "How far into the track, as a bar of segments",
            PLAYING,
            BAR,
        ),
        v(
            "badges",
            "The file's quality: codec, rate, lossless, Opus bitrate, ReplayGain",
            PLAYING,
            None,
        ),
        v(
            "requested_by",
            "Who asked for the track, as a mention (or \"Autoplay\")",
            TRACK,
            None,
        ),
        v(
            "added",
            "What was added: the track, or the album or artist and a count",
            &["queued"],
            None,
        ),
        v(
            "added_meta",
            "Where it sits, when it plays and who asked, in one line",
            &["queued"],
            None,
        ),
        v("count", "How many tracks were added", &["queued"], None),
        v(
            "queue_position",
            "The number the first added track has in the queue",
            &["queued"],
            None,
        ),
        v(
            "eta",
            "How long until the added track plays",
            &["queued", "item"],
            None,
        ),
        v(
            "source",
            "The album or artist the tracks came from, when several were added",
            &["queued"],
            None,
        ),
        v("reason", "Why the bot left", &["left"], None),
        v("index", "The entry's number in the list", &["item"], None),
        v(
            "played_at",
            "When the entry played, as a relative time",
            &["item"],
            None,
        ),
        v("played_for", "How much of it played", &["item"], None),
        v(
            "counted",
            "Whether it counted for anyone's listening history",
            &["item"],
            None,
        ),
        v(
            "page",
            "The page number, e.g. 2/5",
            &["queue", "history"],
            None,
        ),
    ];
    out.push(VariableInfo {
        name: "emoji:name".to_string(),
        description: "One of the bot's own icons by name, e.g. {emoji:listening}",
        scopes: ANY,
        arg: None,
    });
    out
}

/// The bot's icons a template may name with `{emoji:name}`.
#[derive(Debug, Clone, Serialize)]
pub struct EmojiInfo {
    pub name: &'static str,
    /// What the icon looks like without the emoji set, so the dashboard can show it.
    pub fallback: &'static str,
}

pub fn emojis() -> Vec<EmojiInfo> {
    Icon::named()
        .map(|(name, icon)| EmojiInfo {
            name,
            fallback: icon.fallback(),
        })
        .collect()
}

// ---- rendering -----------------------------------------------------------------------------------

/// A piece of a parsed template.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Literal(&'a str),
    Var { name: &'a str, arg: Option<&'a str> },
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn parse(template: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let (lit, after) = rest.split_at(open);
        if !lit.is_empty() {
            out.push(Piece::Literal(lit));
        }
        let inner_end = after[1..].find(['}', '{']);
        match inner_end {
            Some(end) if after.as_bytes()[1 + end] == b'}' => {
                let inner = &after[1..1 + end];
                let (name, arg) = match inner.split_once(':') {
                    Some((n, a)) => (n, Some(a)),
                    None => (inner, None),
                };
                if is_name(name) {
                    out.push(Piece::Var { name, arg });
                } else {
                    out.push(Piece::Literal(&after[..2 + end]));
                }
                rest = &after[2 + end..];
            }
            _ => {
                // An unclosed brace, or one opened again before it closed: literal.
                out.push(Piece::Literal(&after[..1]));
                rest = &after[1..];
            }
        }
    }
    if !rest.is_empty() {
        out.push(Piece::Literal(rest));
    }
    out
}

/// Fill `template` from `resolve`, which answers `None` for a variable it does not know (left as
/// written) and `Some("")` for one it knows but has nothing for.
pub fn render(
    template: &str,
    mut resolve: impl FnMut(&str, Option<&str>) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(template.len());
    for piece in parse(template) {
        match piece {
            Piece::Literal(s) => out.push_str(s),
            Piece::Var { name, arg } => match resolve(name, arg) {
                Some(v) => out.push_str(&v),
                None => {
                    out.push('{');
                    out.push_str(name);
                    if let Some(a) = arg {
                        out.push(':');
                        out.push_str(a);
                    }
                    out.push('}');
                }
            },
        }
    }
    tidy(&out)
}

/// The separators a template puts between variables, once the empty ones are gone: runs of
/// middle dots collapse to one, a line does not start or end with one, and a `-#` line with
/// nothing after it goes away. Lines that were empty in the template stay (a blank line is a
/// paragraph break in Discord).
pub fn tidy(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in s.split('\n') {
        let (prefix, body) = match line.trim_start().strip_prefix("-# ") {
            Some(rest) => ("-# ", rest),
            None => match line.trim_start().strip_prefix("-#") {
                Some(rest) if rest.trim().is_empty() => ("-# ", ""),
                _ => ("", line),
            },
        };
        let mut parts: Vec<&str> = body.split('·').map(str::trim).collect();
        let had_dots = parts.len() > 1;
        if had_dots {
            parts.retain(|p| !p.is_empty());
        }
        let joined = if had_dots {
            parts.join(" · ")
        } else {
            body.to_string()
        };
        if !prefix.is_empty() && joined.trim().is_empty() {
            continue;
        }
        lines.push(format!("{prefix}{joined}"));
    }
    lines.join("\n")
}

/// The count a `{…_bar:n}` argument asks for, within the allowed range.
pub fn bar_cells(arg: Option<&str>) -> usize {
    let n = arg.and_then(|a| a.trim().parse::<u8>().ok()).unwrap_or(12);
    n.clamp(4, 20) as usize
}

/// The names a template refers to, for deciding what to fetch before rendering.
pub fn mentions(template: &str, name: &str) -> bool {
    parse(template)
        .iter()
        .any(|p| matches!(p, Piece::Var { name: n, .. } if *n == name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(t: &str) -> String {
        render(t, |name, arg| match name {
            "title" => Some("One More Time".into()),
            "album" => Some(String::new()),
            "bar" => Some(format!("[{}]", bar_cells(arg))),
            _ => None,
        })
    }

    #[test]
    fn variables_are_filled_and_unknown_ones_kept() {
        assert_eq!(
            r("**{title}** {nope} {bar:6}"),
            "**One More Time** {nope} [6]"
        );
        assert_eq!(r("{bar} {bar:99} {bar:x}"), "[12] [20] [12]");
        assert_eq!(r("{} {{title}} { title }"), "{} {One More Time} { title }");
        assert_eq!(r("a { b"), "a { b");
        assert_eq!(r("{Title}"), "{Title}");
    }

    #[test]
    fn empty_variables_take_their_separators_with_them() {
        assert_eq!(r("{title} · {album} · x"), "One More Time · x");
        assert_eq!(r("{album} · {album}"), "");
        assert_eq!(r("-# {album}\nnext"), "next");
        assert_eq!(
            r("-# {album} · {title}\n\nnext"),
            "-# One More Time\n\nnext"
        );
        assert_eq!(r("a\n-#\nb"), "a\nb");
        assert_eq!(r("plain · text"), "plain · text");
    }

    #[test]
    fn mentions_sees_through_arguments() {
        assert!(mentions("{progress_bar:8}", "progress_bar"));
        assert!(!mentions("{progress_bar:8}", "progress"));
        assert!(!mentions("progress_bar", "progress_bar"));
    }

    #[test]
    fn the_catalogue_names_are_valid_and_unique() {
        let vars = variables();
        let mut names: Vec<&str> = vars.iter().map(|v| v.name.as_str()).collect();
        for n in &names {
            let bare = n.split(':').next().unwrap();
            assert!(is_name(bare), "{n}");
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), vars.len());
        assert!(emojis().iter().any(|e| e.name == "listening"));
    }
}
