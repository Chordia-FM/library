//! The template language every layout text is written in, and the catalogue of what it can say.
//!
//! A template is Discord markdown with variables in braces: `{track.title}`, `{channel}`,
//! `{player.progress_bar:12}`, `{emoji:listening}`. A variable is a lowercase dotted name,
//! optionally followed by a colon and an argument. Anything else in braces is left exactly as written, so a typo is
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

use chordia_contracts::discord_layout::{
    default_queued, BotLayouts, ContainerAccent, LayoutBlock, LayoutOverrides, LayoutView,
    SeparatorSpacing, ViewLayout, LAYOUT_VERSION,
};

use crate::discord::emoji::{Icon, IconSet};
use crate::discord::ui::v2::Emoji;

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
const TRACK: &[&str] = &[
    "now_playing",
    "queued",
    "queued_album",
    "queued_artist",
    "queued_playlist",
    "lyrics",
    "done",
    "notice",
    "error",
    "vote",
    "vote_passed",
    "item",
];
/// Where the file facts and the player line apply: wherever a track can be playing.
const PLAYING: &[&str] = &[
    "now_playing",
    "lyrics",
    "done",
    "notice",
    "error",
    "vote",
    "vote_passed",
];
const QUEUED: &[&str] = &["queued", "queued_album", "queued_artist", "queued_playlist"];
const ITEM: &[&str] = &["item"];
const PAGED: &[&str] = &["queue", "history", "lyrics"];
const REPLY: &[&str] = &["done", "notice", "error"];
const LYRICS: &[&str] = &["lyrics"];
const VOTE: &[&str] = &["vote", "vote_passed"];
const BAR: Option<ArgInfo> = Some(ArgInfo {
    label: "segments",
    min: 4,
    max: 20,
    default: 12,
});

