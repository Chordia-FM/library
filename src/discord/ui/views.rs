//! The design sheet: every message the bot sends, as a function from state to a V2 [`Message`].
//!
//! ## Visual language
//!
//! - **One container per message**, with an accent that means something: the bot's colour (the
//!   icon colour, pink by default) while playing, grey when paused/idle, red for an error, yellow
//!   for a notice. Nothing else.
//! - **Header line** `### <icon> Title`, optionally a `-# subtitle`, then a divider. Body. A wide
//!   gap before the button rows.
//! - **Icons** come from the bot's own application emoji set ([`crate::discord::emoji`]): Phosphor
//!   glyphs in the bot's accent colour. When the set is unavailable the same places show Unicode
//!   glyphs, so nothing depends on it.
//! - **Typography by markdown**: title `**bold**`, artist plain, album `*italic*`, meta `-# small`.
//!   Never a heading below `###`. Every piece of user data goes through [`fmt::escape_md`].
//!   Separators are middle dots, never dashes. Channels are mentions, so they are clickable.
//! - **Links**: when the library is paired to a Hub, titles, artists and albums link to the web
//!   client, so a listener can open what they hear.
//! - **Art**: when a track has cover art it sits beside the title as a section thumbnail, on the
//!   controller and on the queued toast alike.
//! - **Buttons** are all the neutral grey style: Discord's blue and red fight the accent colour.
//!   State lives in the icon and the label (play shows a pause icon while playing; the loop and
//!   autoplay buttons say what they are set to). At most five per row.
//! - **Ephemeral** for anything only the asker cares about (errors, notices, search results, the
//!   queue). Public for the controller, the queued toast and the goodbye.
//!
//! Every custom id is built from the identity index and guild id in the snapshot, so a view is
//! self-describing about who owns its buttons.

use serenity::all::UserId;

use super::custom_id::{Action, CustomId};
use super::fmt::{self, accent};
use super::v2::{
    button, container, row, section, separator, text, thumbnail, Button, ButtonStyle, Component,
    Emoji, Media, Message, SelectOption, Spacing,
};
use crate::catalog::TrackRow;
use crate::discord::emoji::{BarState, Cap, Icon, IconSet};
use crate::discord::player::{Cover, Enqueued, LeaveReason, LoopMode, PlayerSnapshot, QueueItem};
use crate::search::{HitKind, SearchHit};

pub const QUEUE_PAGE_SIZE: usize = 10;

/// Discord renders `-#` lines small; this is how a track's "who and where" line is written.
fn small(s: impl AsRef<str>) -> String {
    format!("-# {}", s.as_ref())
}

fn header(icon: &Emoji, title: &str, subtitle: Option<&str>) -> Vec<Component> {
    let mut line = format!("### {} {title}", icon.markup());
    if let Some(s) = subtitle {
        line.push('\n');
        line.push_str(&small(s));
    }
    vec![text(line), separator(true, Spacing::Small)]
}

/// Escaped text, linked to a web-client search for `query` when the library has a web client to
/// link to.
fn linked(text: &str, web: Option<&str>, query: &str) -> String {
    let shown = fmt::escape_md(text);
    match web {
        Some(base) if !query.trim().is_empty() => {
            format!("[{shown}]({base}/app/search?q={})", fmt::urlencode(query))
        }
        _ => shown,
    }
}

/// `**Title** · Artist`, one line.
fn title_line(t: &TrackRow, web: Option<&str>) -> String {
    let mut s = format!(
        "**{}**",
        linked(&t.title, web, &format!("{} {}", t.title, t.artist))
    );
    if !t.artist.is_empty() {
        s.push_str(" · ");
        s.push_str(&linked(&t.artist, web, &t.artist));
    }
    s
}

/// `**Title**` over `Artist · *Album*`.
fn track_block(t: &TrackRow, web: Option<&str>) -> String {
    let mut s = format!(
        "**{}**\n{}",
        linked(&t.title, web, &format!("{} {}", t.title, t.artist)),
        linked(&t.artist, web, &t.artist)
    );
    if let Some(album) = t.album.as_deref().filter(|a| !a.is_empty()) {
        s.push_str(&format!(
            " · *{}*",
            linked(album, web, &format!("{album} {}", t.artist))
        ));
    }
    s
}

fn mention(u: UserId) -> String {
    format!("<@{}>", u.get())
}

fn id(snap: &PlayerSnapshot, action: Action) -> String {
    CustomId::new(snap.bot_index, snap.guild_id.get(), action).to_string()
}

fn btn(snap: &PlayerSnapshot, action: Action, icon: Icon) -> Component {
    button(Button::new(ButtonStyle::Secondary, id(snap, action)).emoji(snap.icons.get(icon)))
}

fn btn_labeled(snap: &PlayerSnapshot, action: Action, icon: Icon, label: &str) -> Component {
    button(
        Button::new(ButtonStyle::Secondary, id(snap, action))
            .emoji(snap.icons.get(icon))
            .label(label),
    )
}

