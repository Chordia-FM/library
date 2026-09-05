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
//!   State lives in the icon (white off, accent on; the loop's "1" for one track). No labels. At
//!   most five per row.
//! - **Ephemeral** for anything only the asker cares about (errors, notices, search results, the
//!   queue). Public for the controller, the queued toast and the goodbye.
//!
//! ## Layouts
//!
//! The six messages an owner can restyle (now playing, idle, queued, left, queue, history) are
//! not written here but **interpreted** from a [`ViewLayout`]: Discord's own parts (text, a
//! section with a picture or button beside it, a gallery, a separator, a row of buttons, and on
//! the list messages the line each entry is written with), every text a template in the language
//! of [`template`]. The bot's layouts come from its settings and a server may lay its own over
//! them; the shipped defaults reproduce the design above exactly. The other messages (errors,
//! pickers, settings, lyrics) are fixed.
//!
//! Every custom id is built from the identity index and guild id in the snapshot, so a view is
//! self-describing about who owns its buttons.

use serenity::all::UserId;

use super::custom_id::{Action, CustomId};
use super::fmt::{self, accent};
use super::template;
use super::v2::{
    button, container, gallery, row, section, separator, text, thumbnail, Button, ButtonStyle,
    Component, Emoji, Media, Message, SelectOption, Spacing,
};
use crate::catalog::TrackRow;
use crate::discord::emoji::{BarState, Cap, Icon, IconSet};
use crate::discord::player::{
    Cover, CurrentSnapshot, Enqueued, LeaveReason, LoopMode, PlayerSnapshot, QueueItem,
};
use crate::discord::settings::{GuildSettings, PlayEntry};
use crate::search::{HitKind, SearchHit};
use chordia_contracts::discord::ResolvedTrack;
use chordia_contracts::discord_layout::{
    Accessory, BotLayouts, ButtonSpec, ControlButton, ImageSource, LayoutBlock, LayoutView,
    SeparatorSpacing, ViewLayout, MAX_LABEL_CHARS, MAX_PAGE, MIN_PAGE,
};

/// How far back `/history` pages: enough for a long evening, not the whole log.
pub const HISTORY_LIMIT: i64 = 100;

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

/// Where `query` (or the page the Hub named) lives on the web client, when there is one.
fn link_for(web: Option<&str>, page: Option<String>, query: &str) -> Option<String> {
    let base = web?;
    Some(match page {
        Some(path) => format!("{base}{path}"),
        None if query.trim().is_empty() => return None,
        None => format!("{base}/app/search?q={}", fmt::urlencode(query)),
    })
}

/// Escaped text, linked to a page on the web client when the Hub told us where it is, else to a
/// search for `query` when there is a web client at all.
fn linked_to(text: &str, web: Option<&str>, page: Option<String>, query: &str) -> String {
    let shown = fmt::escape_md(text);
    match link_for(web, page, query) {
        Some(url) => format!("[{shown}]({url})"),
        None => shown,
    }
}

fn linked(text: &str, web: Option<&str>, query: &str) -> String {
    linked_to(text, web, None, query)
}

/// A track has no page of its own on the web client; its album's is where it lives.
fn album_page(links: Option<&ResolvedTrack>) -> Option<String> {
    links
        .and_then(|l| l.album_id)
        .map(|id| format!("/app/albums/{id}"))
}

fn artist_page(links: Option<&ResolvedTrack>) -> Option<String> {
    links
        .and_then(|l| l.artist_id)
        .map(|id| format!("/app/artists/{id}"))
}

fn title_query(t: &TrackRow) -> String {
    format!("{} {}", t.title, t.artist)
}

/// `**Title** · Artist`, one line.
fn title_line(t: &TrackRow, web: Option<&str>, links: Option<&ResolvedTrack>) -> String {
    let mut s = format!(
        "**{}**",
        linked_to(&t.title, web, album_page(links), &title_query(t))
    );
    if !t.artist.is_empty() {
        s.push_str(" · ");
        s.push_str(&linked_to(&t.artist, web, artist_page(links), &t.artist));
    }
    s
}

/// `**Title**` over `Artist · *Album*`.
fn track_block(t: &TrackRow, web: Option<&str>, links: Option<&ResolvedTrack>) -> String {
    let mut s = format!(
        "**{}**\n{}",
        linked_to(&t.title, web, album_page(links), &title_query(t)),
        linked_to(&t.artist, web, artist_page(links), &t.artist)
    );
    if let Some(album) = t.album.as_deref().filter(|a| !a.is_empty()) {
        s.push_str(&format!(
            " · *{}*",
            linked_to(
                album,
                web,
                album_page(links),
                &format!("{album} {}", t.artist)
            )
        ));
    }
    s
}

fn mention(u: UserId) -> String {
    format!("<@{}>", u.get())
}

/// Who asked for an item: a mention, or the radio.
fn requester(item: &QueueItem) -> String {
    if item.autoplay {
        "Autoplay".to_string()
    } else {
        mention(item.requested_by)
    }
}

fn onoff(b: bool) -> String {
    (if b { "on" } else { "off" }).to_string()
}

fn id(snap: &PlayerSnapshot, action: Action) -> String {
    CustomId::new(snap.bot_index, snap.guild_id.get(), action).to_string()
}

fn btn(snap: &PlayerSnapshot, action: Action, icon: Icon) -> Component {
    button(Button::new(ButtonStyle::Secondary, id(snap, action)).emoji(snap.icons.get(icon)))
}

/// A bar of `cells` segment emojis (or their text fallbacks) filled to `position` of `total`.
fn bar(icons: &IconSet, position: u64, total: u64, cells: usize) -> String {
    let cells = fmt::progress_cells(position, total, cells.max(2));
    let last = cells.len() - 1;
    cells
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
        .collect()
}

/// The small line under the progress bar: who asked, the queue, the volume when it is not
/// 100 %, and the modes that are on.
fn meta_line(snap: &PlayerSnapshot, cur: &CurrentSnapshot) -> String {
    let mut meta: Vec<String> = Vec::new();
    meta.push(if cur.item.autoplay {
        "Autoplay".to_string()
    } else {
        format!("Requested by {}", mention(cur.item.requested_by))
    });
    meta.push(match snap.queue.len() {
        0 => "queue empty".to_string(),
        n => format!("{n} in queue"),
    });
    if snap.volume != 100 {
        meta.push(format!("vol {}%", snap.volume));
    }
    if snap.loop_mode != LoopMode::Off {
        meta.push(format!("loop: {}", snap.loop_mode.label()));
    }
    if snap.shuffle {
        meta.push("shuffle".to_string());
    }
    if snap.autoplay {
        meta.push("autoplay".to_string());
    }
    meta.join(" · ")
}