/// The catalogue, in the order the dashboard lists it: a namespace per thing (`track.`, `file.`,
/// `player.`, …), every fact its own variable, and a bare name for the composed shortcut.
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
    vec![
        // the message
        v(
            "icon",
            "This message's icon: play, pause, a note, a wave",
            ANY,
            None,
        ),
        v(
            "heading",
            "This message's title, e.g. Now playing, Added to queue",
            ANY,
            None,
        ),
        v(
            "detail",
            "What the reply says under its title, when anything",
            REPLY,
            None,
        ),
        v(
            "emoji:name",
            "One of the bot's icons by name, e.g. {emoji:listening}",
            ANY,
            None,
        ),
        // the bot, the server, the channel
        v("bot", "The bot's name", ANY, None),
        v("bot.name", "The bot's name", ANY, None),
        v("bot.mention", "The bot, as a mention", ANY, None),
        v("bot.avatar", "The address of the bot's avatar", ANY, None),
        v("server", "The server's name", ANY, None),
        v(
            "channel",
            "The voice channel, as a clickable mention",
            ANY,
            None,
        ),
        v("channel.name", "The voice channel's name, plain", ANY, None),
        v(
            "channel.listeners",
            "How many people are in the voice channel",
            ANY,
            None,
        ),
        // the track
        v(
            "track",
            "Bold title, then artist and album, each linked",
            TRACK,
            None,
        ),
        v(
            "track.line",
            "One line: bold title · artist, linked",
            TRACK,
            None,
        ),
        v("track.title", "The title", TRACK, None),
        v("track.artist", "The artist", TRACK, None),
        v("track.album", "The album", TRACK, None),
        v("track.album_artist", "The album's artist", TRACK, None),
        v("track.year", "The album's year", TRACK, None),
        v("track.genre", "The album's genre", TRACK, None),
        v("track.number", "The track number on the album", TRACK, None),
        v("track.disc", "The disc number", TRACK, None),
        v("track.duration", "The length, e.g. 3:45", TRACK, None),
        v(
            "track.url",
            "The web client address for the track",
            TRACK,
            None,
        ),
        v(
            "track.artist_url",
            "The web client address for the artist",
            TRACK,
            None,
        ),
        v(
            "track.album_url",
            "The web client address for the album",
            TRACK,
            None,
        ),
        v("track.title_link", "The title as a link", TRACK, None),
        v("track.artist_link", "The artist as a link", TRACK, None),
        v("track.album_link", "The album as a link", TRACK, None),
        // the file
        v("file", "The quality badges in one line", PLAYING, None),
        v("file.codec", "FLAC, MP3, Opus…", PLAYING, None),
        v("file.sample_rate", "e.g. 44.1 kHz", PLAYING, None),
        v(
            "file.bit_depth",
            "e.g. 16-bit; empty for lossy files",
            PLAYING,
            None,
        ),
        v("file.channels", "stereo, mono, or the count", PLAYING, None),
        v(
            "file.quality",
            "Lossless or Atmos, else empty",
            PLAYING,
            None,
        ),
        v(
            "file.bitrate",
            "The Opus bitrate the channel gets, e.g. 96 kbps",
            PLAYING,
            None,
        ),
        v("file.gain", "The ReplayGain, e.g. −7.1 dB", PLAYING, None),
        // the player
        v("player.status", "playing, paused or idle", ANY, None),
        v(
            "player.position",
            "How far into the track, e.g. 1:05",
            PLAYING,
            None,
        ),
        v(
            "player.remaining",
            "How much of the track is left",
            PLAYING,
            None,
        ),
        v(
            "player.progress_bar",
            "How far into the track, as a bar of segments",
            PLAYING,
            BAR,
        ),
        v(
            "player.volume",
            "The volume as a number, e.g. 80",
            ANY,
            None,
        ),
        v(
            "player.volume_bar",
            "The volume as a bar of segments",
            ANY,
            BAR,
        ),
        v("player.loop", "off, track or queue", ANY, None),
        v("player.shuffle", "on or off", ANY, None),
        v("player.autoplay", "on or off", ANY, None),
        v("player.muted", "on or off", ANY, None),
        v(
            "player.eq",
            "The equalizer: off, a preset's name, or custom",
            ANY,
            None,
        ),
        v(
            "player.meta",
            "Who asked, the queue, the volume and the modes, in one line",
            PLAYING,
            None,
        ),
        v(
            "player.line",
            "What is playing, on one line, or Nothing playing",
            ANY,
            None,
        ),
        // the queue
        v(
            "queue.count",
            "How many tracks are queued, as a number",
            ANY,
            None,
        ),
        v("queue.tracks", "e.g. 3 tracks", ANY, None),
        v(
            "queue.duration",
            "How long the queue runs, e.g. 12:34",
            ANY,
            None,
        ),
        v("queue.next", "The next track, on one line", ANY, None),
        // who asked
        v(
            "requester",
            "Who asked for the track, as a mention (or Autoplay)",
            TRACK,
            None,
        ),
        v("requester.id", "Their Discord id", TRACK, None),
        // what was added
        v(
            "added",
            "What was added: the track, or the album or artist and a count",
            QUEUED,
            None,
        ),
        v(
            "added.meta",
            "Where it sits, when it plays and who asked, in one line",
            QUEUED,
            None,
        ),
        v("added.count", "How many tracks were added", QUEUED, None),
        v(
            "added.position",
            "The queue number of the first added track",
            QUEUED,
            None,
        ),
        v("added.eta", "How long until it plays", QUEUED, None),
        v(
            "added.duration",
            "How long everything added runs",
            QUEUED,
            None,
        ),
        v(
            "added.source",
            "The album or artist the tracks came from",
            QUEUED,
            None,
        ),
        // why the bot left
        v("left.reason", "Why the bot left", &["left"], None),
        // lyrics
        v("lyrics", "This page of the lyrics", LYRICS, None),
        // a vote to skip
        v("vote.by", "Who just voted, as a mention", VOTE, None),
        v("vote.count", "Votes so far", VOTE, None),
        v(
            "vote.needed",
            "Votes the server's rule asks for",
            VOTE,
            None,
        ),
        v("vote.remaining", "Votes still missing", VOTE, None),
        v(
            "vote.listeners",
            "People in the voice channel, bots aside",
            VOTE,
            None,
        ),
        v(
            "vote.percent",
            "The share of listeners a vote needs",
            VOTE,
            None,
        ),
        // list entries
        v("index", "The entry's number in the list", ITEM, None),
        v("eta", "How long until the entry plays", ITEM, None),
        v("play.at", "When it played, as a relative time", ITEM, None),
        v("play.length", "How much of it played", ITEM, None),
        v(
            "play.counted",
            "Whether it counted for anyone's listening history",
            ITEM,
            None,
        ),
        v(
            "play.counted_for",
            "How many listeners it counted for",
            ITEM,
            None,
        ),
        // pages
        v("page", "The page, e.g. 2/5", PAGED, None),
        v("page.number", "The page number", PAGED, None),
        v("page.count", "How many pages there are", PAGED, None),
    ]
}

