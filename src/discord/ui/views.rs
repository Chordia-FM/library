//! The design sheet: every message the bot sends, as a function from state to a V2 [`Message`].
//!
//! ## Visual language
//!
//! - **One container per message**, with an accent that means something: brand while playing,
//!   grey when paused/idle, red for an error, yellow for a notice. Nothing else.
//! - **Header line** `### <icon> Title`, optionally a `-# subtitle`, then a divider. Body. A wide
//!   gap before the button rows.
//! - **Icons** come from the bot's own application emoji set ([`crate::discord::emoji`]): Phosphor
//!   glyphs in the bot's accent colour. When the set is unavailable the same places show Unicode
//!   glyphs, so nothing depends on it.
//! - **Typography by markdown**: title `**bold**`, artist plain, album `*italic*`, meta `-# small`.
//!   Never a heading below `###`. Every piece of user data goes through [`fmt::escape_md`].
//!   Separators are middle dots, never dashes.
//! - **Art**: when a track has cover art it sits beside the title as a section thumbnail, on the
//!   controller and on the queued toast alike.
//! - **Buttons**: one Primary per view (the main thing), Secondary for the rest, Danger only for
//!   destructive. At most five per row.
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
use crate::discord::emoji::{Icon, IconSet};
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

/// `**Title** · Artist`, one line.
fn title_line(t: &TrackRow) -> String {
    let mut s = format!("**{}**", fmt::escape_md(&t.title));
    if !t.artist.is_empty() {
        s.push_str(" · ");
        s.push_str(&fmt::escape_md(&t.artist));
    }
    s
}

/// `**Title**` over `Artist · *Album*`.
fn track_block(t: &TrackRow) -> String {
    let mut s = format!(
        "**{}**\n{}",
        fmt::escape_md(&t.title),
        fmt::escape_md(&t.artist)
    );
    if let Some(album) = t.album.as_deref().filter(|a| !a.is_empty()) {
        s.push_str(&format!(" · *{}*", fmt::escape_md(album)));
    }
    s
}

fn mention(u: UserId) -> String {
    format!("<@{}>", u.get())
}

fn id(snap: &PlayerSnapshot, action: Action) -> String {
    CustomId::new(snap.bot_index, snap.guild_id.get(), action).to_string()
}

fn btn(snap: &PlayerSnapshot, style: ButtonStyle, action: Action, icon: Icon) -> Component {
    button(Button::new(style, id(snap, action)).emoji(snap.icons.get(icon)))
}