/// `▶ **Title** · Artist `1:05``, or a small "Nothing playing".
fn now_playing_line(snap: &PlayerSnapshot) -> String {
    let icons = &snap.icons;
    match &snap.current {
        Some(cur) => format!(
            "{} {} `{}`",
            icons
                .get(if cur.paused { Icon::Pause } else { Icon::Play })
                .markup(),
            title_line(
                &cur.item.track,
                snap.web_base.as_deref(),
                cur.links.as_ref()
            ),
            fmt::duration(cur.position_ms)
        ),
        None => small("Nothing playing"),
    }
}

/// One control as a button. Stateful ones carry their state in the icon's colour: white off,
/// accent on, and the loop's "1" for one track. No labels; the icons are their own explanation.
fn control(snap: &PlayerSnapshot, cur: &CurrentSnapshot, which: ControlButton) -> Component {
    let (action, icon) = match which {
        ControlButton::Previous => (Action::Previous, Icon::Prev),
        // The play/pause button shows what pressing it will do.
        ControlButton::PlayPause => (
            Action::PlayPause,
            if cur.paused { Icon::Play } else { Icon::Pause },
        ),
        ControlButton::Skip => (Action::Skip, Icon::Next),
        ControlButton::Stop => (Action::Stop, Icon::Stop),
        ControlButton::Shuffle => (
            Action::Shuffle,
            if snap.shuffle {
                Icon::Shuffle
            } else {
                Icon::ShuffleOff
            },
        ),
        ControlButton::Loop => (
            Action::LoopCycle,
            match snap.loop_mode {
                LoopMode::Off => Icon::LoopOff,
                LoopMode::Track => Icon::LoopTrack,
                LoopMode::Queue => Icon::LoopQueue,
            },
        ),
        ControlButton::VolumeDown => (Action::VolumeDown, Icon::VolumeDown),
        ControlButton::VolumeUp => (Action::VolumeUp, Icon::Volume),
        ControlButton::Queue => (Action::QueueOpen, Icon::Queue),
        ControlButton::Autoplay => (
            Action::AutoplayToggle,
            if snap.autoplay {
                Icon::Radio
            } else {
                Icon::RadioOff
            },
        ),
        ControlButton::Lyrics => (Action::Lyrics, Icon::Lyrics),
    };
    btn(snap, action, icon)
}

// ---- the layout interpreter ----------------------------------------------------------------------

/// The facts of a `/play` confirmation.
struct Toast<'a> {
    added: String,
    added_meta: String,
    count: usize,
    /// 1-based queue number of the first added track; 0 when it started at once.
    position: usize,
    eta_ms: u64,
    source: Option<&'a str>,
    requested_by: UserId,
}

/// Which message's paging buttons a list uses.
#[derive(Clone, Copy)]
enum Paging {
    Queue,
    History,
}

/// One entry of a list message: its own variables, laid over the scene's.
struct Entry {
    vars: Vec<(&'static str, String)>,
}

/// What one render of a view draws from: the live snapshot, the view's own icon and title, and
/// the facts only that view knows (the current track, the toast's lines, why the bot left, the
/// entries a list shows).
struct Scene<'a> {
    snap: &'a PlayerSnapshot,
    icon: Icon,
    heading: String,
    /// The track the track variables describe.
    track: Option<&'a TrackRow>,
    links: Option<&'a ResolvedTrack>,
    cur: Option<&'a CurrentSnapshot>,
    /// The pictures a layout may show, as uploads.
    cover: Option<&'a Cover>,
    artist_art: Option<&'a Cover>,
    toast: Option<Toast<'a>>,
    reason: Option<&'static str>,
    list: Vec<Entry>,
    /// The page asked for (0-based); the list block clamps it.
    page: usize,
    paging: Option<Paging>,
}

impl<'a> Scene<'a> {
    fn new(snap: &'a PlayerSnapshot, icon: Icon, heading: &str) -> Self {
        Scene {
            snap,
            icon,
            heading: heading.to_string(),
            track: None,
            links: None,
            cur: None,
            cover: None,
            artist_art: None,
            toast: None,
            reason: None,
            list: Vec::new(),
            page: 0,
            paging: None,
        }
    }

