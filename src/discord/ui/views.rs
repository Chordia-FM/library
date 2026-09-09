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

use super::custom_id::{Action, CustomId, Origin};
use super::fmt::{self, accent};
use super::template;
use super::v2::{
    button, container, gallery, row, section, separator, text, thumbnail, Button, ButtonStyle,
    Component, Emoji, Media, Message, SelectOption, Spacing,
};
use crate::catalog::TrackRow;
use crate::discord::emoji::{BarState, Cap, Icon, IconSet};
use crate::discord::eq;
use crate::discord::player::{
    Cover, CurrentSnapshot, Enqueued, LeaveReason, LoopMode, PlayerSnapshot, QueueItem, VoteTally,
};
use crate::discord::settings::{GuildSettings, PlayEntry};
use crate::discord::source::TrackFacts;
use crate::search::{HitKind, SearchHit};
use chordia_contracts::discord::ResolvedTrack;
use chordia_contracts::discord_layout::{
    Accessory, BotLayouts, ButtonSpec, ContainerAccent, ControlButton, ImageSource, LayoutBlock,
    LayoutView, SeparatorSpacing, ViewLayout, MAX_LABEL_CHARS, MAX_PAGE, MIN_PAGE,
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

fn album_url(t: &TrackRow, web: Option<&str>, links: Option<&ResolvedTrack>) -> Option<String> {
    let album = t.album.as_deref().filter(|a| !a.is_empty())?;
    link_for(web, album_page(links), &format!("{album} {}", t.artist))
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

/// A control's button, noting which message it sits on when that is not the controller.
fn btn(scene: &Scene, action: Action, icon: Icon) -> Component {
    let snap = scene.snap;
    let cid = CustomId::new(snap.bot_index, snap.guild_id.get(), action).on(scene.origin);
    button(Button::new(ButtonStyle::Secondary, cid.to_string()).emoji(snap.icons.get(icon)))
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
fn control(scene: &Scene, cur: &CurrentSnapshot, which: ControlButton) -> Component {
    let snap = scene.snap;
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
        ControlButton::Mute => (
            Action::Mute,
            if snap.muted {
                Icon::Mute
            } else {
                Icon::MuteOff
            },
        ),
        ControlButton::SeekBack => (Action::SeekBack, Icon::SeekBack),
        ControlButton::SeekForward => (Action::SeekForward, Icon::SeekForward),
        ControlButton::Clear => (Action::Clear, Icon::Clear),
        ControlButton::History => (Action::HistoryOpen, Icon::History),
        ControlButton::Leave => (Action::Leave, Icon::Leave),
        ControlButton::Equalizer => (Action::EqOpen, Icon::Equalizer),
    };
    btn(scene, action, icon)
}

// ---- the layout interpreter ----------------------------------------------------------------------

/// The facts of a `/play` confirmation.
struct Toast<'a> {
    added: String,
    added_meta: String,
    count: usize,
    /// How long everything added runs.
    duration_ms: u64,
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
    Lyrics,
}

/// Whether a layout has page buttons anywhere; without them there is no way past page one.
fn has_pager(layout: &ViewLayout) -> bool {
    layout
        .flat()
        .iter()
        .any(|b| matches!(b, LayoutBlock::Pager))
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
    /// The colour a container in the bot's colour takes on this message.
    accent: u32,
    /// The track the track variables describe.
    track: Option<&'a TrackRow>,
    links: Option<&'a ResolvedTrack>,
    cur: Option<&'a CurrentSnapshot>,
    /// The pictures a layout may show, as uploads.
    cover: Option<&'a Cover>,
    artist_art: Option<&'a Cover>,
    artist_banner: Option<&'a Cover>,
    toast: Option<Toast<'a>>,
    reason: Option<&'static str>,
    /// What a reply says under its title.
    detail: Option<&'a str>,
    /// The page of lyrics shown.
    lyrics: Option<&'a str>,
    /// A vote to skip, and who just cast one.
    vote: Option<&'a VoteTally>,
    voter: Option<UserId>,
    /// Which message this is when it is not the controller, for its controls to say so.
    origin: Option<Origin>,
    list: Vec<Entry>,
    /// The page shown (0-based, already clamped) and how many there are.
    page: usize,
    pages: usize,
    paging: Option<Paging>,
}

impl<'a> Scene<'a> {
    fn new(snap: &'a PlayerSnapshot, icon: Icon, heading: &str, accent: u32) -> Self {
        Scene {
            snap,
            icon,
            heading: heading.to_string(),
            accent,
            track: None,
            links: None,
            cur: None,
            cover: None,
            artist_art: None,
            artist_banner: None,
            toast: None,
            reason: None,
            detail: None,
            lyrics: None,
            vote: None,
            voter: None,
            origin: None,
            list: Vec::new(),
            page: 0,
            pages: 1,
            paging: None,
        }
    }

    /// Give the scene its entries and the page asked for; the page is clamped to what the
    /// layout's list block holds, so `{page}` in a header agrees with the list under it. A layout
    /// with no page buttons has no way past the first page, so that is the one it shows.
    fn paged(&mut self, list: Vec<Entry>, page: usize, layout: &ViewLayout, paging: Paging) {
        let size = layout
            .blocks
            .iter()
            .find_map(|b| match b {
                LayoutBlock::List { page_size, .. } => Some(*page_size),
                _ => None,
            })
            .unwrap_or(10)
            .clamp(MIN_PAGE, MAX_PAGE) as usize;
        self.pages = list.len().div_ceil(size).max(1);
        let pager = has_pager(layout);
        self.page = if pager { page.min(self.pages - 1) } else { 0 };
        self.list = list;
        self.paging = pager.then_some(paging);
    }

    /// Give the scene pages of text (lyrics) and the page asked for, as [`Scene::paged`] does
    /// for entries.
    fn paged_text(
        &mut self,
        pages: &'a [String],
        page: usize,
        layout: &ViewLayout,
        paging: Paging,
    ) {
        self.pages = pages.len().max(1);
        let pager = has_pager(layout);
        self.page = if pager { page.min(self.pages - 1) } else { 0 };
        self.lyrics = pages.get(self.page).map(String::as_str);
        self.paging = pager.then_some(paging);
    }

    /// A variable's value: `None` for one this scene does not know (left as written), an empty
    /// string for one it knows but has nothing for.
    fn var(&self, name: &str, arg: Option<&str>) -> Option<String> {
        let snap = self.snap;
        let icons = &snap.icons;
        let web = snap.web_base.as_deref();
        let t = self.track;
        let cur = self.cur;
        let facts = cur.map(|c| &c.facts);
        let toast = self.toast.as_ref();
        let cells = || template::bar_cells(arg);
        let of_track = |f: &dyn Fn(&TrackRow) -> String| t.map(f).unwrap_or_default();
        let of_facts = |f: &dyn Fn(&TrackFacts) -> String| facts.map(f).unwrap_or_default();
        let of_toast = |f: &dyn Fn(&Toast) -> String| toast.map(f).unwrap_or_default();
        let of_cur = |f: &dyn Fn(&CurrentSnapshot) -> String| cur.map(f).unwrap_or_default();
        let of_vote = |f: &dyn Fn(&VoteTally) -> String| self.vote.map(f).unwrap_or_default();
        Some(match name {
            // the message
            "icon" => icons.get(self.icon).markup(),
            "heading" => self.heading.clone(),
            "detail" => self.detail.unwrap_or("").to_string(),
            "emoji" => icons.get(Icon::by_name(arg?.trim())?).markup(),
            // the bot, the server, the channel
            "bot" | "bot.name" => fmt::escape_md(&snap.bot_name),
            "bot.mention" => snap
                .bot_user_id
                .map(|id| format!("<@{id}>"))
                .unwrap_or_default(),
            "bot.avatar" => snap.bot_avatar.clone().unwrap_or_default(),
            "server" => snap
                .guild_name
                .as_deref()
                .map(fmt::escape_md)
                .unwrap_or_default(),
            "channel" => match (snap.voice_channel, &snap.voice_channel_name) {
                (Some(ch), _) => format!("<#{}>", ch.get()),
                (None, Some(name)) => fmt::escape_md(name),
                (None, None) => String::new(),
            },
            "channel.name" => snap
                .voice_channel_name
                .as_deref()
                .map(fmt::escape_md)
                .unwrap_or_default(),
            "channel.listeners" => snap.listeners.to_string(),
            // the track
            "track" => of_track(&|t| track_block(t, web, self.links)),
            "track.line" => of_track(&|t| title_line(t, web, self.links)),
            "track.title" => of_track(&|t| fmt::escape_md(&t.title)),
            "track.artist" => of_track(&|t| fmt::escape_md(&t.artist)),
            "track.album" => {
                of_track(&|t| t.album.as_deref().map(fmt::escape_md).unwrap_or_default())
            }
            "track.album_artist" => of_track(&|t| {
                t.album_artist
                    .as_deref()
                    .map(fmt::escape_md)
                    .unwrap_or_default()
            }),
            "track.year" => of_track(&|t| t.year.map(|y| y.to_string()).unwrap_or_default()),
            "track.genre" => {
                of_track(&|t| t.genre.as_deref().map(fmt::escape_md).unwrap_or_default())
            }
            "track.number" => of_track(&|t| t.track_no.map(|n| n.to_string()).unwrap_or_default()),
            "track.disc" => of_track(&|t| t.disc_no.map(|n| n.to_string()).unwrap_or_default()),
            "track.duration" => of_track(&|t| fmt::duration(t.duration_ms.max(0) as u64)),
            "track.url" => of_track(&|t| {
                link_for(web, album_page(self.links), &title_query(t)).unwrap_or_default()
            }),
            "track.artist_url" => {
                of_track(&|t| link_for(web, artist_page(self.links), &t.artist).unwrap_or_default())
            }
            "track.album_url" => of_track(&|t| album_url(t, web, self.links).unwrap_or_default()),
            "track.title_link" => {
                of_track(&|t| linked_to(&t.title, web, album_page(self.links), &title_query(t)))
            }
            "track.artist_link" => {
                of_track(&|t| linked_to(&t.artist, web, artist_page(self.links), &t.artist))
            }
            "track.album_link" => of_track(&|t| match t.album.as_deref() {
                Some(album) if !album.is_empty() => linked_to(
                    album,
                    web,
                    album_page(self.links),
                    &format!("{album} {}", t.artist),
                ),
                _ => String::new(),
            }),
            // the file
            "file" => of_facts(&|f| fmt::badges(f).join(" · ")),
            "file.codec" => of_facts(&|f| fmt::codec_label(&f.codec).to_string()),
            "file.sample_rate" => of_facts(&|f| fmt::sample_rate(f.sample_rate_hz)),
            "file.bit_depth" => of_facts(&|f| {
                if f.lossless && f.bit_depth > 0 {
                    format!("{}-bit", f.bit_depth)
                } else {
                    String::new()
                }
            }),
            "file.channels" => of_facts(&|f| match f.channels {
                1 => "mono".to_string(),
                2 => "stereo".to_string(),
                n => format!("{n} channels"),
            }),
            "file.quality" => of_facts(&|f| {
                if f.spatial {
                    "Atmos".to_string()
                } else if f.lossless {
                    "Lossless".to_string()
                } else {
                    String::new()
                }
            }),
            "file.bitrate" => {
                of_facts(&|f| f.opus_kbps.map(|k| format!("{k} kbps")).unwrap_or_default())
            }
            "file.gain" => of_facts(&|f| f.gain_db.map(fmt::gain).unwrap_or_default()),
            // the player
            "player.status" => match cur {
                Some(c) if c.paused => "paused".to_string(),
                Some(_) => "playing".to_string(),
                None => "idle".to_string(),
            },
            "player.position" => of_cur(&|c| fmt::duration(c.position_ms)),
            "player.remaining" => of_cur(&|c| {
                fmt::duration(
                    (c.item.track.duration_ms.max(0) as u64).saturating_sub(c.position_ms),
                )
            }),
            "player.progress_bar" => of_cur(&|c| {
                bar(
                    icons,
                    c.position_ms,
                    c.item.track.duration_ms.max(0) as u64,
                    cells(),
                )
            }),
            "player.volume" => snap.volume.to_string(),
            "player.volume_bar" => bar(icons, snap.volume.min(100) as u64, 100, cells()),
            "player.loop" => snap.loop_mode.label().to_string(),
            "player.shuffle" => onoff(snap.shuffle),
            "player.autoplay" => onoff(snap.autoplay),
            "player.muted" => onoff(snap.muted),
            "player.eq" => eq::label(&snap.eq),
            "player.meta" => of_cur(&|c| meta_line(snap, c)),
            "player.line" => now_playing_line(snap),
            // the queue
            "queue.count" => snap.queue.len().to_string(),
            "queue.tracks" => fmt::count(snap.queue.len(), "track"),
            "queue.duration" => fmt::duration(snap.queue_duration_ms()),
            "queue.next" => snap
                .queue
                .first()
                .map(|i| title_line(&i.track, web, None))
                .unwrap_or_default(),
            // who asked
            "requester" => match (cur, toast) {
                (Some(c), _) => requester(&c.item),
                (None, Some(t)) => mention(t.requested_by),
                _ => String::new(),
            },
            "requester.id" => match (cur, toast) {
                (Some(c), _) if !c.item.autoplay => c.item.requested_by.get().to_string(),
                (None, Some(t)) => t.requested_by.get().to_string(),
                _ => String::new(),
            },
            // what was added
            "added" => of_toast(&|t| t.added.clone()),
            "added.meta" => of_toast(&|t| t.added_meta.clone()),
            "added.count" => of_toast(&|t| t.count.to_string()),
            "added.position" => of_toast(&|t| {
                if t.position > 0 {
                    t.position.to_string()
                } else {
                    String::new()
                }
            }),
            "added.eta" => of_toast(&|t| {
                if t.position > 0 {
                    fmt::duration(t.eta_ms)
                } else {
                    String::new()
                }
            }),
            "added.duration" => of_toast(&|t| fmt::duration(t.duration_ms)),
            "added.source" => of_toast(&|t| t.source.map(fmt::escape_md).unwrap_or_default()),
            // why the bot left
            "left.reason" => self.reason.unwrap_or("").to_string(),
            // lyrics
            "lyrics" => self.lyrics.map(fmt::escape_md).unwrap_or_default(),
            // a vote to skip
            "vote.by" => self.voter.map(mention).unwrap_or_default(),
            "vote.count" => of_vote(&|v| v.count.to_string()),
            "vote.needed" => of_vote(&|v| v.needed.to_string()),
            "vote.remaining" => of_vote(&|v| v.needed.saturating_sub(v.count).to_string()),
            "vote.listeners" => of_vote(&|v| v.listeners.to_string()),
            "vote.percent" => of_vote(&|v| v.percent.to_string()),
            // pages
            "page" => format!("{}/{}", self.page + 1, self.pages),
            "page.number" => (self.page + 1).to_string(),
            "page.count" => self.pages.to_string(),
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
    banner: bool,
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
        ImageSource::ArtistBanner => scene.artist_banner.map(|c| {
            used.banner = true;
            Media::attachment(&c.filename)
        }),
        ImageSource::BotAvatar => scene.snap.bot_avatar.as_deref().map(Media::url),
        ImageSource::ServerIcon => scene.snap.guild_icon.as_deref().map(Media::url),
        ImageSource::Url { url } => {
            let url = scene.render(url);
            let url = url.trim();
            (url.starts_with("https://") || url.starts_with("http://")).then(|| Media::url(url))
        }
    }
}

fn button_for(scene: &Scene, spec: &ButtonSpec) -> Option<Component> {
    match spec {
        // A control needs something playing (the play/pause button shows its state); it goes on
        // any message, not only ones about the current track.
        ButtonSpec::Control { control: which } => scene
            .snap
            .current
            .as_ref()
            .map(|c| control(scene, c, *which)),
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

/// `#rrggbb` as Discord's integer colour.
fn hex_colour(hex: &str) -> Option<u32> {
    u32::from_str_radix(hex.trim().trim_start_matches('#'), 16).ok()
}

/// A divider with nothing under it reads as a mistake: the separators a block list ends with go.
fn trim_trailing_separators(components: &mut Vec<Component>) {
    while matches!(components.last(), Some(Component::Separator { .. })) {
        components.pop();
    }
}

/// Draw a view's blocks.
fn render_layout(scene: &Scene, layout: &ViewLayout) -> Rendered {
    let mut out = render_blocks(scene, &layout.blocks);
    trim_trailing_separators(&mut out.components);
    out
}

fn render_blocks(scene: &Scene, blocks: &[LayoutBlock]) -> Rendered {
    let mut out = Rendered {
        components: Vec::new(),
        cover: false,
        artist: false,
        banner: false,
    };
    for block in blocks {
        match block {
            LayoutBlock::Container { accent, blocks } => {
                let mut inner = render_blocks(scene, blocks);
                trim_trailing_separators(&mut inner.components);
                out.cover |= inner.cover;
                out.artist |= inner.artist;
                out.banner |= inner.banner;
                if inner.components.is_empty() {
                    continue;
                }
                out.components.push(Component::Container {
                    accent: match accent {
                        ContainerAccent::Bot => Some(scene.accent),
                        ContainerAccent::Fixed { hex } => hex_colour(hex),
                        ContainerAccent::None => None,
                    },
                    components: inner.components,
                });
            }
            LayoutBlock::Text { content } => {
                let s = scene.render(content);
                if !s.trim().is_empty() {
                    out.components.push(text(s));
                }
            }
            LayoutBlock::Section { texts, accessory } => {
                let bodies: Vec<Component> = texts
                    .iter()
                    .map(|t| scene.render(t))
                    .filter(|s| !s.trim().is_empty())
                    .map(text)
                    .collect();
                match accessory {
                    Accessory::Image { source } => {
                        match (media_for(scene, source, &mut out), bodies.is_empty()) {
                            (Some(m), false) => {
                                out.components.push(section(bodies, thumbnail(m, None)))
                            }
                            (Some(m), true) => out.components.push(gallery(vec![m])),
                            (None, false) => out.components.extend(bodies),
                            (None, true) => {}
                        }
                    }
                    Accessory::Button { button: spec } => {
                        match (button_for(scene, spec), bodies.is_empty()) {
                            (Some(b), false) => out.components.push(section(bodies, b)),
                            (Some(b), true) => out.components.push(row(vec![b])),
                            (None, false) => out.components.extend(bodies),
                            (None, true) => {}
                        }
                    }
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
            LayoutBlock::Pager => {
                if let Some(paging) = scene.paging.filter(|_| scene.pages > 1) {
                    out.components
                        .push(paging_row(scene.snap, paging, scene.page, scene.pages));
                }
            }
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
    let page = scene.page;
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
}

/// First / back / "2/5" / next / last, drawn wherever the layout's pager block sits. Every button
/// carries a distinct custom id even at the edges (Discord refuses a message that repeats one),
/// which is why first/last are their own actions.
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
        Paging::Lyrics => (
            Action::LyricsFirst,
            Action::LyricsPage(back),
            Action::LyricsPage(next),
            Action::LyricsLast,
        ),
    };
    row(vec![
        button(
            Button::new(ButtonStyle::Secondary, id(snap, first))
                .emoji(icons.get(Icon::PageFirst))
                .disabled(page == 0),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, prev))
                .emoji(icons.get(Icon::PageBack))
                .disabled(page == 0),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, Action::Refresh))
                .label(format!("{}/{}", page + 1, pages))
                .disabled(true),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, fwd))
                .emoji(icons.get(Icon::PageNext))
                .disabled(page >= last),
        ),
        button(
            Button::new(ButtonStyle::Secondary, id(snap, end))
                .emoji(icons.get(Icon::PageLast))
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

fn finish(scene: &Scene, layout: &ViewLayout, reuse: bool) -> Message {
    let r = render_layout(scene, layout);
    with_uploads(
        Message::new(r.components),
        &[
            r.cover.then_some(scene.cover).flatten(),
            r.artist.then_some(scene.artist_art).flatten(),
            r.banner.then_some(scene.artist_banner).flatten(),
        ],
        reuse,
    )
}

/// Whether any of these layouts shows the artist's picture, so the player knows to fetch it.
pub fn uses_artist_art(layouts: &BotLayouts) -> bool {
    LayoutView::ALL.iter().any(|v| {
        layouts.view(*v).flat().into_iter().any(|b| match b {
            LayoutBlock::Section {
                accessory: Accessory::Image { source },
                ..
            } => matches!(source, ImageSource::Artist | ImageSource::ArtistBanner),
            LayoutBlock::Gallery { images } => images
                .iter()
                .any(|i| matches!(i, ImageSource::Artist | ImageSource::ArtistBanner)),
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
    let mut scene = Scene::new(snap, icon, heading, color);
    scene.track = Some(&cur.item.track);
    scene.links = cur.links.as_ref();
    scene.cur = Some(cur);
    scene.cover = cur.cover.as_ref();
    scene.artist_art = cur.artist_art.as_ref();
    scene.artist_banner = cur.artist_banner.as_ref();
    finish(&scene, &snap.layouts.now_playing, reuse)
}

/// The controller when nothing is playing.
pub fn idle(snap: &PlayerSnapshot) -> Message {
    let scene = Scene::new(snap, Icon::Note, "Nothing playing", accent::PAUSED);
    finish(&scene, &snap.layouts.idle, false)
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
    let mut scene = Scene::new(snap, Icon::Wave, "Left the voice channel", accent::PAUSED);
    scene.reason = Some(why);
    finish(&scene, &snap.layouts.left, false)
}

// ---- toasts & lists --------------------------------------------------------------------------------

/// What a `/play` (or `/album`, `/artist`, `/playlist`) added, for the toast.
#[derive(Clone, Copy)]
pub struct Added<'a> {
    pub kind: HitKind,
    /// The album, artist or playlist the tracks came from.
    pub source: Option<&'a str>,
    /// The source's page on the web client, relative to it.
    pub url: Option<&'a str>,
}

/// Public confirmation after `/play`, laid out by the `queued`, `queued_album`, `queued_artist`
/// or `queued_playlist` layout by what the query was. `cover` is the picture for what was added
/// (the first track's art, or the artist's when the query was an artist) and `artist_art` the
/// artist's picture when it was fetched.
pub fn queued(
    snap: &PlayerSnapshot,
    items: &[QueueItem],
    enq: &Enqueued,
    added: Added<'_>,
    cover: Option<&Cover>,
    artist_art: Option<&Cover>,
    artist_banner: Option<&Cover>,
) -> Message {
    let Some(first) = items.first() else {
        return notice(snap, "Nothing added", "No tracks matched.");
    };
    let web = snap.web_base.as_deref();
    let by = mention(first.requested_by);
    let eta_ms = if enq.position > 0 {
        snap.eta_ms(enq.position - 1)
    } else {
        0
    };
    let total_ms: u64 = items
        .iter()
        .map(|i| i.track.duration_ms.max(0) as u64)
        .sum();
    let total = fmt::duration(total_ms);
    // When it plays: now, somewhere in the shuffle, or at a place in the queue after so long.
    let when = |first_of_many: bool| {
        if enq.position == 0 {
            "playing now".to_string()
        } else if snap.shuffle {
            "shuffled in".to_string()
        } else {
            format!(
                "{}plays in ~{} (#{} in queue)",
                if first_of_many { "first " } else { "" },
                fmt::duration(eta_ms),
                enq.position
            )
        }
    };
    let (icon, heading, added_line, added_meta, layout) = match added.kind {
        HitKind::Album | HitKind::Artist | HitKind::Playlist => {
            let what = match (added.source, added.url, web) {
                (Some(s), Some(path), Some(web)) => {
                    format!("**[{}]({web}{path})**", fmt::escape_md(s))
                }
                (Some(s), _, _) => format!("**{}**", linked(s, web, s)),
                (None, _, _) => fmt::count(enq.count, "track"),
            };
            let (icon, layout) = match added.kind {
                HitKind::Artist => (Icon::Artist, &snap.layouts.queued_artist),
                HitKind::Playlist => (Icon::Playlist, &snap.layouts.queued_playlist),
                _ => (Icon::Album, &snap.layouts.queued_album),
            };
            (
                icon,
                format!("Added {}", fmt::count(enq.count, "track")),
                what,
                format!("{total} in all · {} · by {by}", when(true)),
                layout,
            )
        }
        HitKind::Track => (
            if enq.position == 0 {
                Icon::Play
            } else {
                Icon::Note
            },
            if enq.position == 0 {
                "Playing now".to_string()
            } else {
                "Added to queue".to_string()
            },
            track_block(&first.track, web, None),
            format!("{total} long · {} · by {by}", when(false)),
            &snap.layouts.queued,
        ),
    };
    let mut scene = Scene::new(snap, icon, &heading, snap.icons.accent());
    scene.track = Some(&first.track);
    scene.cover = cover;
    scene.artist_art = artist_art;
    scene.artist_banner = artist_banner;
    scene.toast = Some(Toast {
        added: added_line,
        added_meta,
        count: enq.count,
        duration_ms: total_ms,
        position: enq.position,
        eta_ms,
        source: added.source,
        requested_by: first.requested_by,
    });
    finish(&scene, layout, false)
}

/// A list entry's track, under the same names a scene answers for its own.
fn track_vars(t: &TrackRow, web: Option<&str>) -> Vec<(&'static str, String)> {
    let opt = |s: Option<&str>| s.map(fmt::escape_md).unwrap_or_default();
    let num = |n: Option<i64>| n.map(|n| n.to_string()).unwrap_or_default();
    vec![
        ("track", track_block(t, web, None)),
        ("track.line", title_line(t, web, None)),
        ("track.title", fmt::escape_md(&t.title)),
        ("track.artist", fmt::escape_md(&t.artist)),
        ("track.album", opt(t.album.as_deref())),
        ("track.album_artist", opt(t.album_artist.as_deref())),
        ("track.year", num(t.year)),
        ("track.genre", opt(t.genre.as_deref())),
        ("track.number", num(t.track_no)),
        ("track.disc", num(t.disc_no)),
        ("track.duration", fmt::duration(t.duration_ms.max(0) as u64)),
        (
            "track.url",
            link_for(web, None, &title_query(t)).unwrap_or_default(),
        ),
        (
            "track.artist_url",
            link_for(web, None, &t.artist).unwrap_or_default(),
        ),
        (
            "track.album_url",
            album_url(t, web, None).unwrap_or_default(),
        ),
        (
            "track.title_link",
            linked_to(&t.title, web, None, &title_query(t)),
        ),
        ("track.artist_link", linked(&t.artist, web, &t.artist)),
        (
            "track.album_link",
            match t.album.as_deref() {
                Some(a) if !a.is_empty() => linked(a, web, &format!("{a} {}", t.artist)),
                _ => String::new(),
            },
        ),
    ]
}

/// One page of the queue (0-based, clamped), laid out by the `queue` layout.
pub fn queue_page(snap: &PlayerSnapshot, page: usize) -> Message {
    let web = snap.web_base.as_deref();
    let mut scene = Scene::new(snap, Icon::Queue, "Queue", snap.icons.accent());
    let list = snap
        .queue
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let mut vars = track_vars(&item.track, web);
            vars.push(("requester", requester(item)));
            vars.push((
                "requester.id",
                if item.autoplay {
                    String::new()
                } else {
                    item.requested_by.get().to_string()
                },
            ));
            vars.push(("eta", fmt::duration(snap.eta_ms(i))));
            Entry { vars }
        })
        .collect();
    scene.paged(list, page, &snap.layouts.queue, Paging::Queue);
    scene.origin = Some(Origin::Queue(scene.page as u32));
    finish(&scene, &snap.layouts.queue, false).ephemeral()
}

/// What this server heard, newest first, from the play log (a stop or a loop does not erase what
/// was heard), laid out by the `history` layout.
pub fn history(snap: &PlayerSnapshot, plays: &[PlayEntry], page: usize) -> Message {
    let web = snap.web_base.as_deref();
    let icons = &snap.icons;
    let mut scene = Scene::new(snap, Icon::List, "History", icons.accent());
    let list = plays
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
            let requester_id = p
                .requested_by
                .as_deref()
                .and_then(|u| u.parse::<u64>().ok());
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
                    ("track.line", line.clone()),
                    ("track", line),
                    ("track.title", fmt::escape_md(&p.title)),
                    ("track.artist", fmt::escape_md(&p.artist)),
                    (
                        "track.title_link",
                        linked(&p.title, web, &format!("{} {}", p.title, p.artist)),
                    ),
                    ("track.artist_link", linked(&p.artist, web, &p.artist)),
                    ("play.at", format!("<t:{}:R>", p.started_at / 1000)),
                    (
                        "play.length",
                        if p.ms_played > 0 {
                            fmt::duration(p.ms_played as u64)
                        } else {
                            String::new()
                        },
                    ),
                    (
                        "requester",
                        requester_id
                            .map(|u| mention(UserId::new(u)))
                            .unwrap_or_default(),
                    ),
                    (
                        "requester.id",
                        requester_id.map(|u| u.to_string()).unwrap_or_default(),
                    ),
                    ("play.counted", counted),
                    ("play.counted_for", p.scrobbled_for.to_string()),
                ],
            }
        })
        .collect();
    scene.paged(list, page, &snap.layouts.history, Paging::History);
    scene.origin = Some(Origin::History(scene.page as u32));
    finish(&scene, &snap.layouts.history, false).ephemeral()
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
                HitKind::Playlist => format!(
                    "`{n:>2}.` {} **{}** · {} · {}",
                    icons.get(Icon::Playlist).markup(),
                    fmt::escape_md(&h.title),
                    fmt::escape_md(&h.subtitle),
                    fmt::count(h.track_count.max(0) as usize, "track")
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
                HitKind::Playlist => ("pl", "Playlist", Icon::Playlist),
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

/// Which reply layout an acknowledgement is laid out by.
#[derive(Clone, Copy)]
enum Reply {
    Done,
    Notice,
    Error,
}

/// A reply to a command or a button: the title is `{heading}`, the rest `{detail}`, and what is
/// playing (if anything) is there for the layout to mention.
fn reply(snap: &PlayerSnapshot, kind: Reply, title: &str, detail: &str) -> Message {
    let (icon, accent, layout) = match kind {
        Reply::Done => (Icon::Check, snap.icons.accent(), &snap.layouts.done),
        Reply::Notice => (Icon::Warning, accent::NOTICE, &snap.layouts.notice),
        Reply::Error => (Icon::Cross, accent::ERROR, &snap.layouts.error),
    };
    let mut scene = Scene::new(snap, icon, title, accent);
    scene.detail = Some(detail);
    with_current(&mut scene, snap);
    finish(&scene, layout, false).ephemeral()
}

/// Whatever is playing, for a message that is not about it but may mention it.
fn with_current<'a>(scene: &mut Scene<'a>, snap: &'a PlayerSnapshot) {
    if let Some(cur) = &snap.current {
        scene.track = Some(&cur.item.track);
        scene.links = cur.links.as_ref();
        scene.cur = Some(cur);
        scene.cover = cur.cover.as_ref();
        scene.artist_art = cur.artist_art.as_ref();
        scene.artist_banner = cur.artist_banner.as_ref();
    }
}

/// A listener's vote to skip with the tally, laid out by the `vote` layout; public, so the
/// channel sees how far along it is. Once it passed, the `vote_passed` layout instead.
pub fn vote(snap: &PlayerSnapshot, tally: &VoteTally, by: UserId) -> Message {
    let (icon, heading, layout) = if tally.passed {
        (Icon::Next, "Skipped by vote", &snap.layouts.vote_passed)
    } else {
        (Icon::Vote, "Vote to skip", &snap.layouts.vote)
    };
    let mut scene = Scene::new(snap, icon, heading, snap.icons.accent());
    scene.vote = Some(tally);
    scene.voter = Some(by);
    with_current(&mut scene, snap);
    finish(&scene, layout, false)
}

/// Something went wrong: red, laid out by the `error` layout.
pub fn error(snap: &PlayerSnapshot, title: &str, detail: &str) -> Message {
    reply(snap, Reply::Error, title, detail)
}

/// A heads-up rather than a failure: yellow, laid out by the `notice` layout.
pub fn notice(snap: &PlayerSnapshot, title: &str, detail: &str) -> Message {
    reply(snap, Reply::Notice, title, detail)
}

/// A short, positive acknowledgement ("Skipped **Title**"), laid out by the `done` layout. With
/// no detail the default layout is the title line alone: a divider with nothing under it reads
/// as a mistake.
pub fn ok(snap: &PlayerSnapshot, title: &str, detail: &str) -> Message {
    reply(snap, Reply::Done, title, detail)
}

/// The bot is busy in another channel of this guild; name the siblings that are free.
pub fn busy(
    snap: &PlayerSnapshot,
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
    notice(snap, "Already playing elsewhere", &detail)
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
    let skip = gs.skip_label();
    let mut body = header(
        &icons.get(Icon::Gear),
        "Settings",
        Some(&format!("{} in this server", snap.bot_name)),
    );
    body.push(text(format!(
        "**DJ roles** · {dj}\n**Volume** · {volume}\n**Normalize volume** · {}\n**Autoplay** · {autoplay}\n**24/7** · {always_on}\n**Skipping** · {skip}\n**Re-post controller when it scrolls away** · {}",
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

/// The equalizer panel: the curve as a picture, the bands as numbers, a preset to pick, a band
/// to choose and nudge, and the switch. Every control edits the panel in place.
pub fn equalizer(snap: &PlayerSnapshot, band: usize, picture: Option<Vec<u8>>) -> Message {
    let icons = &snap.icons;
    let cfg = &snap.eq;
    let state = if cfg.enabled { "on" } else { "off" };
    let mut body = header(
        &icons.get(Icon::Equalizer),
        "Equalizer",
        Some(&format!("{} · {state}", eq::label(cfg))),
    );
    if picture.is_some() {
        body.push(gallery(vec![Media::attachment("eq.png")]));
    }
    body.push(text(eq::table(cfg)));
    body.push(separator(false, Spacing::Large));
    let current = eq::preset_of(cfg).map(|p| p.name);
    let mut presets: Vec<SelectOption> = eq::PRESETS
        .iter()
        .map(|p| {
            let mut o = SelectOption::new(p.name.to_string(), format!("p:{}", p.name));
            if current == Some(p.name) {
                o = o.default(true);
            }
            o
        })
        .collect();
    if current.is_none() {
        presets.insert(0, SelectOption::new("Custom", "custom").default(true));
    }
    body.push(row(vec![Component::StringSelect {
        custom_id: id(snap, Action::Select("eq_preset".into())),
        placeholder: Some("Preset".into()),
        options: presets,
        min: 1,
        max: 1,
        disabled: false,
    }]));
    let band = band.min(eq::FREQS.len() - 1);
    let bands: Vec<SelectOption> = cfg
        .bands
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let hz = if b.freq >= 1000.0 {
                format!("{} kHz", b.freq / 1000.0)
            } else {
                format!("{} Hz", b.freq)
            };
            let mut o =
                SelectOption::new(hz, format!("b:{i}")).description(format!("{:+.1} dB", b.gain));
            if i == band {
                o = o.default(true);
            }
            o
        })
        .collect();
    body.push(row(vec![Component::StringSelect {
        custom_id: id(snap, Action::Select("eq_band".into())),
        placeholder: Some("Band to nudge".into()),
        options: bands,
        min: 1,
        max: 1,
        disabled: false,
    }]));
    let step = |n: i8| {
        button(
            Button::new(ButtonStyle::Secondary, id(snap, Action::EqStep(n)))
                .label(format!("{n:+} dB")),
        )
    };
    body.push(row(vec![
        step(-3),
        step(-1),
        step(1),
        step(3),
        button(Button::new(ButtonStyle::Secondary, id(snap, Action::EqFlat)).label("Flat")),
    ]));
    body.push(row(vec![button(
        Button::new(ButtonStyle::Secondary, id(snap, Action::EqToggle))
            .emoji(icons.get(Icon::Equalizer))
            .label(if cfg.enabled { "Turn off" } else { "Turn on" }),
    )]));
    let msg = Message::new(vec![container(icons.accent(), body)]).ephemeral();
    match picture {
        Some(png) => msg.attach("eq.png", png),
        None => msg,
    }
}

/// One page of a track's lyrics, laid out by the `lyrics` layout; its page buttons turn the
/// pages. The cover and the file facts are there when the track is the one playing.
pub fn lyrics(snap: &PlayerSnapshot, track: &TrackRow, pages: &[String], page: usize) -> Message {
    let mut scene = Scene::new(snap, Icon::Lyrics, "Lyrics", snap.icons.accent());
    scene.track = Some(track);
    if let Some(cur) = snap
        .current
        .as_ref()
        .filter(|c| c.item.track.id == track.id)
    {
        scene.links = cur.links.as_ref();
        scene.cur = Some(cur);
        scene.cover = cur.cover.as_ref();
        scene.artist_art = cur.artist_art.as_ref();
        scene.artist_banner = cur.artist_banner.as_ref();
    }
    scene.paged_text(pages, page, &snap.layouts.lyrics, Paging::Lyrics);
    scene.origin = Some(Origin::Lyrics(scene.page as u32));
    finish(&scene, &snap.layouts.lyrics, false).ephemeral()
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
    use chordia_contracts::user::EqConfig;
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
            bot_user_id: Some(9),
            guild_name: Some("Test guild".into()),
            guild_icon: None,
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
                artist_banner: None,
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
            muted: false,
            eq: EqConfig::default(),
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
                Added {
                    kind: HitKind::Track,
                    source: None,
                    url: None,
                },
                Some(&c),
                None,
                None,
            ),
            queued(
                &s,
                &s.queue[..1],
                &Enqueued {
                    position: 0,
                    count: 1,
                },
                Added {
                    kind: HitKind::Track,
                    source: None,
                    url: None,
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
                Added {
                    kind: HitKind::Album,
                    source: Some("Discovery"),
                    url: None,
                },
                Some(&c),
                Some(&c),
                None,
            ),
            queue_page(&s, 0),
            queue_page(&s, 99),
            queue_page(&snap(0, false, false), 0),
            history(&s, &[], 0),
            history(&s, &plays(25), 1),
            history(&s, &plays(25), 99),
            error(&s, "Couldn't do that", "reason"),
            notice(&s, "Heads up", "detail"),
            ok(&s, "Skipped", "**x**"),
            ok(&s, "Left", ""),
            busy(&s, "Chordia", ChannelId::new(555), 3, &["Chordia 2".into()]),
            busy(&s, "Chordia", ChannelId::new(555), 3, &[]),
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
        // header, divider, the section (everything beside the art); the rows sit under the box
        assert_eq!(kids.len(), 3);
        let head = kids[0]["content"].as_str().unwrap();
        assert_eq!(head, "### ▶ Now playing in <#555>");
        assert_eq!(kids[2]["type"], 9);
        assert_eq!(kids[2]["accessory"]["type"], 11);
        assert_eq!(
            kids[2]["accessory"]["media"]["url"],
            "attachment://cover-c.jpg"
        );
        let body = kids[2]["components"][0]["content"].as_str().unwrap();
        // Without the emoji set the bar is its text fallback: 12 cells, playhead a fifth in.
        assert_eq!(
            body,
            "**One More Time**\nDaft Punk · *Discovery*\n-# Requested by <@42>\n━━●───────── 1:05 / 5:20\n-# 3 tracks in queue (16:00) · Volume: 80%"
        );
        let row1 = b["components"][1]["components"].as_array().unwrap();
        let row2 = b["components"][2]["components"].as_array().unwrap();
        assert_eq!(row1.len(), 5);
        assert_eq!(row2.len(), 5);
        // Every button is the neutral style; state lives in the icons, never in a label.
        assert!(row1.iter().chain(row2.iter()).all(|b| b["style"] == 2));
        assert!(row1.iter().chain(row2.iter()).all(|b| b["label"].is_null()));
        assert_eq!(row1[2]["custom_id"], "cd:1:1:777:pl");
        // Playing, so the play/pause button offers pause.
        assert_eq!(row1[2]["emoji"]["name"], "⏸");
        // Without the emoji set the state icons fall back to glyphs.
        assert_eq!(row1[0]["emoji"]["name"], "🔁");
        assert_eq!(row2[1]["emoji"]["name"], "🕘");
        assert_eq!(row2[2]["emoji"]["name"], "📻");
        assert_eq!(row2[4]["custom_id"], "cd:1:1:777:lv");
        assert_eq!(row2[4]["emoji"]["name"], "🚪");
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
        assert_eq!(b["components"][1]["components"][2]["emoji"]["name"], "▶");
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
        // The first button row sits under the box.
        let pause = &b["components"][1]["components"][2]["emoji"];
        assert_eq!(pause["name"], "cd_pause");
        assert_eq!(pause["id"], "1001");
        assert!(!b.to_string().contains('⏸'));
        // The bar is emojis too: the left cap, ten middles, the right cap.
        let progress = kids[2]["content"].as_str().unwrap();
        // 65 s of 320 s: the first two segments full, the playhead mid-third.
        assert!(progress.contains("<:cd_bar_l3:"), "{progress}");
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
        let e = error(&s, "x", "y").body();
        assert_eq!(e["components"][0]["accent_color"], accent::ERROR);
    }

    #[test]
    fn a_bare_acknowledgement_has_no_dangling_divider() {
        let s = snap(0, false, false);
        let b = ok(&s, "Left", "").body();
        let kids = b["components"][0]["components"].as_array().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["type"], 10);
        let b = ok(&s, "Skipped", "**x**").body();
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
        // header, divider, the section's text on its own
        assert_eq!(kids.len(), 3);
        assert_eq!(kids[2]["type"], 10);
        assert!(b["attachments"].as_array().unwrap().is_empty());
        // With nothing queued the line says so.
        assert!(kids[2]["content"]
            .as_str()
            .unwrap()
            .contains("0 tracks in queue (0:00)"));
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

    /// The blocks in one container in the bot's colour, the way the defaults are.
    fn layouts_with(view: LayoutView, blocks: Vec<LayoutBlock>) -> BotLayouts {
        let mut l = BotLayouts::default();
        *l.view_mut(view) = ViewLayout {
            blocks: vec![LayoutBlock::Container {
                accent: ContainerAccent::Bot,
                blocks,
            }],
        };
        l.validate().unwrap();
        l
    }

    #[test]
    fn containers_are_blocks_with_a_colour_of_their_own() {
        let mut s = snap(0, true, false);
        let l = BotLayouts {
            now_playing: ViewLayout {
                blocks: vec![
                    LayoutBlock::Text {
                        content: "outside".into(),
                    },
                    LayoutBlock::Container {
                        accent: ContainerAccent::Fixed {
                            hex: "#123456".into(),
                        },
                        blocks: vec![
                            LayoutBlock::Text {
                                content: "{track.title}".into(),
                            },
                            LayoutBlock::Separator {
                                divider: true,
                                spacing: SeparatorSpacing::Small,
                            },
                        ],
                    },
                    LayoutBlock::Container {
                        accent: ContainerAccent::None,
                        blocks: vec![LayoutBlock::Text {
                            content: "-# {file.codec}".into(),
                        }],
                    },
                    LayoutBlock::Container {
                        accent: ContainerAccent::Bot,
                        blocks: vec![LayoutBlock::Text {
                            content: "{nothing.here.but.unknown}".into(),
                        }],
                    },
                    LayoutBlock::Separator {
                        divider: false,
                        spacing: SeparatorSpacing::Large,
                    },
                ],
            },
            ..Default::default()
        };
        l.validate().unwrap();
        s.layouts = Arc::new(l);
        let m = now_playing(&s, false);
        m.validate().unwrap_or_else(|e| panic!("{e}"));
        let b = m.body();
        let top = b["components"].as_array().unwrap();
        // The text, two containers, the bot-coloured one; the trailing gap is dropped, and so is
        // the separator that ended the first container.
        assert_eq!(top.len(), 4, "{b}");
        assert_eq!(top[0]["type"], 10);
        assert_eq!(top[0]["content"], "outside");
        assert_eq!(top[1]["type"], 17);
        assert_eq!(top[1]["accent_color"], 0x12_34_56);
        assert_eq!(top[1]["components"].as_array().unwrap().len(), 1);
        assert_eq!(top[2]["type"], 17);
        assert!(top[2]["accent_color"].is_null());
        assert_eq!(top[3]["accent_color"], accent::BRAND);
    }

    #[test]
    fn a_custom_layout_renders_and_attaches_only_what_it_shows() {
        let mut s = snap(2, true, true);
        s.layouts = Arc::new(layouts_with(
            LayoutView::NowPlaying,
            vec![
                LayoutBlock::Text {
                    content:
                        "Now: **{track.title}** by {track.artist} for {requester} ({player.position}/{track.duration})"
                            .into(),
                },
                LayoutBlock::Text {
                    content: "{player.progress_bar:6}\n-# {queue.count} queued · {player.volume_bar:4} {player.volume}%"
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
                    texts: vec![
                        "{emoji:listening} {channel} · {emoji:nope} · {bot} · {server}".into(),
                    ],
                    accessory: Accessory::Image {
                        source: ImageSource::BotAvatar,
                    },
                },
                LayoutBlock::Section {
                    texts: vec!["{track.line}".into()],
                    accessory: Accessory::Button {
                        button: ButtonSpec::Link {
                            label: "Open {track.album}".into(),
                            url: "{track.album_url}".into(),
                        },
                    },
                },
                LayoutBlock::Section {
                    texts: vec!["-# {file} · {file.codec} · {file.bitrate} · {file.gain}".into()],
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
            "🎧 <#555> · {emoji:nope} · Chordia 2 · Test guild"
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
        assert!(
            k[2]["content"]
                .as_str()
                .unwrap()
                .ends_with("Opus 96k · RG −7.1 dB · FLAC · 96 kbps · −7.1 dB"),
            "{}",
            k[2]["content"]
        );
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
        // The server's version has no container, so the text is the message.
        let b = idle(&s).body();
        assert_eq!(b["components"].as_array().unwrap().len(), 1);
        assert_eq!(b["components"][0]["content"], "Quiet in <#555>");
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
        let t = track("c", "Song");
        lyrics(&s, &t, &pages, 0).validate().unwrap();
        lyrics(&s, &t, &pages, 9).validate().unwrap();
        lyrics(&s, &t, &[], 0).validate().unwrap();
    }

    #[test]
    fn lyrics_page_through_their_own_layout() {
        let s = snap(0, true, false);
        let t = track("c", "One More Time");
        let pages: Vec<String> = vec!["one".into(), "two".into(), "three".into()];
        let m = lyrics(&s, &t, &pages, 1);
        m.validate().unwrap();
        let k = kids(&m);
        let head = k[0]["content"].as_str().unwrap();
        assert!(head.ends_with(" One More Time\n-# Daft Punk"), "{head}");
        assert_eq!(k[2]["content"], "two");
        let lb = m.body();
        let nav = lb["components"][1]["components"].as_array().unwrap();
        assert_eq!(nav[0]["custom_id"], "cd:1:1:777:lyf");
        assert_eq!(nav[1]["custom_id"], "cd:1:1:777:lyp:0");
        assert_eq!(nav[2]["label"], "2/3");
        assert_eq!(nav[3]["custom_id"], "cd:1:1:777:lyp:2");
        assert_eq!(nav[4]["custom_id"], "cd:1:1:777:lyl");
        // The playing track's cover is there for a layout that shows it; another track's is not.
        let mut s = snap(0, true, true);
        s.layouts = Arc::new(BotLayouts {
            lyrics: ViewLayout {
                blocks: vec![LayoutBlock::Section {
                    texts: vec!["{lyrics}".into()],
                    accessory: Accessory::Image {
                        source: ImageSource::Cover,
                    },
                }],
            },
            ..Default::default()
        });
        assert!(lyrics(&s, &t, &pages, 0)
            .body()
            .to_string()
            .contains("cover-c.jpg"));
        assert!(!lyrics(&s, &track("d", "Other"), &pages, 0)
            .body()
            .to_string()
            .contains("cover-c.jpg"));
    }

    #[test]
    fn replies_and_toasts_use_their_own_layouts() {
        let mut s = snap(3, true, false);
        // A reply laid out without a container: its text is top-level, and the title, the
        // detail and the player's facts come through as variables.
        s.layouts = Arc::new(BotLayouts {
            done: ViewLayout {
                blocks: vec![LayoutBlock::Text {
                    content: "{heading} · {detail} · {player.volume}%".into(),
                }],
            },
            ..Default::default()
        });
        let b = ok(&s, "Volume", "80").body();
        assert_eq!(b["components"][0]["type"], 10);
        assert_eq!(b["components"][0]["content"], "Volume · 80 · 80%");
        // Errors keep their colour through a container in "the bot's colour".
        let e = error(&s, "x", "y").body();
        assert_eq!(e["components"][0]["accent_color"], accent::ERROR);
        // An artist add takes the artist layout, an album add the album layout.
        s.layouts = Arc::new(BotLayouts {
            queued_artist: ViewLayout {
                blocks: vec![LayoutBlock::Text {
                    content: "artist: {added} ({added.count})".into(),
                }],
            },
            ..Default::default()
        });
        let items: Vec<QueueItem> = (0..3)
            .map(|i| item(&i.to_string(), &format!("T{i}")))
            .collect();
        let enq = Enqueued {
            position: 1,
            count: 3,
        };
        let m = queued(
            &s,
            &items,
            &enq,
            Added {
                kind: HitKind::Artist,
                source: Some("Daft Punk"),
                url: None,
            },
            None,
            None,
            None,
        )
        .body();
        assert_eq!(m["components"][0]["content"], "artist: **Daft Punk** (3)");
        let m = queued(
            &s,
            &items,
            &enq,
            Added {
                kind: HitKind::Album,
                source: Some("Discovery"),
                url: None,
            },
            None,
            None,
            None,
        )
        .body();
        assert_eq!(m["components"][0]["type"], 17);
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
            lyrics(&s, &track("c", "t"), &pages, 0),
            lyrics(&s, &track("c", "t"), &pages, 2),
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
        // The page buttons sit under the box.
        let fb = first.body();
        let nav = fb["components"][1]["components"].as_array().unwrap();
        assert_eq!(nav[0]["disabled"], true);
        assert_eq!(nav[0]["custom_id"], "cd:1:1:777:qf");
        assert_eq!(nav[3]["disabled"], false);
        assert_eq!(nav[2]["label"], "1/3");
        // The header is a section with the Clear button beside it.
        let head = rows[0]["components"][0]["content"].as_str().unwrap();
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
        let lb = last.body();
        let nav = lb["components"][1]["components"].as_array().unwrap();
        assert_eq!(nav[3]["disabled"], true);
        assert_eq!(nav[4]["custom_id"], "cd:1:1:777:ql");
        assert_eq!(nav[2]["label"], "3/3");
        // An empty queue says so and has no paging.
        let empty = queue_page(&snap(0, false, false), 0);
        let rows = kids(&empty);
        assert_eq!(rows.last().unwrap()["content"], "-# The queue is empty.");
    }

    #[test]
    fn page_buttons_go_where_they_are_put_or_nowhere() {
        let mut s = snap(25, true, false);
        let list = LayoutBlock::List {
            item: "{index}. {track.title}".into(),
            empty: "-".into(),
            page_size: 10,
        };
        s.layouts = Arc::new(BotLayouts {
            queue: ViewLayout {
                blocks: vec![LayoutBlock::Pager, list.clone(), LayoutBlock::Pager],
            },
            ..Default::default()
        });
        let m = queue_page(&s, 1);
        m.validate().unwrap();
        let top = m.body()["components"].as_array().unwrap().clone();
        // No container: the rows are top-level, one before the entries and one after.
        assert_eq!(top.len(), 3);
        assert_eq!(top[0]["type"], 1);
        assert_eq!(top[2]["type"], 1);
        assert_eq!(top[0]["components"][2]["label"], "2/3");
        assert_eq!(top[0]["components"][1]["emoji"]["name"], "◀");
        assert!(top[1]["content"].as_str().unwrap().starts_with("11. "));
        // Without page buttons the list stays on its first page, whatever page is asked for.
        s.layouts = Arc::new(BotLayouts {
            queue: ViewLayout { blocks: vec![list] },
            ..Default::default()
        });
        let m = queue_page(&s, 2);
        m.validate().unwrap();
        let top = m.body()["components"].as_array().unwrap().clone();
        assert_eq!(top.len(), 1);
        assert!(top[0]["content"].as_str().unwrap().starts_with(" 1. "));
    }

    #[test]
    fn a_vote_reads_its_tally_and_a_control_on_a_page_says_where_it_sits() {
        let s = snap(2, true, false);
        let tally = VoteTally {
            count: 1,
            needed: 2,
            listeners: 3,
            percent: 50,
            passed: false,
        };
        let m = vote(&s, &tally, UserId::new(42));
        m.validate().unwrap();
        // Public: the channel sees the tally.
        assert_eq!(m.body()["flags"].as_u64().unwrap_or(0) & 64, 0);
        let body = kids(&m)[0]["content"].as_str().unwrap().to_string();
        assert!(body.contains("Vote to skip"), "{body}");
        assert!(
            body.contains("<@42> wants to skip **One More Time**"),
            "{body}"
        );
        assert!(body.contains("1 of 2 votes · 50% of 3 listening"), "{body}");
        let passed = vote(
            &s,
            &VoteTally {
                count: 2,
                passed: true,
                ..tally
            },
            UserId::new(42),
        );
        let body = kids(&passed)[0]["content"].as_str().unwrap().to_string();
        assert!(body.contains("Skipped by vote"), "{body}");
        assert!(body.contains("2 votes"), "{body}");

        // A control on the queue page carries the page, so a press redraws it.
        let mut s = snap(25, true, false);
        s.layouts = Arc::new(BotLayouts {
            queue: ViewLayout {
                blocks: vec![
                    LayoutBlock::List {
                        item: "{index}. {track.title}".into(),
                        empty: "-".into(),
                        page_size: 10,
                    },
                    LayoutBlock::Row {
                        buttons: vec![ButtonSpec::Control {
                            control: ControlButton::Clear,
                        }],
                    },
                    LayoutBlock::Pager,
                ],
            },
            ..Default::default()
        });
        let m = queue_page(&s, 1);
        m.validate().unwrap();
        let top = m.body()["components"].as_array().unwrap().clone();
        assert_eq!(top[1]["components"][0]["custom_id"], "cd:1:1:777:cl:@q1");
        // The controller's own controls say nothing.
        let c = now_playing(&s, false).body();
        assert_eq!(
            c["components"][1]["components"][2]["custom_id"],
            "cd:1:1:777:pl"
        );
    }

    #[test]
    fn the_toast_says_how_long_and_when() {
        let mut s = snap(3, true, false);
        let one = Added {
            kind: HitKind::Track,
            source: None,
            url: None,
        };
        let text = |m: Message| m.body().to_string();
        let enq = |position: usize, count: usize| Enqueued { position, count };
        let body = text(queued(&s, &s.queue[..1], &enq(2, 1), one, None, None, None));
        assert!(body.contains("5:20 long · plays in ~"), "{body}");
        assert!(body.contains("(#2 in queue) · by <@42>"), "{body}");
        let body = text(queued(&s, &s.queue[..1], &enq(0, 1), one, None, None, None));
        assert!(
            body.contains("5:20 long · playing now · by <@42>"),
            "{body}"
        );
        s.shuffle = true;
        let body = text(queued(&s, &s.queue[..1], &enq(2, 1), one, None, None, None));
        assert!(
            body.contains("5:20 long · shuffled in · by <@42>"),
            "{body}"
        );
        s.shuffle = false;
        let album = Added {
            kind: HitKind::Album,
            source: Some("Discovery"),
            url: None,
        };
        let body = text(queued(&s, &s.queue, &enq(2, 3), album, None, None, None));
        assert!(body.contains("16:00 in all · first plays in ~"), "{body}");
        // A playlist links to its page on the web client.
        s.web_base = Some("https://chordia.example".into());
        let playlist = Added {
            kind: HitKind::Playlist,
            source: Some("Evening drive"),
            url: Some("/app/playlists/abc"),
        };
        let body = text(queued(&s, &s.queue, &enq(0, 3), playlist, None, None, None));
        assert!(
            body.contains("**[Evening drive](https://chordia.example/app/playlists/abc)**"),
            "{body}"
        );
        assert!(
            body.contains("16:00 in all · playing now · by <@42>"),
            "{body}"
        );
    }

    #[test]
    fn the_equalizer_panel_names_its_state_and_validates() {
        let mut s = snap(0, true, false);
        let m = equalizer(&s, 2, None);
        m.validate().unwrap();
        let k = kids(&m);
        assert!(k[0]["content"]
            .as_str()
            .unwrap()
            .contains("Equalizer\n-# off · off"));
        // Rows: the preset select, the band select (its third option chosen), the nudges, the switch.
        assert_eq!(k[k.len() - 2]["components"].as_array().unwrap().len(), 5);
        let band_select = &k[k.len() - 3]["components"][0];
        assert_eq!(band_select["options"][2]["default"], true);
        assert_eq!(band_select["options"][2]["label"], "125 Hz");
        s.eq = eq::preset_config(eq::preset("Rock").unwrap());
        let m = equalizer(&s, 0, Some(vec![0u8; 8]));
        let k = kids(&m);
        assert!(k[0]["content"].as_str().unwrap().contains("Rock · on"));
        assert_eq!(k[2]["type"], 12);
        assert_eq!(m.body()["attachments"][0]["filename"], "eq.png");
        assert!(m.body().to_string().contains("+5  +4  +3"));
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
        let hb = m.body();
        let nav = hb["components"][1]["components"].as_array().unwrap();
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
        let it = vec![item("x", "F**K # 1")];
        let m = queued(
            &s,
            &it,
            &Enqueued {
                position: 1,
                count: 1,
            },
            Added {
                kind: HitKind::Track,
                source: None,
                url: None,
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
            busy(&s, "A", ChannelId::new(1), 1, &["C".into()]),
            idle(&s),
        ] {
            assert!(!m.body().to_string().contains('—'), "{}", m.body());
        }
    }
}