/// Text beside a cover thumbnail when there is one, plain text otherwise.
fn with_art(lines: Vec<Component>, cover: Option<&Cover>) -> Vec<Component> {
    match cover {
        Some(c) => vec![section(
            lines,
            thumbnail(Media::attachment(&c.filename), None),
        )],
        None => lines,
    }
}

/// Attach the cover to the message: either upload the bytes, or (`reuse`, on an edit of the same
/// message that already carries it) keep the existing attachment by id.
fn carry_cover(msg: Message, cover: Option<&Cover>, reuse: bool) -> Message {
    match cover {
        Some(c) => match (reuse, c.attachment_id) {
            (true, Some(id)) => msg.keep_attachment(id),
            _ => msg.attach(&c.filename, c.bytes.as_ref().clone()),
        },
        None => msg,
    }
}

/// How many segments the progress bar has. Twelve emojis at Discord's inline size is about the
/// width of the title line; more and the row wraps on a phone.
pub const PROGRESS_CELLS: usize = 12;

/// The progress bar as a row of bar-segment emojis (or their text fallbacks), then the times.
fn progress_row(icons: &IconSet, position_ms: u64, duration_ms: u64) -> String {
    let cells = fmt::progress_cells(position_ms, duration_ms, PROGRESS_CELLS);
    let last = cells.len() - 1;
    let bar: String = cells
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            let cap = match i {
                0 => Cap::Left,
                n if n == last => Cap::Right,
                _ => Cap::Middle,
            };
            let state = match seg {
                fmt::Segment::Empty => BarState::Empty,
                fmt::Segment::StartDot => BarState::StartDot,
                fmt::Segment::DotLeft => BarState::DotLeft,
                fmt::Segment::HalfDot => BarState::HalfDot,
                fmt::Segment::Full => BarState::Full,
                fmt::Segment::DotRight => BarState::DotRight,
                fmt::Segment::EndDot => BarState::EndDot,
            };
            icons.get(Icon::bar(cap, state)).markup()
        })
        .collect();
    format!(
        "{bar} {} / {}",
        fmt::duration(position_ms.min(duration_ms)),
        fmt::duration(duration_ms)
    )
}

/// "in 🎧 #channel" as a real channel mention when the id is known.
fn where_line(snap: &PlayerSnapshot) -> String {
    let icon = snap.icons.get(Icon::Listening).markup();
    match (snap.voice_channel, &snap.voice_channel_name) {
        (Some(ch), _) => format!("in {icon} <#{}>", ch.get()),
        (None, Some(name)) => format!("in {icon} {}", fmt::escape_md(name)),
        (None, None) => snap.bot_name.clone(),
    }
}

// ---- controller ----------------------------------------------------------------------------------

/// The now-playing controller: the one public message per guild that is edited in place.
///
/// `reuse` says this render will edit the message that already carries the cover upload, so the
/// attachment can be kept by id instead of uploaded again.
pub fn now_playing(snap: &PlayerSnapshot, reuse: bool) -> Message {
    let Some(cur) = &snap.current else {
        return idle(snap);
    };
    let icons = &snap.icons;
    let web = snap.web_base.as_deref();
    let t = &cur.item.track;
    let (icon, title, color) = if cur.paused {
        (Icon::Pause, "Paused", accent::PAUSED)
    } else {
        (Icon::Play, "Now playing", icons.accent())
    };
    let mut meta = vec![format!("Requested by {}", mention(cur.item.requested_by))];
    meta.push(match snap.queue.len() {
        0 => "queue empty".to_string(),
        n => format!("{n} in queue"),
    });
    if snap.volume != 100 {
        meta.push(format!("vol {}%", snap.volume));
    }
    let mut body = header(&icons.get(icon), title, Some(&where_line(snap)));
    body.extend(with_art(
        vec![
            text(track_block(t, web)),
            text(small(fmt::badges(&cur.facts).join(" · "))),
        ],
        cur.cover.as_ref(),
    ));
    body.push(separator(false, Spacing::Small));
    body.push(text(format!(
        "{}\n{}",
        progress_row(icons, cur.position_ms, t.duration_ms.max(0) as u64),
        small(meta.join(" · "))
    )));
    body.push(separator(false, Spacing::Large));
    // The play/pause button shows what pressing it will do.
    let play_icon = if cur.paused { Icon::Play } else { Icon::Pause };
    body.push(row(vec![
        btn(snap, Action::Previous, Icon::Prev),
        btn(snap, Action::PlayPause, play_icon),
        btn(snap, Action::Skip, Icon::Next),
        btn(snap, Action::Stop, Icon::Stop),
        btn(snap, Action::Shuffle, Icon::Shuffle),
    ]));
    let loop_icon = match snap.loop_mode {
        LoopMode::Track => Icon::LoopTrack,
        _ => Icon::LoopQueue,
    };
    body.push(row(vec![
        btn_labeled(
            snap,
            Action::LoopCycle,
            loop_icon,
            &format!("Loop: {}", snap.loop_mode.label()),
        ),
        btn(snap, Action::VolumeDown, Icon::VolumeDown),
        btn(snap, Action::VolumeUp, Icon::Volume),
        btn_labeled(snap, Action::QueueOpen, Icon::Queue, "Queue"),
        btn_labeled(
            snap,
            Action::AutoplayToggle,
            Icon::Radio,
            if snap.autoplay {
                "Autoplay: on"
            } else {
                "Autoplay: off"
            },
        ),
    ]));
    carry_cover(
        Message::new(vec![container(color, body)]),
        cur.cover.as_ref(),
        reuse,
    )
}