    /// A variable's value: `None` for one this scene does not know (left as written), an empty
    /// string for one it knows but has nothing for.
    fn var(&self, name: &str, arg: Option<&str>) -> Option<String> {
        let snap = self.snap;
        let icons = &snap.icons;
        let web = snap.web_base.as_deref();
        let t = self.track;
        let cells = || template::bar_cells(arg);
        Some(match name {
            "icon" => icons.get(self.icon).markup(),
            "heading" => self.heading.clone(),
            "bot" => fmt::escape_md(&snap.bot_name),
            "channel" => match (snap.voice_channel, &snap.voice_channel_name) {
                (Some(ch), _) => format!("<#{}>", ch.get()),
                (None, Some(name)) => fmt::escape_md(name),
                (None, None) => String::new(),
            },
            "channel_name" => snap
                .voice_channel_name
                .as_deref()
                .map(fmt::escape_md)
                .unwrap_or_default(),
            "listeners" => snap.listeners.to_string(),
            "queue_count" => snap.queue.len().to_string(),
            "queue_tracks" => fmt::count(snap.queue.len(), "track"),
            "queue_duration" => fmt::duration(snap.queue_duration_ms()),
            "volume" => snap.volume.to_string(),
            "volume_bar" => bar(icons, snap.volume.min(100) as u64, 100, cells()),
            "loop" => snap.loop_mode.label().to_string(),
            "shuffle" => onoff(snap.shuffle),
            "autoplay" => onoff(snap.autoplay),
            "meta" => self.cur.map(|c| meta_line(snap, c)).unwrap_or_default(),
            "now_playing_line" => now_playing_line(snap),
            "emoji" => icons.get(Icon::by_name(arg?.trim())?).markup(),
            "track" => t
                .map(|t| track_block(t, web, self.links))
                .unwrap_or_default(),
            "track_line" => t
                .map(|t| title_line(t, web, self.links))
                .unwrap_or_default(),
            "title" => t.map(|t| fmt::escape_md(&t.title)).unwrap_or_default(),
            "artist" => t.map(|t| fmt::escape_md(&t.artist)).unwrap_or_default(),
            "album" => t
                .and_then(|t| t.album.as_deref())
                .map(fmt::escape_md)
                .unwrap_or_default(),
            "title_link" => t
                .and_then(|t| link_for(web, album_page(self.links), &title_query(t)))
                .unwrap_or_default(),
            "artist_link" => t
                .and_then(|t| link_for(web, artist_page(self.links), &t.artist))
                .unwrap_or_default(),
            "album_link" => t
                .and_then(|t| {
                    let album = t.album.as_deref().unwrap_or("");
                    link_for(
                        web,
                        album_page(self.links),
                        &format!("{album} {}", t.artist),
                    )
                })
                .unwrap_or_default(),
            "duration" => t
                .map(|t| fmt::duration(t.duration_ms.max(0) as u64))
                .unwrap_or_default(),
            "position" => self
                .cur
                .map(|c| fmt::duration(c.position_ms))
                .unwrap_or_default(),
            "progress_bar" => self
                .cur
                .map(|c| {
                    bar(
                        icons,
                        c.position_ms,
                        c.item.track.duration_ms.max(0) as u64,
                        cells(),
                    )
                })
                .unwrap_or_default(),
            "badges" => self
                .cur
                .map(|c| fmt::badges(&c.facts).join(" · "))
                .unwrap_or_default(),
            "requested_by" => match (self.cur, &self.toast) {
                (Some(c), _) => requester(&c.item),
                (None, Some(t)) => mention(t.requested_by),
                _ => String::new(),
            },
            "added" => self
                .toast
                .as_ref()
                .map(|t| t.added.clone())
                .unwrap_or_default(),
            "added_meta" => self
                .toast
                .as_ref()
                .map(|t| t.added_meta.clone())
                .unwrap_or_default(),
            "count" => self
                .toast
                .as_ref()
                .map(|t| t.count.to_string())
                .unwrap_or_default(),
            "queue_position" => self
                .toast
                .as_ref()
                .filter(|t| t.position > 0)
                .map(|t| t.position.to_string())
                .unwrap_or_default(),
            "eta" => self
                .toast
                .as_ref()
                .filter(|t| t.position > 0)
                .map(|t| fmt::duration(t.eta_ms))
                .unwrap_or_default(),
            "source" => self
                .toast
                .as_ref()
                .and_then(|t| t.source)
                .map(fmt::escape_md)
                .unwrap_or_default(),
            "reason" => self.reason.unwrap_or("").to_string(),
            _ => return None,
        })
    }

    fn render(&self, tpl: &str) -> String {
        template::render(tpl, |n, a| self.var(n, a))
    }

    /// A list entry's line: its own variables first, then the scene's.
    fn render_entry(&self, tpl: &str, entry: &Entry, index: String) -> String {
        template::render(tpl, |n, a| {
            if n == "index" {
                return Some(index.clone());
            }
            entry
                .vars
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.clone())
                .or_else(|| self.var(n, a))
        })
    }
}

/// What a layout drew, and which uploads it referred to, so the caller attaches only those.
struct Rendered {
    components: Vec<Component>,
    cover: bool,
    artist: bool,
}

fn media_for(scene: &Scene, source: &ImageSource, used: &mut Rendered) -> Option<Media> {
    match source {
        ImageSource::Cover => scene.cover.map(|c| {
            used.cover = true;
            Media::attachment(&c.filename)
        }),
        ImageSource::Artist => scene.artist_art.map(|c| {
            used.artist = true;
            Media::attachment(&c.filename)
        }),
        ImageSource::BotAvatar => scene.snap.bot_avatar.as_deref().map(Media::url),
        ImageSource::Url { url } => {
            let url = scene.render(url);
            let url = url.trim();
            (url.starts_with("https://") || url.starts_with("http://")).then(|| Media::url(url))
        }
    }
}

fn button_for(scene: &Scene, spec: &ButtonSpec) -> Option<Component> {
    match spec {
        ButtonSpec::Control { control: which } => scene.cur.map(|c| control(scene.snap, c, *which)),
        ButtonSpec::Link { label, url } => {
            let url = scene.render(url);
            let url = url.trim();
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return None;
            }
            let label = super::v2::clip(scene.render(label), MAX_LABEL_CHARS);
            Some(button(Button::link(url, label)))
        }
    }
}

/// Draw a view's blocks.
fn render_layout(scene: &Scene, layout: &ViewLayout) -> Rendered {
    let mut out = Rendered {
        components: Vec::new(),
        cover: false,
        artist: false,
    };
    for block in &layout.blocks {
        match block {
            LayoutBlock::Text { content } => {
                let s = scene.render(content);
                if !s.trim().is_empty() {
                    out.components.push(text(s));
                }
            }
            LayoutBlock::Section { content, accessory } => {
                let s = scene.render(content);
                let body = (!s.trim().is_empty()).then(|| text(s));
                match accessory {
                    Accessory::Image { source } => {
                        match (media_for(scene, source, &mut out), body) {
                            (Some(m), Some(b)) => {
                                out.components.push(section(vec![b], thumbnail(m, None)))
                            }
                            (Some(m), None) => out.components.push(gallery(vec![m])),
                            (None, Some(b)) => out.components.push(b),
                            (None, None) => {}
                        }
                    }
                    Accessory::Button { button: spec } => match (button_for(scene, spec), body) {
                        (Some(b), Some(t)) => out.components.push(section(vec![t], b)),
                        (Some(b), None) => out.components.push(row(vec![b])),
                        (None, Some(t)) => out.components.push(t),
                        (None, None) => {}
                    },
                }
            }
            LayoutBlock::Gallery { images } => {
                let items: Vec<Media> = images
                    .iter()
                    .filter_map(|i| media_for(scene, i, &mut out))
                    .collect();
                if !items.is_empty() {
                    out.components.push(gallery(items));
                }
            }
            LayoutBlock::Separator { divider, spacing } => out.components.push(separator(
                *divider,
                match spacing {
                    SeparatorSpacing::Small => Spacing::Small,
                    SeparatorSpacing::Large => Spacing::Large,
                },
            )),
            LayoutBlock::Row { buttons } => {
                let items: Vec<Component> = buttons
                    .iter()
                    .filter_map(|b| button_for(scene, b))
                    .collect();
                if !items.is_empty() {
                    out.components.push(row(items));
                }
            }
            LayoutBlock::List {
                item,
                empty,
                page_size,
            } => render_list(scene, item, empty, *page_size, &mut out.components),
        }
    }
    out
}

/// A text display holds 4000 characters; a page of long lines is split across several.
const LIST_CHUNK_CHARS: usize = 3800;