fn btn_labeled(
    snap: &PlayerSnapshot,
    style: ButtonStyle,
    action: Action,
    icon: Icon,
    label: &str,
) -> Component {
    button(
        Button::new(style, id(snap, action))
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
    let t = &cur.item.track;
    let (icon, title, color) = if cur.paused {
        (Icon::Pause, "Paused", accent::PAUSED)
    } else {
        (Icon::Play, "Now playing", accent::BRAND)
    };
    let subtitle = match &snap.voice_channel_name {
        Some(ch) => format!(
            "in {} {}",
            icons.get(Icon::Listening).markup(),
            fmt::escape_md(ch)
        ),
        None => snap.bot_name.clone(),
    };
    let mut meta = vec![format!("Requested by {}", mention(cur.item.requested_by))];
    meta.push(match snap.queue.len() {
        0 => "queue empty".to_string(),
        n => format!("{n} in queue"),
    });
    if snap.loop_mode != LoopMode::Off {
        meta.push(format!("loop: {}", snap.loop_mode.label()));
    }
    if snap.autoplay {
        meta.push("autoplay on".into());
    }
    if snap.volume != 100 {
        meta.push(format!("vol {}%", snap.volume));
    }
    let mut body = header(&icons.get(icon), title, Some(&subtitle));
    body.extend(with_art(
        vec![
            text(track_block(t)),
            text(small(fmt::badges(&cur.facts).join(" · "))),
        ],
        cur.cover.as_ref(),
    ));
    body.push(separator(false, Spacing::Small));
    body.push(text(format!(
        "{}\n{}",
        fmt::progress_line(cur.position_ms, t.duration_ms.max(0) as u64),
        small(meta.join(" · "))
    )));
    body.push(separator(false, Spacing::Large));
    body.push(row(vec![
        btn(snap, ButtonStyle::Secondary, Action::Previous, Icon::Prev),
        btn(
            snap,
            ButtonStyle::Primary,
            Action::PlayPause,
            Icon::PlayPause,
        ),
        btn(snap, ButtonStyle::Secondary, Action::Skip, Icon::Next),
        btn(snap, ButtonStyle::Danger, Action::Stop, Icon::Stop),
        btn(snap, ButtonStyle::Secondary, Action::Shuffle, Icon::Shuffle),
    ]));
    let loop_icon = match snap.loop_mode {
        LoopMode::Track => Icon::LoopTrack,
        _ => Icon::LoopQueue,
    };
    let on = |b: bool| {
        if b {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Secondary
        }
    };
    body.push(row(vec![
        btn_labeled(
            snap,
            on(snap.loop_mode != LoopMode::Off),
            Action::LoopCycle,
            loop_icon,
            &format!("Loop: {}", snap.loop_mode.label()),
        ),
        btn(
            snap,
            ButtonStyle::Secondary,
            Action::VolumeDown,
            Icon::VolumeDown,
        ),
        btn(snap, ButtonStyle::Secondary, Action::VolumeUp, Icon::Volume),
        btn_labeled(
            snap,
            ButtonStyle::Secondary,
            Action::QueueOpen,
            Icon::Queue,
            "Queue",
        ),
        btn_labeled(
            snap,
            on(snap.autoplay),
            Action::AutoplayToggle,
            Icon::Radio,
            "Autoplay",
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
        Some(&snap.bot_name),
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
    let by = mention(first.requested_by);
    let (icon, title, line, meta) = if enq.count > 1 {
        let total_ms: u64 = items
            .iter()
            .map(|i| i.track.duration_ms.max(0) as u64)
            .sum();
        let what = source
            .map(|s| format!("**{}**", fmt::escape_md(s)))
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
            track_block(&first.track),
            format!("by {by}"),
        )
    } else {
        let eta = snap.eta_ms(enq.position - 1);
        (
            Icon::Note,
            "Added to queue".to_string(),
            track_block(&first.track),
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
        Message::new(vec![container(accent::BRAND, body)]),
        cover,
        false,
    )
}

/// One page of the queue (0-based), with paging buttons.
pub fn queue_page(snap: &PlayerSnapshot, page: usize) -> Message {
    let icons = &snap.icons;
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
            title_line(&cur.item.track),
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
                    title_line(&item.track),
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
        body.push(row(vec![
            button(
                Button::new(ButtonStyle::Secondary, id(snap, Action::Queue(0)))
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
                    id(snap, Action::Queue((page + 1) as u32)),
                )
                .label("Next")
                .disabled(page >= last),
            ),
            button(
                Button::new(ButtonStyle::Secondary, id(snap, Action::Queue(last as u32)))
                    .emoji(icons.get(Icon::Next))
                    .disabled(page >= last),
            ),
        ]));
    }
    Message::new(vec![container(accent::BRAND, body)]).ephemeral()
}

/// Recently played, newest first.
pub fn history(snap: &PlayerSnapshot) -> Message {
    let mut body = header(&snap.icons.get(Icon::List), "History", Some(&snap.bot_name));
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
                    title_line(&item.track),
                    fmt::duration(item.track.duration_ms.max(0) as u64),
                    mention(item.requested_by)
                )
            })
            .collect();
        body.push(text(lines.join("\n")));
    }
    Message::new(vec![container(accent::BRAND, body)]).ephemeral()
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
    Message::new(vec![container(accent::BRAND, body)]).ephemeral()
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

/// A short, positive acknowledgement ("Skipped **Title**").
pub fn ok(icons: &IconSet, title: &str, detail: &str) -> Message {
    let mut body = header(&icons.get(Icon::Check), title, None);
    if !detail.is_empty() {
        body.push(text(detail.to_string()));
    }
    Message::new(vec![container(accent::BRAND, body)]).ephemeral()
}