/// The controller when nothing is playing.
pub fn idle(snap: &PlayerSnapshot) -> Message {
    let mut body = header(
        &snap.icons.get(Icon::Note),
        "Nothing playing",
        Some(&where_line(snap)),
    );
    body.push(text(small("The queue is empty. `/play` something.")));
    Message::new(vec![container(accent::PAUSED, body)])
}

/// The goodbye the controller turns into when the bot leaves.
pub fn left(snap: &PlayerSnapshot, reason: LeaveReason) -> Message {
    let why = match reason {
        LeaveReason::Command => "as asked",
        LeaveReason::Idle => "nothing was queued for a while",
        LeaveReason::Alone => "everyone left",
        LeaveReason::Shutdown => "the library is restarting",
        LeaveReason::Disconnected => "disconnected",
    };
    let mut body = header(
        &snap.icons.get(Icon::Wave),
        "Left the voice channel",
        Some(&snap.bot_name),
    );
    body.push(text(small(format!("{why} · `/play` to bring me back"))));
    Message::new(vec![container(accent::PAUSED, body)])
}

// ---- toasts & lists --------------------------------------------------------------------------------

/// Public confirmation after `/play`. `source` names an album/artist when several tracks were
/// added; `cover` is the first track's art.
pub fn queued(
    snap: &PlayerSnapshot,
    items: &[QueueItem],
    enq: &Enqueued,
    source: Option<&str>,
    cover: Option<&Cover>,
) -> Message {
    let Some(first) = items.first() else {
        return notice(&snap.icons, "Nothing added", "No tracks matched.");
    };
    let icons = &snap.icons;
    let web = snap.web_base.as_deref();
    let by = mention(first.requested_by);
    let (icon, title, line, meta) = if enq.count > 1 {
        let total_ms: u64 = items
            .iter()
            .map(|i| i.track.duration_ms.max(0) as u64)
            .sum();
        let what = source
            .map(|s| format!("**{}**", linked(s, web, s)))
            .unwrap_or_else(|| fmt::count(enq.count, "track"));
        let position = if enq.position == 0 {
            "playing now".to_string()
        } else {
            format!("starting at #{}", enq.position)
        };
        (
            Icon::Album,
            format!("Added {}", fmt::count(enq.count, "track")),
            what,
            format!("{position} · {} · by {by}", fmt::duration(total_ms)),
        )
    } else if enq.position == 0 {
        (
            Icon::Play,
            "Playing now".to_string(),
            track_block(&first.track, web),
            format!("by {by}"),
        )
    } else {
        let eta = snap.eta_ms(enq.position - 1);
        (
            Icon::Note,
            "Added to queue".to_string(),
            track_block(&first.track, web),
            format!(
                "#{} · plays in ~{} · by {by}",
                enq.position,
                fmt::duration(eta)
            ),
        )
    };
    let mut body = header(&snap.icons.get(icon), &title, None);
    body.extend(with_art(
        vec![text(format!("{line}\n{}", small(meta)))],
        cover,
    ));
    carry_cover(
        Message::new(vec![container(icons.accent(), body)]),
        cover,
        false,
    )
}