/// The bot's icons a template may name with `{emoji:name}`.
#[derive(Debug, Clone, Serialize)]
pub struct EmojiInfo {
    pub name: &'static str,
    /// What the icon looks like without the emoji set, so the dashboard can show it.
    pub fallback: &'static str,
    /// The application emoji's id once the set is live, so the dashboard can show the real one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

pub fn emojis(icons: &IconSet) -> Vec<EmojiInfo> {
    Icon::named()
        .map(|(name, icon)| EmojiInfo {
            name,
            fallback: icon.fallback(),
            id: match icons.get(icon) {
                Emoji::Custom { id, .. } => Some(id.to_string()),
                Emoji::Unicode(_) => None,
            },
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

/// `track.title`, `player.progress_bar`: lowercase words, joined by dots.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.ends_with('.')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
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

// ---- older layouts --------------------------------------------------------------------------------

/// The name a variable had before the namespaces, for what was saved then.
fn renamed(view: LayoutView, name: &str) -> Option<&'static str> {
    Some(match name {
        "channel_name" => "channel.name",
        "listeners" => "channel.listeners",
        "queue_count" => "queue.count",
        "queue_tracks" => "queue.tracks",
        "queue_duration" => "queue.duration",
        "volume" => "player.volume",
        "volume_bar" => "player.volume_bar",
        "loop" => "player.loop",
        "shuffle" => "player.shuffle",
        "autoplay" => "player.autoplay",
        "meta" => "player.meta",
        "now_playing_line" => "player.line",
        "position" => "player.position",
        "progress_bar" => "player.progress_bar",
        "track_line" => "track.line",
        "title" => "track.title",
        "artist" => "track.artist",
        "album" => "track.album",
        "title_link" => "track.url",
        "artist_link" => "track.artist_url",
        "album_link" => "track.album_url",
        "duration" => "track.duration",
        "badges" => "file",
        "requested_by" => "requester",
        "added_meta" => "added.meta",
        "count" => "added.count",
        "queue_position" => "added.position",
        "eta" if view == LayoutView::Queued => "added.eta",
        "source" => "added.source",
        "reason" => "left.reason",
        "played_at" => "play.at",
        "played_for" => "play.length",
        "counted" => "play.counted",
        _ => return None,
    })
}

/// `text` with the variables it names brought up to date; everything else untouched.
pub fn modernize(view: LayoutView, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for piece in parse(text) {
        match piece {
            Piece::Literal(s) => out.push_str(s),
            Piece::Var { name, arg } => {
                out.push('{');
                out.push_str(renamed(view, name).unwrap_or(name));
                if let Some(a) = arg {
                    out.push(':');
                    out.push_str(a);
                }
                out.push('}');
            }
        }
    }
    out
}

fn modernize_block(view: LayoutView, block: &mut LayoutBlock) {
    match block {
        LayoutBlock::Text { content } => *content = modernize(view, content),
        LayoutBlock::Section { texts, .. } => {
            for t in texts {
                *t = modernize(view, t);
            }
        }
        LayoutBlock::List { item, empty, .. } => {
            *item = modernize(view, item);
            *empty = modernize(view, empty);
        }
        LayoutBlock::Container { blocks, .. } => {
            for b in blocks {
                modernize_block(view, b);
            }
        }
        LayoutBlock::Gallery { .. }
        | LayoutBlock::Separator { .. }
        | LayoutBlock::Row { .. }
        | LayoutBlock::Pager => {}
    }
}

/// A layout saved by an older build, brought up to what this one writes so it looks exactly as
/// it did. Each step is keyed to the version that introduced it: before 3, variables were renamed
/// and the container every message used to be drawn in was made explicit; before 4, the page
/// buttons came with the list and are now a block of their own, put after it.
fn upgrade_view(view: LayoutView, layout: &mut ViewLayout, from: u32) {
    if from < 3 {
        for b in &mut layout.blocks {
            modernize_block(view, b);
        }
        if !layout
            .blocks
            .iter()
            .any(|b| matches!(b, LayoutBlock::Container { .. }))
        {
            let blocks = std::mem::take(&mut layout.blocks);
            layout.blocks = vec![LayoutBlock::Container {
                accent: ContainerAccent::Bot,
                blocks,
            }];
        }
    }
    if from < 4
        && view.is_list()
        && !layout
            .flat()
            .iter()
            .any(|b| matches!(b, LayoutBlock::Pager))
    {
        add_pager(&mut layout.blocks);
    }
}

/// The gap and the page buttons that used to follow the list, after it wherever it is.
fn add_pager(blocks: &mut Vec<LayoutBlock>) -> bool {
    if let Some(i) = blocks
        .iter()
        .position(|b| matches!(b, LayoutBlock::List { .. }))
    {
        blocks.insert(i + 1, LayoutBlock::Pager);
        blocks.insert(
            i + 1,
            LayoutBlock::Separator {
                divider: false,
                spacing: SeparatorSpacing::Large,
            },
        );
        return true;
    }
    blocks.iter_mut().any(|b| match b {
        LayoutBlock::Container { blocks, .. } => add_pager(blocks),
        _ => false,
    })
}

/// Bring a bot's saved layouts up to date, once.
pub fn upgrade(layouts: &mut BotLayouts) {
    let from = layouts.version;
    if from >= LAYOUT_VERSION {
        return;
    }
    for view in LayoutView::ALL {
        upgrade_view(view, layouts.view_mut(view), from);
    }
    // Before 5 one toast served tracks, albums and artists; a toast someone shaped carries over
    // to the two new ones, so every add still looks as it did.
    if from < 5 && layouts.queued != default_queued() {
        layouts.queued_album = layouts.queued.clone();
        layouts.queued_artist = layouts.queued.clone();
    }
    if from < 6 && layouts.queued != default_queued() {
        layouts.queued_playlist = layouts.queued.clone();
    }
    layouts.version = LAYOUT_VERSION;
}

/// Bring a server's saved versions up to date, once.
pub fn upgrade_overrides(overrides: &mut LayoutOverrides) {
    let from = overrides.version;
    if from >= LAYOUT_VERSION {
        return;
    }
    for (view, slot) in [
        (LayoutView::NowPlaying, &mut overrides.now_playing),
        (LayoutView::Idle, &mut overrides.idle),
        (LayoutView::Queued, &mut overrides.queued),
        (LayoutView::Left, &mut overrides.left),
        (LayoutView::Queue, &mut overrides.queue),
        (LayoutView::History, &mut overrides.history),
    ] {
        if let Some(l) = slot {
            upgrade_view(view, l, from);
        }
    }
    if from < 5 {
        if let Some(q) = overrides.queued.clone() {
            overrides.queued_album.get_or_insert_with(|| q.clone());
            overrides.queued_artist.get_or_insert(q);
        }
    }
    if from < 6 {
        if let Some(q) = overrides.queued.clone() {
            overrides.queued_playlist.get_or_insert(q);
        }
    }
    overrides.version = LAYOUT_VERSION;
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
            "track.title" => Some("TRACK.TITLE".into()),
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
        assert_eq!(
            r("{Title} {.title} {title.} {a..b}"),
            "{Title} {.title} {title.} {a..b}"
        );
        assert_eq!(r("{track.title}"), "TRACK.TITLE");
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
    fn older_layouts_come_up_to_date_once() {
        let old = r####"{"now_playing":{"blocks":[
            {"kind":"text","content":"### {icon} {heading}\n-# {channel_name}"},
            {"kind":"section","content":"{track}\n-# {badges}","accessory":{"kind":"image","source":{"kind":"cover"}}},
            {"kind":"text","content":"{progress_bar:8} {position} / {duration}\n-# {meta}"}
        ]},"queued":{"blocks":[{"kind":"text","content":"{added} · {eta} · {requested_by}"}]},
        "queue":{"blocks":[{"kind":"list","item":"{index}. {track_line} · {eta}","empty":"-","page_size":5}]}}"####;
        let mut l: BotLayouts = serde_json::from_str(old).unwrap();
        assert_eq!(l.version, 0);
        upgrade(&mut l);
        assert_eq!(l.version, LAYOUT_VERSION);
        let LayoutBlock::Container { accent, blocks } = &l.now_playing.blocks[0] else {
            panic!("boxed");
        };
        assert_eq!(*accent, ContainerAccent::Bot);
        assert_eq!(blocks.len(), 3);
        let LayoutBlock::Text { content } = &blocks[0] else {
            panic!()
        };
        assert_eq!(content, "### {icon} {heading}\n-# {channel.name}");
        let LayoutBlock::Section { texts, .. } = &blocks[1] else {
            panic!()
        };
        assert_eq!(texts, &["{track}\n-# {file}".to_string()]);
        let LayoutBlock::Text { content } = &blocks[2] else {
            panic!()
        };
        assert_eq!(
            content,
            "{player.progress_bar:8} {player.position} / {track.duration}\n-# {player.meta}"
        );
        // `{eta}` means the toast's on the queued message and the entry's on the queue.
        let LayoutBlock::Container { blocks, .. } = &l.queued.blocks[0] else {
            panic!()
        };
        let LayoutBlock::Text { content } = &blocks[0] else {
            panic!()
        };
        assert_eq!(content, "{added} · {added.eta} · {requester}");
        let LayoutBlock::Container { blocks, .. } = &l.queue.blocks[0] else {
            panic!()
        };
        let LayoutBlock::List { item, .. } = &blocks[0] else {
            panic!()
        };
        assert_eq!(item, "{index}. {track.line} · {eta}");
        // The page buttons that used to come with the list are a block after it now.
        assert!(matches!(
            blocks[1],
            LayoutBlock::Separator { divider: false, .. }
        ));
        assert_eq!(blocks[2], LayoutBlock::Pager);
        assert_eq!(blocks.len(), 3);
        // Untouched views got the (already boxed) defaults, and a second pass changes nothing.
        let again = l.clone();
        upgrade(&mut l);
        assert_eq!(l, again);
        l.validate().unwrap();
        // A current save is left alone even when it has no container.
        let mut current = BotLayouts {
            idle: ViewLayout {
                blocks: vec![LayoutBlock::Text {
                    content: "{bot}".into(),
                }],
            },
            ..Default::default()
        };
        let before = current.clone();
        upgrade(&mut current);
        assert_eq!(current, before);
        let mut o = LayoutOverrides {
            version: 0,
            idle: Some(ViewLayout {
                blocks: vec![LayoutBlock::Text {
                    content: "{volume}".into(),
                }],
            }),
            ..Default::default()
        };
        upgrade_overrides(&mut o);
        assert_eq!(o.version, LAYOUT_VERSION);
        assert!(matches!(
            &o.idle.as_ref().unwrap().blocks[0],
            LayoutBlock::Container { blocks, .. }
                if matches!(&blocks[0], LayoutBlock::Text { content } if content == "{player.volume}")
        ));
    }