fn render_list(scene: &Scene, item: &str, empty: &str, page_size: u8, out: &mut Vec<Component>) {
    let total = scene.list.len();
    if total == 0 {
        let s = scene.render(empty);
        if !s.trim().is_empty() {
            out.push(text(s));
        }
        return;
    }
    let size = page_size.clamp(MIN_PAGE, MAX_PAGE) as usize;
    let pages = total.div_ceil(size);
    let page = scene.page.min(pages - 1);
    let start = page * size;
    let end = (start + size).min(total);
    let width = end.to_string().len();
    let mut chunks: Vec<String> = Vec::new();
    for (i, entry) in scene.list[start..end].iter().enumerate() {
        let line = scene.render_entry(item, entry, format!("{:>width$}", start + i + 1));
        match chunks.last_mut() {
            Some(c) if c.chars().count() + line.chars().count() < LIST_CHUNK_CHARS => {
                c.push('\n');
                c.push_str(&line);
            }
            _ => chunks.push(line),
        }
    }
    out.extend(chunks.into_iter().map(text));
    if pages > 1 {
        if let Some(paging) = scene.paging {
            out.push(separator(false, Spacing::Large));
            out.push(paging_row(scene.snap, paging, page, pages));
        }
    }
}

/// First / back / "2/5" / next / last. Every button carries a distinct custom id even at the
/// edges (Discord refuses a message that repeats one), which is why first/last are their own
/// actions.
fn paging_row(snap: &PlayerSnapshot, paging: Paging, page: usize, pages: usize) -> Component {
    let icons = &snap.icons;
    let last = pages - 1;
    let back = page.saturating_sub(1) as u32;
    let next = (page + 1).min(last) as u32;
    let (first, prev, fwd, end) = match paging {
        Paging::Queue => (
            Action::QueueFirst,
            Action::Queue(back),
            Action::Queue(next),
            Action::QueueLast,
        ),
        Paging::History => (
            Action::HistoryFirst,
            Action::History(back),
            Action::History(next),
            Action::HistoryLast,
        ),
    };
    row(vec![
        button(
            Button::new(ButtonStyle::Secondary, id(snap, first))
                .emoji(icons.get(Icon::Prev))
                .disabled(page == 0),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, prev))
                .label("Back")
                .disabled(page == 0),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, Action::Refresh))
                .label(format!("{}/{}", page + 1, pages))
                .disabled(true),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, fwd))
                .label("Next")
                .disabled(page >= last),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, end))
                .emoji(icons.get(Icon::Next))
                .disabled(page >= last),
        ),
    ])
}

/// The message with the uploads the layout referred to. `reuse` says this render will edit the
/// message that already carries them, so an upload can be kept by attachment id instead of sent
/// again. The same file is never attached twice.
fn with_uploads(msg: Message, uploads: &[Option<&Cover>], reuse: bool) -> Message {
    let mut msg = msg;
    let mut seen: Vec<&str> = Vec::new();
    for c in uploads.iter().flatten() {
        if seen.contains(&c.filename.as_str()) {
            continue;
        }
        seen.push(&c.filename);
        msg = match (reuse, c.attachment_id) {
            (true, Some(id)) => msg.keep_attachment(id),
            _ => msg.attach(&c.filename, c.bytes.as_ref().clone()),
        };
    }
    msg
}

fn finish(scene: &Scene, layout: &ViewLayout, color: u32, reuse: bool) -> Message {
    let r = render_layout(scene, layout);
    with_uploads(
        Message::new(vec![container(color, r.components)]),
        &[
            r.cover.then_some(scene.cover).flatten(),
            r.artist.then_some(scene.artist_art).flatten(),
        ],
        reuse,
    )
}

/// Whether any of these layouts shows the artist's picture, so the player knows to fetch it.
pub fn uses_artist_art(layouts: &BotLayouts) -> bool {
    LayoutView::ALL.iter().any(|v| {
        layouts.view(*v).blocks.iter().any(|b| match b {
            LayoutBlock::Section {
                accessory: Accessory::Image { source },
                ..
            } => matches!(source, ImageSource::Artist),
            LayoutBlock::Gallery { images } => {
                images.iter().any(|i| matches!(i, ImageSource::Artist))
            }
            _ => false,
        })
    })
}

// ---- controller ----------------------------------------------------------------------------------

/// The now-playing controller: the one public message per guild that is edited in place, laid
/// out by the `now_playing` layout.
pub fn now_playing(snap: &PlayerSnapshot, reuse: bool) -> Message {
    let Some(cur) = &snap.current else {
        return idle(snap);
    };
    let (icon, heading, color) = if cur.paused {
        (Icon::Pause, "Paused", accent::PAUSED)
    } else {
        (Icon::Play, "Now playing", snap.icons.accent())
    };
    let mut scene = Scene::new(snap, icon, heading);
    scene.track = Some(&cur.item.track);
    scene.links = cur.links.as_ref();
    scene.cur = Some(cur);
    scene.cover = cur.cover.as_ref();
    scene.artist_art = cur.artist_art.as_ref();
    finish(&scene, &snap.layouts.now_playing, color, reuse)
}

/// The controller when nothing is playing.
pub fn idle(snap: &PlayerSnapshot) -> Message {
    let scene = Scene::new(snap, Icon::Note, "Nothing playing");
    finish(&scene, &snap.layouts.idle, accent::PAUSED, false)
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
    let mut scene = Scene::new(snap, Icon::Wave, "Left the voice channel");
    scene.reason = Some(why);
    finish(&scene, &snap.layouts.left, accent::PAUSED, false)
}

// ---- toasts & lists --------------------------------------------------------------------------------

/// Public confirmation after `/play`, laid out by the `queued` layout. `source` names an
/// album/artist when several tracks were added; `cover` is the picture for what was added (the
/// first track's art, or the artist's when the query was an artist) and `artist_art` the
/// artist's picture when it was fetched.
pub fn queued(
    snap: &PlayerSnapshot,
    items: &[QueueItem],
    enq: &Enqueued,
    source: Option<&str>,
    cover: Option<&Cover>,
    artist_art: Option<&Cover>,
) -> Message {
    let Some(first) = items.first() else {
        return notice(&snap.icons, "Nothing added", "No tracks matched.");
    };
    let web = snap.web_base.as_deref();
    let by = mention(first.requested_by);
    let eta_ms = if enq.position > 0 {
        snap.eta_ms(enq.position - 1)
    } else {
        0
    };
    let (icon, heading, added, added_meta) = if enq.count > 1 {
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
            track_block(&first.track, web, None),
            format!("by {by}"),
        )
    } else {
        (
            Icon::Note,
            "Added to queue".to_string(),
            track_block(&first.track, web, None),
            format!(
                "#{} · plays in ~{} · by {by}",
                enq.position,
                fmt::duration(eta_ms)
            ),
        )
    };
    let mut scene = Scene::new(snap, icon, &heading);
    scene.track = Some(&first.track);
    scene.cover = cover;
    scene.artist_art = artist_art;
    scene.toast = Some(Toast {
        added,
        added_meta,
        count: enq.count,
        position: enq.position,
        eta_ms,
        source,
        requested_by: first.requested_by,
    });
    finish(&scene, &snap.layouts.queued, snap.icons.accent(), false)
}