/// One page of the queue (0-based, clamped), with paging buttons.
pub fn queue_page(snap: &PlayerSnapshot, page: usize) -> Message {
    let icons = &snap.icons;
    let web = snap.web_base.as_deref();
    let total = snap.queue.len();
    let pages = total.div_ceil(QUEUE_PAGE_SIZE).max(1);
    let page = page.min(pages - 1);
    let mut body = header(
        &icons.get(Icon::Queue),
        "Queue",
        Some(&format!(
            "{} · {} · {}",
            fmt::count(total, "track"),
            fmt::duration(snap.queue_duration_ms()),
            snap.bot_name
        )),
    );
    match &snap.current {
        Some(cur) => body.push(text(format!(
            "{} {} `{}`",
            icons
                .get(if cur.paused { Icon::Pause } else { Icon::Play })
                .markup(),
            title_line(&cur.item.track, web),
            fmt::duration(cur.position_ms)
        ))),
        None => body.push(text(small("Nothing playing"))),
    }
    if total == 0 {
        body.push(text(small("The queue is empty.")));
    } else {
        let start = page * QUEUE_PAGE_SIZE;
        let lines: Vec<String> = snap
            .queue
            .iter()
            .enumerate()
            .skip(start)
            .take(QUEUE_PAGE_SIZE)
            .map(|(i, item)| {
                format!(
                    "`{:>2}.` {} · {} · {}",
                    i + 1,
                    title_line(&item.track, web),
                    fmt::duration(item.track.duration_ms.max(0) as u64),
                    mention(item.requested_by)
                )
            })
            .collect();
        body.push(separator(false, Spacing::Small));
        body.push(text(lines.join("\n")));
    }
    if pages > 1 {
        body.push(separator(false, Spacing::Large));
        let last = pages - 1;
        // Every button carries a distinct custom id even at the edges (Discord refuses a message
        // that repeats one), which is why first/last are their own actions.
        body.push(row(vec![
            button(
                Button::new(ButtonStyle::Secondary, id(snap, Action::QueueFirst))
                    .emoji(icons.get(Icon::Prev))
                    .disabled(page == 0),
            ),
            button(
                Button::new(
                    ButtonStyle::Secondary,
                    id(snap, Action::Queue(page.saturating_sub(1) as u32)),
                )
                .label("Back")
                .disabled(page == 0),
            ),
            button(
                Button::new(ButtonStyle::Secondary, id(snap, Action::Refresh))
                    .label(format!("{}/{}", page + 1, pages))
                    .disabled(true),
            ),
            button(
                Button::new(
                    ButtonStyle::Secondary,
                    id(snap, Action::Queue((page + 1).min(last) as u32)),
                )
                .label("Next")
                .disabled(page >= last),
            ),
            button(
                Button::new(ButtonStyle::Secondary, id(snap, Action::QueueLast))
                    .emoji(icons.get(Icon::Next))
                    .disabled(page >= last),
            ),
        ]));
    }
    Message::new(vec![container(icons.accent(), body)]).ephemeral()
}

/// Recently played, newest first.
pub fn history(snap: &PlayerSnapshot) -> Message {
    let icons = &snap.icons;
    let web = snap.web_base.as_deref();
    let mut body = header(&icons.get(Icon::List), "History", Some(&snap.bot_name));
    if snap.history.is_empty() {
        body.push(text(small("Nothing has played yet.")));
    } else {
        let lines: Vec<String> = snap
            .history
            .iter()
            .rev()
            .take(QUEUE_PAGE_SIZE)
            .map(|item| {
                format!(
                    "{} · {} · {}",
                    title_line(&item.track, web),
                    fmt::duration(item.track.duration_ms.max(0) as u64),
                    mention(item.requested_by)
                )
            })
            .collect();
        body.push(text(lines.join("\n")));
    }
    Message::new(vec![container(icons.accent(), body)]).ephemeral()
}

/// Search results with a picker. Option values are `t:<id>` / `al:<id>` / `ar:<id>`, the same
/// shape `/play`'s autocomplete uses, so one resolver serves both.
pub fn search_results(
    icons: &IconSet,
    bot: u8,
    guild: u64,
    query: &str,
    hits: &[SearchHit],
) -> Message {
    let cid = |a: Action| CustomId::new(bot, guild, a).to_string();
    let mut body = header(
        &icons.get(Icon::Search),
        &format!(
            "Results for “{}”",
            fmt::escape_md(&fmt::ellipsize(query, 60))
        ),
        None,
    );
    if hits.is_empty() {
        body.push(text(small("Nothing in the library matches.")));
        return Message::new(vec![container(accent::NOTICE, body)]).ephemeral();
    }
    let lines: Vec<String> = hits
        .iter()
        .enumerate()
        .take(10)
        .map(|(i, h)| {
            let n = i + 1;
            match h.kind {
                HitKind::Track => format!(
                    "`{n:>2}.` {} **{}** · {} · {}",
                    icons.get(Icon::Note).markup(),
                    fmt::escape_md(&h.title),
                    fmt::escape_md(&h.subtitle),
                    fmt::duration(h.duration_ms.unwrap_or(0).max(0) as u64)
                ),
                HitKind::Album => format!(
                    "`{n:>2}.` {} *{}* · {} · {}",
                    icons.get(Icon::Album).markup(),
                    fmt::escape_md(&h.title),
                    fmt::escape_md(&h.subtitle),
                    fmt::count(h.track_count.max(0) as usize, "track")
                ),
                HitKind::Artist => format!(
                    "`{n:>2}.` {} **{}** · {}",
                    icons.get(Icon::Artist).markup(),
                    fmt::escape_md(&h.title),
                    fmt::escape_md(&h.subtitle)
                ),
            }
        })
        .collect();
    body.push(text(lines.join("\n")));
    body.push(separator(false, Spacing::Large));
    let options: Vec<SelectOption> = hits
        .iter()
        .take(25)
        .map(|h| {
            let (prefix, kind, icon) = match h.kind {
                HitKind::Track => ("t", "Track", Icon::Note),
                HitKind::Album => ("al", "Album", Icon::Album),
                HitKind::Artist => ("ar", "Artist", Icon::Artist),
            };
            let label = if h.subtitle.is_empty() || h.kind == HitKind::Artist {
                h.title.clone()
            } else {
                format!("{} · {}", h.title, h.subtitle)
            };
            let desc = match h.kind {
                HitKind::Track => format!(
                    "{kind} · {}",
                    fmt::duration(h.duration_ms.unwrap_or(0).max(0) as u64)
                ),
                _ => format!(
                    "{kind} · {}",
                    fmt::count(h.track_count.max(0) as usize, "track")
                ),
            };
            SelectOption::new(label, format!("{prefix}:{}", h.id))
                .description(desc)
                .emoji(icons.get(icon))
        })
        .collect();
    body.push(row(vec![Component::StringSelect {
        custom_id: cid(Action::Select("search".into())),
        placeholder: Some("Pick one to play…".into()),
        options,
        min: 1,
        max: 1,
        disabled: false,
    }]));
    body.push(row(vec![button(
        Button::new(ButtonStyle::Secondary, cid(Action::Cancel)).label("Cancel"),
    )]));
    Message::new(vec![container(icons.accent(), body)]).ephemeral()
}