/// The bot is busy in another channel of this guild; name the siblings that are free.
pub fn busy(
    icons: &IconSet,
    bot_name: &str,
    channel_name: &str,
    listeners: usize,
    free: &[String],
) -> Message {
    let mut detail = format!(
        "**{}** is in **#{}** with {}.",
        fmt::escape_md(bot_name),
        fmt::escape_md(channel_name),
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
    pub playing_in: Option<String>,
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
                match &b.playing_in {
                    Some(ch) => format!(
                        "playing in **#{}** · {}",
                        fmt::escape_md(ch),
                        fmt::count(b.listeners, "listener")
                    ),
                    None => "free".to_string(),
                }
            };
            format!("**{}** · {state}", fmt::escape_md(&b.name))
        })
        .collect();
    body.push(text(rows.join("\n")));
    Message::new(vec![container(accent::BRAND, body)]).ephemeral()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::player::CurrentSnapshot;
    use crate::discord::source::TrackFacts;
    use serenity::all::GuildId;
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
            guild_id: GuildId::new(777),
            voice_channel: None,
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
            busy(&icons, "Chordia", "music", 3, &["Chordia 2".into()]),
            busy(&icons, "Chordia", "music", 3, &[]),
            cancelled(),
            bots(
                &icons,
                &[BotLine {
                    name: "Chordia".into(),
                    online: true,
                    playing_in: Some("music".into()),
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
        assert!(head.contains("in 🎧 music"), "{head}");
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
        assert!(progress.contains('●'));
        assert!(progress.contains("3 in queue"));
        assert_eq!(kids[6]["components"].as_array().unwrap().len(), 5);
        assert_eq!(kids[7]["components"].as_array().unwrap().len(), 5);
        // Exactly one Primary in the first row (play/pause).
        let primaries = kids[6]["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["style"] == 1)
            .count();
        assert_eq!(primaries, 1);
        assert_eq!(kids[6]["components"][1]["custom_id"], "cd:1:1:777:pl");
        // Without an emoji set the buttons carry Unicode glyphs.
        assert_eq!(kids[6]["components"][1]["emoji"]["name"], "⏯");
        // The cover rides along as an upload.
        assert_eq!(b["attachments"][0]["filename"], "cover-c.jpg");
        assert!(!m.ephemeral);
        assert!(!b.to_string().contains('—'));
    }

    #[test]
    fn custom_emoji_set_reaches_headers_and_buttons() {
        let mut s = snap(0, true, false);
        let mut set = IconSet::default();
        for (i, icon) in Icon::ALL.iter().enumerate() {
            set.insert_for_test(*icon, 1000 + i as u64);
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
        let play = &kids[7]["components"][1]["emoji"];
        assert_eq!(play["name"], "cd_playpause");
        assert_eq!(play["id"], "1002");
        assert!(!b.to_string().contains('⏯'));
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

    #[test]
    fn paused_controller_goes_grey() {
        let mut s = snap(0, true, false);
        s.current.as_mut().unwrap().paused = true;
        let b = now_playing(&s, false).body();
        assert_eq!(b["components"][0]["accent_color"], accent::PAUSED);
    }

    #[test]
    fn queue_paging_buttons_disable_at_the_edges() {
        let s = snap(25, true, false);
        let first = queue_page(&s, 0).body();
        let rows = first["components"][0]["components"].as_array().unwrap();
        let nav = rows.last().unwrap()["components"].as_array().unwrap();
        assert_eq!(nav[0]["disabled"], true);
        assert_eq!(nav[3]["disabled"], false);
        assert_eq!(nav[2]["label"], "1/3");
        let last = queue_page(&s, 2).body();
        let rows = last["components"][0]["components"].as_array().unwrap();
        let nav = rows.last().unwrap()["components"].as_array().unwrap();
        assert_eq!(nav[3]["disabled"], true);
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
            busy(&icons, "A", "b", 1, &["C".into()]),
            idle(&s),
        ] {
            assert!(!m.body().to_string().contains('—'), "{}", m.body());
        }
    }
}