fn track_vars(t: &TrackRow, web: Option<&str>) -> Vec<(&'static str, String)> {
    vec![
        ("track", track_block(t, web, None)),
        ("track_line", title_line(t, web, None)),
        ("title", fmt::escape_md(&t.title)),
        ("artist", fmt::escape_md(&t.artist)),
        (
            "album",
            t.album.as_deref().map(fmt::escape_md).unwrap_or_default(),
        ),
        (
            "title_link",
            link_for(web, None, &title_query(t)).unwrap_or_default(),
        ),
        (
            "artist_link",
            link_for(web, None, &t.artist).unwrap_or_default(),
        ),
        (
            "album_link",
            t.album
                .as_deref()
                .and_then(|a| link_for(web, None, &format!("{a} {}", t.artist)))
                .unwrap_or_default(),
        ),
        ("duration", fmt::duration(t.duration_ms.max(0) as u64)),
    ]
}

/// One page of the queue (0-based, clamped), laid out by the `queue` layout.
pub fn queue_page(snap: &PlayerSnapshot, page: usize) -> Message {
    let web = snap.web_base.as_deref();
    let mut scene = Scene::new(snap, Icon::Queue, "Queue");
    scene.list = snap
        .queue
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let mut vars = track_vars(&item.track, web);
            vars.push(("requested_by", requester(item)));
            vars.push(("eta", fmt::duration(snap.eta_ms(i))));
            Entry { vars }
        })
        .collect();
    scene.page = page;
    scene.paging = Some(Paging::Queue);
    finish(&scene, &snap.layouts.queue, snap.icons.accent(), false).ephemeral()
}

/// What this server heard, newest first, from the play log (a stop or a loop does not erase what
/// was heard), laid out by the `history` layout.
pub fn history(snap: &PlayerSnapshot, plays: &[PlayEntry], page: usize) -> Message {
    let web = snap.web_base.as_deref();
    let icons = &snap.icons;
    let mut scene = Scene::new(snap, Icon::List, "History");
    scene.list = plays
        .iter()
        .map(|p| {
            let mut line = format!(
                "**{}**",
                linked(&p.title, web, &format!("{} {}", p.title, p.artist))
            );
            if !p.artist.is_empty() {
                line.push_str(" · ");
                line.push_str(&linked(&p.artist, web, &p.artist));
            }
            let requested_by = p
                .requested_by
                .as_deref()
                .and_then(|u| u.parse::<u64>().ok())
                .map(|u| mention(UserId::new(u)))
                .unwrap_or_default();
            let counted = if p.scrobbled_for > 0 {
                format!(
                    "{} counted for {}",
                    icons.get(Icon::Check).markup(),
                    fmt::count(p.scrobbled_for as usize, "listener")
                )
            } else {
                "not counted".to_string()
            };
            Entry {
                vars: vec![
                    ("track_line", line.clone()),
                    ("track", line),
                    ("title", fmt::escape_md(&p.title)),
                    ("artist", fmt::escape_md(&p.artist)),
                    ("played_at", format!("<t:{}:R>", p.started_at / 1000)),
                    (
                        "played_for",
                        if p.ms_played > 0 {
                            fmt::duration(p.ms_played as u64)
                        } else {
                            String::new()
                        },
                    ),
                    ("requested_by", requested_by),
                    ("counted", counted),
                ],
            }
        })
        .collect();
    scene.page = page;
    scene.paging = Some(Paging::History);
    finish(&scene, &snap.layouts.history, icons.accent(), false).ephemeral()
}