// ---- status & feedback -----------------------------------------------------------------------------

pub fn error(icons: &IconSet, title: &str, detail: &str) -> Message {
    let mut body = header(&icons.get(Icon::Cross), title, None);
    body.push(text(detail.to_string()));
    Message::new(vec![container(accent::ERROR, body)]).ephemeral()
}

pub fn notice(icons: &IconSet, title: &str, detail: &str) -> Message {
    let mut body = header(&icons.get(Icon::Warning), title, None);
    body.push(text(detail.to_string()));
    Message::new(vec![container(accent::NOTICE, body)]).ephemeral()
}

/// A short, positive acknowledgement ("Skipped **Title**"). With no detail it is the title line
/// alone: a divider with nothing under it reads as a mistake.
pub fn ok(icons: &IconSet, title: &str, detail: &str) -> Message {
    let body = if detail.is_empty() {
        vec![text(format!(
            "### {} {title}",
            icons.get(Icon::Check).markup()
        ))]
    } else {
        let mut b = header(&icons.get(Icon::Check), title, None);
        b.push(text(detail.to_string()));
        b
    };
    Message::new(vec![container(icons.accent(), body)]).ephemeral()
}

/// The bot is busy in another channel of this guild; name the siblings that are free.
pub fn busy(
    icons: &IconSet,
    bot_name: &str,
    channel: serenity::all::ChannelId,
    listeners: usize,
    free: &[String],
) -> Message {
    let mut detail = format!(
        "**{}** is in <#{}> with {}.",
        fmt::escape_md(bot_name),
        channel.get(),
        fmt::count(listeners, "listener")
    );
    match free.len() {
        0 => detail.push_str("\n-# Every bot is busy right now. Try again when a channel empties."),
        1 => detail.push_str(&format!(
            "\n**{}** is free: use its `/play`.",
            fmt::escape_md(&free[0])
        )),
        _ => detail.push_str(&format!(
            "\nFree right now: {}. Use one of their `/play` commands.",
            free.iter()
                .map(|n| format!("**{}**", fmt::escape_md(n)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
    notice(icons, "Already playing elsewhere", &detail)
}

/// A minimal replacement for a picker that was dismissed.
pub fn cancelled() -> Message {
    Message::new(vec![container(
        accent::PAUSED,
        vec![text(small("Cancelled."))],
    )])
    .ephemeral()
}

/// One line per identity, for `/bots`.
pub struct BotLine {
    pub name: String,
    pub online: bool,
    pub playing_in: Option<serenity::all::ChannelId>,
    pub listeners: usize,
}

pub fn bots(icons: &IconSet, lines: &[BotLine]) -> Message {
    let mut body = header(
        &icons.get(Icon::Info),
        "Bots",
        Some("Each one can play in a different voice channel"),
    );
    let rows: Vec<String> = lines
        .iter()
        .map(|b| {
            let state = if !b.online {
                "offline".to_string()
            } else {
                match b.playing_in {
                    Some(ch) => format!(
                        "playing in <#{}> · {}",
                        ch.get(),
                        fmt::count(b.listeners, "listener")
                    ),
                    None => "free".to_string(),
                }
            };
            format!("**{}** · {state}", fmt::escape_md(&b.name))
        })
        .collect();
    body.push(text(rows.join("\n")));
    Message::new(vec![container(icons.accent(), body)]).ephemeral()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::player::CurrentSnapshot;
    use crate::discord::source::TrackFacts;
    use serenity::all::{ChannelId, GuildId};
    use std::sync::Arc;

    fn track(id: &str, title: &str) -> Arc<TrackRow> {
        Arc::new(TrackRow {
            id: id.into(),
            library_id: String::new(),
            content_hash: format!("h{id}"),
            title: title.into(),
            artist: "Daft Punk".into(),
            album_artist: None,
            album: Some("Discovery".into()),
            year: Some(2001),
            genre: None,
            track_no: None,
            disc_no: None,
            duration_ms: 320_000,
            acoustid: None,
            recording_mbid: None,
            artist_norm: "daft punk".into(),
            title_norm: title.to_lowercase(),
            album_norm: Some("discovery".into()),
            codec: "flac".into(),
            sample_rate_hz: 44100,
            bit_depth: 16,
            channels: 2,
            lossless: 1,
            spatial: 0,
            rg_gain_db: Some(-7.1),
            rg_peak: Some(0.99),
        })
    }

    fn item(id: &str, title: &str) -> QueueItem {
        QueueItem {
            track: track(id, title),
            requested_by: UserId::new(42),
        }
    }

    fn cover() -> Cover {
        Cover {
            filename: "cover-c.jpg".into(),
            bytes: Arc::new(vec![0xff, 0xd8, 0xff]),
            attachment_id: None,
        }
    }

    fn snap(queue: usize, playing: bool, with_cover: bool) -> PlayerSnapshot {
        let mut facts = TrackFacts::from_row(&track("c", "One More Time"));
        facts.opus_kbps = Some(96);
        PlayerSnapshot {
            bot_index: 1,
            bot_name: "Chordia 2".into(),
            icons: Arc::new(IconSet::default()),
            web_base: None,
            guild_id: GuildId::new(777),
            voice_channel: Some(ChannelId::new(555)),
            voice_channel_name: Some("music".into()),
            current: playing.then(|| CurrentSnapshot {
                item: item("c", "One More Time"),
                facts,
                position_ms: 65_000,
                paused: false,
                cover: with_cover.then(cover),
            }),
            queue: (0..queue)
                .map(|i| item(&i.to_string(), &format!("Track {i}")))
                .collect(),
            history: vec![],
            loop_mode: LoopMode::Queue,
            autoplay: true,
            volume: 80,
            normalize: true,
            listeners: 3,
        }
    }

    #[test]
    fn every_view_validates() {
        let s = snap(23, true, true);
        let plain = snap(23, true, false);
        let c = cover();
        let icons = IconSet::default();
        for m in [
            now_playing(&s, false),
            now_playing(&s, true),
            now_playing(&plain, false),
            idle(&snap(0, false, false)),
            left(&s, LeaveReason::Idle),
            queued(
                &s,
                &s.queue[..1],
                &Enqueued {
                    position: 4,
                    count: 1,
                },
                None,
                Some(&c),
            ),
            queued(
                &s,
                &s.queue[..1],
                &Enqueued {
                    position: 0,
                    count: 1,
                },
                None,
                None,
            ),
            queued(
                &s,
                &s.queue,
                &Enqueued {
                    position: 0,
                    count: 23,
                },
                Some("Discovery"),
                Some(&c),
            ),
            queue_page(&s, 0),
            queue_page(&s, 99),
            queue_page(&snap(0, false, false), 0),
            history(&s),
            error(&icons, "Couldn't do that", "reason"),
            notice(&icons, "Heads up", "detail"),
            ok(&icons, "Skipped", "**x**"),
            ok(&icons, "Left", ""),
            busy(
                &icons,
                "Chordia",
                ChannelId::new(555),
                3,
                &["Chordia 2".into()],
            ),
            busy(&icons, "Chordia", ChannelId::new(555), 3, &[]),
            cancelled(),
            bots(
                &icons,
                &[BotLine {
                    name: "Chordia".into(),
                    online: true,
                    playing_in: Some(ChannelId::new(555)),
                    listeners: 2,
                }],
            ),
        ] {
            m.validate().unwrap_or_else(|e| panic!("{e}"));
            assert!(m.component_count() <= 40);
        }
        let hits = vec![
            SearchHit {
                kind: HitKind::Track,
                id: "t1".into(),
                title: "One More Time".into(),
                subtitle: "Daft Punk".into(),
                duration_ms: Some(320_000),
                cover_hash: None,
                track_count: 1,
                score: 1,
            },
            SearchHit {
                kind: HitKind::Album,
                id: "a1".into(),
                title: "Discovery".into(),
                subtitle: "Daft Punk · 2001".into(),
                duration_ms: None,
                cover_hash: None,
                track_count: 14,
                score: 1,
            },
        ];
        search_results(&icons, 1, 777, "one more", &hits)
            .validate()
            .unwrap();
        search_results(&icons, 1, 777, "zzz", &[])
            .validate()
            .unwrap();
    }

    #[test]
    fn controller_shape_matches_the_sheet() {
        let m = now_playing(&snap(3, true, true), false);
        let b = m.body();
        let c = &b["components"][0];
        assert_eq!(c["type"], 17);
        assert_eq!(c["accent_color"], accent::BRAND);
        let kids = c["components"].as_array().unwrap();
        // header, divider, section (text + badges beside the art), gap, progress, gap, two rows
        assert_eq!(kids.len(), 8);
        let head = kids[0]["content"].as_str().unwrap();
        assert!(head.starts_with("### ▶ Now playing"), "{head}");
        assert!(head.contains("in 🎧 <#555>"), "{head}");
        assert_eq!(kids[2]["type"], 9);
        assert_eq!(kids[2]["accessory"]["type"], 11);
        assert_eq!(
            kids[2]["accessory"]["media"]["url"],
            "attachment://cover-c.jpg"
        );
        let badges = kids[2]["components"][1]["content"].as_str().unwrap();
        assert!(badges.starts_with("-# FLAC · "));
        assert!(badges.contains("Opus 96k"));
        let progress = kids[4]["content"].as_str().unwrap();
        assert!(progress.contains("1:05 / 5:20"));
        // Without the emoji set the bar is its text fallback: 12 cells, playhead a fifth in.
        assert!(progress.starts_with("━━●─────────"), "{progress}");
        assert!(progress.contains("3 in queue"));
        let row1 = kids[6]["components"].as_array().unwrap();
        let row2 = kids[7]["components"].as_array().unwrap();
        assert_eq!(row1.len(), 5);
        assert_eq!(row2.len(), 5);
        // Every button is the neutral style; state lives in icons and labels.
        assert!(row1.iter().chain(row2.iter()).all(|b| b["style"] == 2));
        assert_eq!(row1[1]["custom_id"], "cd:1:1:777:pl");
        // Playing, so the play/pause button offers pause.
        assert_eq!(row1[1]["emoji"]["name"], "⏸");
        assert_eq!(row2[0]["label"], "Loop: queue");
        assert_eq!(row2[4]["label"], "Autoplay: on");
        // The cover rides along as an upload.
        assert_eq!(b["attachments"][0]["filename"], "cover-c.jpg");
        assert!(!m.ephemeral);
        assert!(!b.to_string().contains('—'));
    }

    #[test]
    fn paused_controller_offers_play_and_goes_grey() {
        let mut s = snap(0, true, false);
        s.current.as_mut().unwrap().paused = true;
        let b = now_playing(&s, false).body();
        assert_eq!(b["components"][0]["accent_color"], accent::PAUSED);
        let kids = b["components"][0]["components"].as_array().unwrap();
        assert_eq!(kids[7]["components"][1]["emoji"]["name"], "▶");
    }

    #[test]
    fn custom_emoji_set_reaches_headers_and_buttons() {
        let mut s = snap(0, true, false);
        let mut set = IconSet::default();
        for (i, icon) in Icon::all().enumerate() {
            set.insert_for_test(icon, 1000 + i as u64);
        }
        s.icons = Arc::new(set);
        let b = now_playing(&s, false).body();
        let kids = b["components"][0]["components"].as_array().unwrap();
        let head = kids[0]["content"].as_str().unwrap();
        assert!(
            head.starts_with("### <:cd_play:1000> Now playing"),
            "{head}"
        );
        // No art here, so the layout is flat and the first button row is the eighth child.
        let pause = &kids[7]["components"][1]["emoji"];
        assert_eq!(pause["name"], "cd_pause");
        assert_eq!(pause["id"], "1001");
        assert!(!b.to_string().contains('⏸'));
        // The bar is emojis too: the left cap, ten middles, the right cap.
        let progress = kids[5]["content"].as_str().unwrap();
        // 65 s of 320 s: the first two segments full, the playhead mid-third.
        assert!(progress.starts_with("<:cd_bar_l3:"), "{progress}");
        assert!(progress.contains("<:cd_bar_m2:"), "{progress}");
        assert_eq!(progress.matches("<:cd_bar_").count(), 12);
        assert!(progress.contains("<:cd_bar_r0:"), "{progress}");
    }

    #[test]
    fn web_links_appear_only_when_a_hub_is_linked() {
        let mut s = snap(1, true, false);
        let plain = now_playing(&s, false).body().to_string();
        assert!(!plain.contains("](http"));
        s.web_base = Some("https://chordia.dev".into());
        let linked = now_playing(&s, false).body().to_string();
        assert!(
            linked.contains(
                "[One More Time](https://chordia.dev/app/search?q=One%20More%20Time%20Daft%20Punk)"
            ),
            "{linked}"
        );
        assert!(linked.contains("[Daft Punk](https://chordia.dev/app/search?q=Daft%20Punk)"));
        assert!(queue_page(&s, 0)
            .body()
            .to_string()
            .contains("](https://chordia.dev/app/search?q="));
    }

    #[test]
    fn containers_take_the_icon_colour() {
        let mut s = snap(0, true, false);
        s.icons = Arc::new(IconSet::default().with_accent("#e67451"));
        let b = now_playing(&s, false).body();
        assert_eq!(b["components"][0]["accent_color"], 0xE6_74_51);
        let q = queue_page(&s, 0).body();
        assert_eq!(q["components"][0]["accent_color"], 0xE6_74_51);
        // Errors stay red whatever the theme.
        let e = error(&s.icons, "x", "y").body();
        assert_eq!(e["components"][0]["accent_color"], accent::ERROR);
    }

    #[test]
    fn a_bare_acknowledgement_has_no_dangling_divider() {
        let icons = IconSet::default();
        let b = ok(&icons, "Left", "").body();
        let kids = b["components"][0]["components"].as_array().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["type"], 10);
        let b = ok(&icons, "Skipped", "**x**").body();
        assert_eq!(
            b["components"][0]["components"].as_array().unwrap().len(),
            3
        );
    }

    #[test]
    fn controller_without_art_is_flat() {
        let b = now_playing(&snap(0, true, false), false).body();
        let kids = b["components"][0]["components"].as_array().unwrap();
        // header, divider, track, badges, gap, progress, gap, two rows
        assert_eq!(kids.len(), 9);
        assert_eq!(kids[2]["type"], 10);
        assert!(b["attachments"].as_array().unwrap().is_empty());
    }

    #[test]
    fn edits_keep_the_uploaded_cover_by_id() {
        let mut s = snap(0, true, true);
        s.current
            .as_mut()
            .unwrap()
            .cover
            .as_mut()
            .unwrap()
            .attachment_id = Some(4242);
        let kept = now_playing(&s, true);
        assert!(kept.attachments.is_empty());
        assert_eq!(kept.keep_attachments, vec![4242]);
        assert_eq!(kept.body()["attachments"][0]["id"], "4242");
        // A fresh message (ephemeral /nowplaying) still uploads.
        let fresh = now_playing(&s, false);
        assert_eq!(fresh.attachments.len(), 1);
    }

    fn custom_ids(msg: &Message) -> Vec<String> {
        fn walk(c: &Component, out: &mut Vec<String>) {
            match c {
                Component::Button(b) => out.extend(b.custom_id.clone()),
                Component::StringSelect { custom_id, .. }
                | Component::RoleSelect { custom_id, .. } => out.push(custom_id.clone()),
                Component::ActionRow(items)
                | Component::Container {
                    components: items, ..
                } => items.iter().for_each(|c| walk(c, out)),
                Component::Section {
                    components,
                    accessory,
                } => {
                    components.iter().for_each(|c| walk(c, out));
                    walk(accessory, out);
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        msg.components.iter().for_each(|c| walk(c, &mut out));
        out
    }

    #[test]
    fn no_view_repeats_a_custom_id() {
        let s = snap(25, true, false);
        for m in [
            now_playing(&s, false),
            queue_page(&s, 0),
            queue_page(&s, 1),
            queue_page(&s, 2),
        ] {
            let ids = custom_ids(&m);
            let mut dedup = ids.clone();
            dedup.sort();
            dedup.dedup();
            assert_eq!(ids.len(), dedup.len(), "duplicate custom id in {ids:?}");
        }
    }

    #[test]
    fn queue_paging_buttons_disable_at_the_edges() {
        let s = snap(25, true, false);
        let first = queue_page(&s, 0).body();
        let rows = first["components"][0]["components"].as_array().unwrap();
        let nav = rows.last().unwrap()["components"].as_array().unwrap();
        assert_eq!(nav[0]["disabled"], true);
        assert_eq!(nav[0]["custom_id"], "cd:1:1:777:qf");
        assert_eq!(nav[3]["disabled"], false);
        assert_eq!(nav[2]["label"], "1/3");
        let last = queue_page(&s, 2).body();
        let rows = last["components"][0]["components"].as_array().unwrap();
        let nav = rows.last().unwrap()["components"].as_array().unwrap();
        assert_eq!(nav[3]["disabled"], true);
        assert_eq!(nav[4]["custom_id"], "cd:1:1:777:ql");
        assert_eq!(nav[2]["label"], "3/3");
    }

    #[test]
    fn titles_are_markdown_escaped_and_nothing_uses_dashes() {
        let s = snap(0, true, false);
        let icons = IconSet::default();
        let it = vec![item("x", "F**K # 1")];
        let m = queued(
            &s,
            &it,
            &Enqueued {
                position: 1,
                count: 1,
            },
            None,
            None,
        );
        let body = m.body().to_string();
        assert!(body.contains("F\\\\*\\\\*K \\\\# 1"));
        for m in [
            queue_page(&s, 0),
            history(&s),
            left(&s, LeaveReason::Alone),
            busy(&icons, "A", ChannelId::new(1), 1, &["C".into()]),
            idle(&s),
        ] {
            assert!(!m.body().to_string().contains('—'), "{}", m.body());
        }
    }
}