    #[test]
    fn a_version_three_list_keeps_its_shape_and_gains_page_buttons() {
        let list = || LayoutBlock::List {
            item: "{track.line}".into(),
            empty: "-".into(),
            page_size: 5,
        };
        let mut l = BotLayouts {
            version: 3,
            queue: ViewLayout {
                blocks: vec![
                    LayoutBlock::Text {
                        content: "### {heading}".into(),
                    },
                    list(),
                    LayoutBlock::Text {
                        content: "-# end".into(),
                    },
                ],
            },
            ..Default::default()
        };
        upgrade(&mut l);
        assert_eq!(l.version, LAYOUT_VERSION);
        // Not boxed: a version-three save chose to have no container.
        let kinds: Vec<&str> = l.queue.blocks.iter().map(|b| b.kind()).collect();
        assert_eq!(kinds, ["text", "list", "separator", "pager", "text"]);
        l.validate().unwrap();
        // A server's version inside a container gets them inside it too.
        let mut o = LayoutOverrides {
            version: 3,
            history: Some(ViewLayout {
                blocks: vec![LayoutBlock::Container {
                    accent: ContainerAccent::None,
                    blocks: vec![list()],
                }],
            }),
            ..Default::default()
        };
        upgrade_overrides(&mut o);
        let LayoutBlock::Container { blocks, .. } = &o.history.as_ref().unwrap().blocks[0] else {
            panic!()
        };
        let kinds: Vec<&str> = blocks.iter().map(|b| b.kind()).collect();
        assert_eq!(kinds, ["list", "separator", "pager"]);
        o.validate().unwrap();
    }