/// What a server has played through the bot: a few headed sections of text.
pub fn stats(
    icons: &IconSet,
    title: &str,
    subtitle: &str,
    sections: &[(String, String)],
) -> Message {
    let mut body = header(&icons.get(Icon::Listening), title, Some(subtitle));
    for (heading, lines) in sections {
        body.push(text(format!("**{heading}**\n{lines}")));
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

/// This server's settings for the bot: a summary, a DJ-role picker and toggle buttons. Every
/// control edits the message in place, so it doubles as the settings panel.
pub fn settings(snap: &PlayerSnapshot, gs: &GuildSettings) -> Message {
    let icons = &snap.icons;
    let onoff = |b: bool| if b { "on" } else { "off" };
    let dj_roles = gs.dj_roles();
    let dj = if dj_roles.is_empty() {
        "none, anyone in the channel".to_string()
    } else {
        dj_roles
            .iter()
            .map(|id| format!("<@&{id}>"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let not_enabled = "not enabled by the library owner";
    let volume = gs
        .volume
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| format!("{}% (bot default)", snap.volume));
    let always_on = match (
        gs.can_always_on,
        gs.always_on,
        gs.always_on_channel_id.as_deref(),
    ) {
        (false, ..) => not_enabled.to_string(),
        (true, true, Some(ch)) => format!("on, in <#{ch}>"),
        (true, true, None) => "on".to_string(),
        (true, false, _) => "off".to_string(),
    };
    let autoplay = if gs.can_autoplay {
        onoff(gs.autoplay)
    } else {
        not_enabled
    };
    let mut body = header(
        &icons.get(Icon::Gear),
        "Settings",
        Some(&format!("{} in this server", snap.bot_name)),
    );
    body.push(text(format!(
        "**DJ roles** · {dj}\n**Volume** · {volume}\n**Normalize volume** · {}\n**Autoplay** · {autoplay}\n**24/7** · {always_on}\n**Re-post controller when it scrolls away** · {}",
        onoff(gs.normalize),
        onoff(gs.announce)
    )));
    body.push(separator(false, Spacing::Large));
    body.push(row(vec![Component::RoleSelect {
        custom_id: id(snap, Action::Select("dj".into())),
        placeholder: Some("DJ roles: who may control shared playback".into()),
        default_roles: dj_roles,
        max_values: 25,
    }]));
    let setting = |name: &str, icon: Icon, label: String| {
        button(
            Button::new(
                ButtonStyle::Secondary,
                id(snap, Action::Setting(name.into())),
            )
            .emoji(icons.get(icon))
            .label(label),
        )
    };
    body.push(row(vec![
        setting(
            "normalize",
            Icon::Volume,
            format!("Normalize: {}", onoff(gs.normalize)),
        ),
        setting(
            "autoplay",
            Icon::Radio,
            format!("Autoplay: {}", onoff(gs.autoplay)),
        ),
        setting(
            "always_on",
            Icon::Listening,
            format!("24/7: {}", onoff(gs.always_on)),
        ),
        setting(
            "announce",
            Icon::Queue,
            format!("Re-post: {}", onoff(gs.announce)),
        ),
        setting("dj_clear", Icon::Cross, "Clear DJ roles".into()),
    ]));
    Message::new(vec![container(icons.accent(), body)]).ephemeral()
}

/// One page of lyrics, with paging when there is more than one.
pub fn lyrics(
    snap: &PlayerSnapshot,
    title: &str,
    artist: &str,
    pages: &[String],
    page: usize,
) -> Message {
    let icons = &snap.icons;
    let last = pages.len().saturating_sub(1);
    let page = page.min(last);
    let mut body = header(
        &icons.get(Icon::Lyrics),
        &fmt::escape_md(title),
        Some(&fmt::escape_md(artist)),
    );
    match pages.get(page) {
        Some(p) => body.push(text(fmt::escape_md(p))),
        None => body.push(text(small("No lyrics in this file's tags."))),
    }
    if pages.len() > 1 {
        body.push(separator(false, Spacing::Large));
        body.push(row(vec![
            button(
                Button::new(
                    ButtonStyle::Secondary,
                    id(snap, Action::LyricsPage(page.saturating_sub(1) as u32)),
                )
                .label("Back")
                .disabled(page == 0),
            ),
            button(
                Button::new(ButtonStyle::Secondary, id(snap, Action::Refresh))
                    .label(format!("{}/{}", page + 1, pages.len()))
                    .disabled(true),
            ),
            button(
                Button::new(
                    ButtonStyle::Secondary,
                    id(snap, Action::LyricsPage((page + 1).min(last) as u32)),
                )
                .label("Next")
                .disabled(page >= last),
            ),
        ]));
    }
    Message::new(vec![container(icons.accent(), body)]).ephemeral()
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
            autoplay: false,
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
            bot_avatar: Some("https://cdn.discordapp.com/avatars/1/a.png".into()),
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
                artist_art: None,
                links: None,
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
            shuffle: false,
            layouts: Arc::new(BotLayouts::default()),
        }
    }

    fn plays(n: usize) -> Vec<PlayEntry> {
        (0..n)
            .map(|i| PlayEntry {
                title: format!("Play {i}"),
                artist: "Daft Punk".into(),
                requested_by: (i % 2 == 0).then(|| "42".to_string()),
                started_at: 1_700_000_000_000 + i as i64 * 1000,
                ms_played: if i % 3 == 0 { 0 } else { 120_000 },
                scrobbled_for: (i % 2) as i64,
            })
            .collect()
    }

    fn kids(m: &Message) -> Vec<serde_json::Value> {
        m.body()["components"][0]["components"]
            .as_array()
            .unwrap()
            .clone()
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
                None,
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
                Some(&c),
            ),
            queue_page(&s, 0),
            queue_page(&s, 99),
            queue_page(&snap(0, false, false), 0),
            history(&s, &[], 0),
            history(&s, &plays(25), 1),
            history(&s, &plays(25), 99),
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
        // header, divider, section (track and badges beside the art), gap, progress, gap, two rows
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
        let track = kids[2]["components"][0]["content"].as_str().unwrap();
        assert!(
            track.starts_with("**One More Time**\nDaft Punk · *Discovery*\n-# FLAC · "),
            "{track}"
        );
        assert!(track.contains("Opus 96k"));
        let progress = kids[4]["content"].as_str().unwrap();
        assert!(progress.contains("1:05 / 5:20"));
        // Without the emoji set the bar is its text fallback: 12 cells, playhead a fifth in.
        assert!(progress.starts_with("━━●─────────"), "{progress}");
        assert!(progress.contains("\n-# Requested by <@42> · 3 in queue · vol 80%"));
        let row1 = kids[6]["components"].as_array().unwrap();
        let row2 = kids[7]["components"].as_array().unwrap();
        assert_eq!(row1.len(), 5);
        assert_eq!(row2.len(), 5);
        // Every button is the neutral style; state lives in the icons, never in a label.
        assert!(row1.iter().chain(row2.iter()).all(|b| b["style"] == 2));
        assert!(row1.iter().chain(row2.iter()).all(|b| b["label"].is_null()));
        assert_eq!(row1[1]["custom_id"], "cd:1:1:777:pl");
        // Playing, so the play/pause button offers pause.
        assert_eq!(row1[1]["emoji"]["name"], "⏸");
        // Without the emoji set the state icons fall back to glyphs; the meta line says it in words.
        assert_eq!(row2[0]["emoji"]["name"], "🔁");
        assert_eq!(row2[4]["emoji"]["name"], "📻");
        assert!(progress.contains("loop: queue") && progress.contains("autoplay"));
        // The cover rides along as an upload.
        assert_eq!(b["attachments"][0]["filename"], "cover-c.jpg");
        assert!(!m.ephemeral);
        assert!(!b.to_string().contains('—'));
    }

    #[test]
    fn paused_controller_offers_play_and_goes_grey() {
        let mut s = snap(0, true, false);
        s.current.as_mut().unwrap().paused = true;
        let m = now_playing(&s, false);
        let b = m.body();
        assert_eq!(b["components"][0]["accent_color"], accent::PAUSED);
        let kids = kids(&m);
        assert!(kids[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("### ⏸ Paused"));
        assert_eq!(kids[6]["components"][1]["emoji"]["name"], "▶");
    }

    #[test]
    fn custom_emoji_set_reaches_headers_and_buttons() {
        let mut s = snap(0, true, false);
        let mut set = IconSet::default();
        for (i, icon) in Icon::all().enumerate() {
            set.insert_for_test(icon, 1000 + i as u64);
        }
        s.icons = Arc::new(set);
        let m = now_playing(&s, false);
        let b = m.body();
        let kids = kids(&m);
        let head = kids[0]["content"].as_str().unwrap();
        assert!(
            head.starts_with("### <:cd_play:1000> Now playing"),
            "{head}"
        );
        assert!(head.contains("in <:cd_listening:"), "{head}");
        // No art here, so the section is plain text and the first button row is the seventh child.
        let pause = &kids[6]["components"][1]["emoji"];
        assert_eq!(pause["name"], "cd_pause");
        assert_eq!(pause["id"], "1001");
        assert!(!b.to_string().contains('⏸'));
        // The bar is emojis too: the left cap, ten middles, the right cap.
        let progress = kids[4]["content"].as_str().unwrap();
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
        // The Hub's ids win over a search when they are known.
        s.current.as_mut().unwrap().links = Some(ResolvedTrack {
            track_ref: "c".into(),
            track_id: uuid::Uuid::nil(),
            album_id: Some(uuid::Uuid::nil()),
            artist_id: None,
        });
        let deep = now_playing(&s, false).body().to_string();
        assert!(
            deep.contains("[One More Time](https://chordia.dev/app/albums/"),
            "{deep}"
        );
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
        let m = now_playing(&snap(0, true, false), false);
        let b = m.body();
        let kids = kids(&m);
        // header, divider, track text, gap, progress, gap, two rows
        assert_eq!(kids.len(), 8);
        assert_eq!(kids[2]["type"], 10);
        assert!(b["attachments"].as_array().unwrap().is_empty());
        // With nothing queued the meta line says so.
        assert!(kids[4]["content"].as_str().unwrap().contains("queue empty"));
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

    fn layouts_with(view: LayoutView, blocks: Vec<LayoutBlock>) -> BotLayouts {
        let mut l = BotLayouts::default();
        *l.view_mut(view) = ViewLayout { blocks };
        l.validate().unwrap();
        l
    }

    #[test]
    fn a_custom_layout_renders_and_attaches_only_what_it_shows() {
        let mut s = snap(2, true, true);
        s.layouts = Arc::new(layouts_with(
            LayoutView::NowPlaying,
            vec![
                LayoutBlock::Text {
                    content:
                        "Now: **{title}** by {artist} for {requested_by} ({position}/{duration})"
                            .into(),
                },
                LayoutBlock::Text {
                    content: "{progress_bar:6}\n-# {queue_count} queued · {volume_bar:4} {volume}%"
                        .into(),
                },
                LayoutBlock::Gallery {
                    images: vec![ImageSource::Cover, ImageSource::Artist],
                },
                LayoutBlock::Row {
                    buttons: vec![
                        ButtonSpec::Control {
                            control: ControlButton::PlayPause,
                        },
                        ButtonSpec::Control {
                            control: ControlButton::Lyrics,
                        },
                    ],
                },
            ],
        ));
        let m = now_playing(&s, false);
        m.validate().unwrap_or_else(|e| panic!("{e}"));
        let b = m.body();
        let kids = kids(&m);
        let first = kids[0]["content"].as_str().unwrap();
        assert_eq!(
            first,
            "Now: **One More Time** by Daft Punk for <@42> (1:05/5:20)"
        );
        let second = kids[1]["content"].as_str().unwrap();
        // Six cells, and a volume bar of four at 80 %.
        assert_eq!(second.chars().filter(|c| "━●─".contains(*c)).count(), 10);
        assert!(second.ends_with("\n-# 2 queued · ━━━● 80%"), "{second}");
        // Text, text, gallery (the artist picture is not there, so one item), one row.
        assert_eq!(kids.len(), 4);
        assert_eq!(kids[2]["type"], 12);
        assert_eq!(kids[2]["items"].as_array().unwrap().len(), 1);
        assert_eq!(kids[3]["components"].as_array().unwrap().len(), 2);
        assert_eq!(b["attachments"][0]["filename"], "cover-c.jpg");

        // With the art placed nowhere, nothing is uploaded.
        s.layouts = Arc::new(layouts_with(
            LayoutView::NowPlaying,
            vec![LayoutBlock::Text {
                content: "{track}".into(),
            }],
        ));
        let b = now_playing(&s, false).body();
        assert!(
            b["attachments"].as_array().is_none_or(|a| a.is_empty()),
            "{b}"
        );
    }

    #[test]
    fn pictures_buttons_and_emoji_come_from_the_layout() {
        let mut s = snap(0, true, true);
        s.web_base = Some("https://chordia.dev".into());
        s.layouts = Arc::new(layouts_with(
            LayoutView::NowPlaying,
            vec![
                LayoutBlock::Section {
                    content: "{emoji:listening} {channel} · {emoji:nope} · {bot}".into(),
                    accessory: Accessory::Image {
                        source: ImageSource::BotAvatar,
                    },
                },
                LayoutBlock::Section {
                    content: "{track_line}".into(),
                    accessory: Accessory::Button {
                        button: ButtonSpec::Link {
                            label: "Open {album}".into(),
                            url: "{album_link}".into(),
                        },
                    },
                },
                LayoutBlock::Section {
                    content: "-# {badges}".into(),
                    accessory: Accessory::Image {
                        source: ImageSource::Url {
                            url: "{nope}".into(),
                        },
                    },
                },
                LayoutBlock::Row {
                    buttons: vec![
                        ButtonSpec::Link {
                            label: "Chordia".into(),
                            url: "https://chordia.dev".into(),
                        },
                        ButtonSpec::Control {
                            control: ControlButton::Skip,
                        },
                    ],
                },
            ],
        ));
        let m = now_playing(&s, false);
        m.validate().unwrap_or_else(|e| panic!("{e}"));
        let k = kids(&m);
        // The avatar is a thumbnail by URL; an unknown emoji name stays visible.
        assert_eq!(k[0]["accessory"]["type"], 11);
        assert_eq!(
            k[0]["accessory"]["media"]["url"],
            "https://cdn.discordapp.com/avatars/1/a.png"
        );
        assert_eq!(
            k[0]["components"][0]["content"],
            "🎧 <#555> · {emoji:nope} · Chordia 2"
        );
        // A link button beside the track, to the album's search page.
        assert_eq!(k[1]["accessory"]["type"], 2);
        assert_eq!(k[1]["accessory"]["style"], 5);
        assert_eq!(k[1]["accessory"]["label"], "Open Discovery");
        assert_eq!(
            k[1]["accessory"]["url"],
            "https://chordia.dev/app/search?q=Discovery%20Daft%20Punk"
        );
        // A picture that did not resolve leaves plain text.
        assert_eq!(k[2]["type"], 10);
        assert!(k[2]["content"].as_str().unwrap().starts_with("-# FLAC"));
        let row = k[3]["components"].as_array().unwrap();
        assert_eq!(row[0]["style"], 5);
        assert_eq!(row[1]["custom_id"], "cd:1:1:777:sk");
        // Nothing referred to the cover, so it is not uploaded.
        assert!(m.attachments.is_empty());

        // Without a web client the album link has nowhere to go: no button, the text stays.
        s.web_base = None;
        let m = now_playing(&s, false);
        m.validate().unwrap();
        assert_eq!(kids(&m)[1]["type"], 10);
    }

    #[test]
    fn a_server_lays_its_own_layout_over_the_bots() {
        use chordia_contracts::discord_layout::LayoutOverrides;
        let bot = BotLayouts::default();
        let mut gs = GuildSettings::defaults("1", "777");
        assert_eq!(gs.layouts(&bot), bot);
        gs.layout_overrides = LayoutOverrides {
            idle: Some(ViewLayout {
                blocks: vec![LayoutBlock::Text {
                    content: "Quiet in {channel}".into(),
                }],
            }),
            ..Default::default()
        };
        let mut s = snap(0, false, false);
        s.layouts = Arc::new(gs.layouts(&bot));
        assert_eq!(s.layouts.now_playing, bot.now_playing);
        let k = kids(&idle(&s));
        assert_eq!(k.len(), 1);
        assert_eq!(k[0]["content"], "Quiet in <#555>");
    }

    #[test]
    fn settings_and_lyrics_validate() {
        let s = snap(0, true, false);
        let mut gs = GuildSettings::defaults("1", "777");
        settings(&s, &gs).validate().unwrap();
        gs.dj_role_ids = vec!["99".into(), "98".into()];
        gs.always_on = true;
        gs.always_on_channel_id = Some("555".into());
        let b = settings(&s, &gs).body();
        let txt = b.to_string();
        assert!(
            txt.contains("<@&99>") && txt.contains("<@&98>") && txt.contains("<#555>"),
            "{txt}"
        );
        gs.can_always_on = false;
        let txt = settings(&s, &gs).body().to_string();
        assert!(txt.contains("not enabled by the library owner"), "{txt}");
        // header, divider, summary, gap, then the DJ role select row.
        assert_eq!(
            b["components"][0]["components"][4]["components"][0]["type"],
            6
        );
        let pages: Vec<String> = vec!["la la".into(), "da da".into()];
        lyrics(&s, "Song", "Band", &pages, 0).validate().unwrap();
        lyrics(&s, "Song", "Band", &pages, 9).validate().unwrap();
        lyrics(&s, "Song", "Band", &[], 0).validate().unwrap();
    }

    #[test]
    fn no_view_repeats_a_custom_id() {
        let s = snap(25, true, false);
        let pages: Vec<String> = (0..3).map(|i| format!("page {i}")).collect();
        for m in [
            now_playing(&s, false),
            queue_page(&s, 0),
            queue_page(&s, 1),
            queue_page(&s, 2),
            history(&s, &plays(25), 0),
            history(&s, &plays(25), 2),
            settings(&s, &GuildSettings::defaults("1", "777")),
            lyrics(&s, "t", "a", &pages, 0),
            lyrics(&s, "t", "a", &pages, 2),
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
        let first = queue_page(&s, 0);
        let rows = kids(&first);
        let nav = rows.last().unwrap()["components"].as_array().unwrap();
        assert_eq!(nav[0]["disabled"], true);
        assert_eq!(nav[0]["custom_id"], "cd:1:1:777:qf");
        assert_eq!(nav[3]["disabled"], false);
        assert_eq!(nav[2]["label"], "1/3");
        // The entries are numbered, right-aligned to the page's widest number.
        let head = rows[0]["content"].as_str().unwrap();
        assert!(head.contains("25 tracks"), "{head}");
        assert!(rows[2]["content"]
            .as_str()
            .unwrap()
            .starts_with("▶ **One More Time**"));
        let list = rows[4]["content"].as_str().unwrap();
        assert!(
            list.starts_with("` 1.` **Track 0** · Daft Punk · 5:20 · <@42>"),
            "{list}"
        );
        assert!(list.contains("\n`10.` **Track 9**"), "{list}");
        let last = queue_page(&s, 2);
        let rows = kids(&last);
        let nav = rows.last().unwrap()["components"].as_array().unwrap();
        assert_eq!(nav[3]["disabled"], true);
        assert_eq!(nav[4]["custom_id"], "cd:1:1:777:ql");
        assert_eq!(nav[2]["label"], "3/3");
        // An empty queue says so and has no paging.
        let empty = queue_page(&snap(0, false, false), 0);
        let rows = kids(&empty);
        assert_eq!(rows.last().unwrap()["content"], "-# The queue is empty.");
    }

    #[test]
    fn history_pages_and_says_what_counted() {
        let s = snap(0, true, false);
        let m = history(&s, &plays(12), 1);
        let rows = kids(&m);
        let list = rows[2]["content"].as_str().unwrap();
        // Page two of a ten-a-page list holds the last two entries.
        assert_eq!(list.lines().count(), 4, "{list}");
        assert!(
            list.starts_with(
                "**Play 10** · Daft Punk\n-# <t:1700000010:R> · 2:00 · <@42> · not counted"
            ),
            "{list}"
        );
        assert!(
            list.contains(
                "**Play 11** · Daft Punk\n-# <t:1700000011:R> · 2:00 · ✔ counted for 1 listener"
            ),
            "{list}"
        );
        let nav = rows.last().unwrap()["components"].as_array().unwrap();
        assert_eq!(nav[0]["custom_id"], "cd:1:1:777:hf");
        assert_eq!(nav[1]["custom_id"], "cd:1:1:777:h:0");
        assert_eq!(nav[3]["disabled"], true);
        let empty = history(&s, &[], 0);
        assert_eq!(
            kids(&empty).last().unwrap()["content"],
            "-# Nothing has played yet."
        );
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
            None,
        );
        let body = m.body().to_string();
        assert!(body.contains("F\\\\*\\\\*K \\\\# 1"));
        for m in [
            queue_page(&s, 0),
            history(&s, &plays(3), 0),
            left(&s, LeaveReason::Alone),
            busy(&icons, "A", ChannelId::new(1), 1, &["C".into()]),
            idle(&s),
        ] {
            assert!(!m.body().to_string().contains('—'), "{}", m.body());
        }
    }
}