    #[test]
    fn a_shaped_toast_carries_over_to_albums_and_artists() {
        let shaped = ViewLayout {
            blocks: vec![LayoutBlock::Text {
                content: "{added}".into(),
            }],
        };
        let mut l = BotLayouts {
            version: 4,
            queued: shaped.clone(),
            ..Default::default()
        };
        upgrade(&mut l);
        assert_eq!(l.queued_album, shaped);
        assert_eq!(l.queued_artist, shaped);
        assert_eq!(l.queued_playlist, shaped);
        // A toast left at its default leaves the new ones at theirs.
        let mut l = BotLayouts {
            version: 4,
            ..Default::default()
        };
        upgrade(&mut l);
        assert_eq!(l, BotLayouts::default());
        // A server's own toast carries over too, but never over a version it already has.
        let own = ViewLayout {
            blocks: vec![LayoutBlock::Text {
                content: "{bot}".into(),
            }],
        };
        let mut o = LayoutOverrides {
            version: 4,
            queued: Some(shaped.clone()),
            queued_artist: Some(own.clone()),
            ..Default::default()
        };
        upgrade_overrides(&mut o);
        assert_eq!(o.queued_album, Some(shaped));
        assert_eq!(o.queued_artist, Some(own));
        assert_eq!(o.version, LAYOUT_VERSION);
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
        let list = emojis(&IconSet::default());
        assert!(list.iter().any(|e| e.name == "listening" && e.id.is_none()));
    }
}
