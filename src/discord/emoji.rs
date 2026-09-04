//! The bot's own icon set, as Discord **application emojis**.
//!
//! Default Unicode symbols are what made the first controller look cheap: `⏯` and `🔀` render in
//! whatever emoji font the viewer has, at whatever colour that font chose. An application can own
//! up to 2000 emojis of its own that work in every server it is in without using anyone's emoji
//! slots, so the bot uploads a matching set once and uses those everywhere: headers, buttons, list
//! markers, and the now-playing progress bar, which is a row of bar-segment emojis rather than
//! text.
//!
//! The icons are [Phosphor](https://phosphoricons.com) (MIT; vendored under `assets/phosphor/`),
//! the `fill` weight; the bar segments are drawn here. Everything is tinted in one colour and
//! rendered to 128 px PNGs with resvg. The colour is a per-bot setting (default the Chordia
//! accent); changing it regenerates the whole set, so a bot can match its server's palette.
//!
//! Emojis are named `cd_<icon>`. Provisioning lists the set (one request), uploads only what is
//! missing, and recreates everything when the colour changed. It talks to Discord through
//! [`crate::discord::rest`] so a rate limit comes back as a value the theme job can schedule
//! around, never as a sleep. If the set is unavailable, views fall back to Unicode glyphs.

use std::collections::HashMap;

use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::discord::rest::{Rest, RestError};
use crate::discord::ui::fmt::{accent, glyph};
use crate::discord::ui::v2::Emoji;

/// The Chordia accent: the brand pink the app's icons are drawn in (`--primary`, as sRGB).
pub const DEFAULT_HEX: &str = "#f2258c";
/// Discord recommends 128×128 for custom emoji and caps the file at 256 KiB; a tinted glyph is a
/// few kilobytes.
const SIZE: u32 = 128;
const NAME_PREFIX: &str = "cd_";
/// Bumped whenever an existing emoji name changes meaning (a redrawn segment, say). It is part of
/// the applied stamp, so a set made by an older build is regenerated rather than reused by name.
pub const SET_VERSION: u32 = 2;

/// What `emoji_hex_applied` records: the colour and the set version it was made with.
pub fn applied_stamp(hex: &str) -> String {
    format!("{hex}@v{SET_VERSION}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    PlayPause,
    Next,
    Prev,
    Stop,
    Shuffle,
    LoopQueue,
    LoopTrack,
    Volume,
    VolumeDown,
    Radio,
    Note,
    Queue,
    Search,
    Warning,
    Cross,
    Check,
    Lyrics,
    Info,
    Wave,
    Gear,
    Album,
    Artist,
    Listening,
    List,
    /// Progress-bar segments: left cap, middle, right cap, five states each (see [`BarState`]).
    BarL0,
    BarL1,
    BarL2,
    BarL3,
    BarL4,
    BarM0,
    BarM1,
    BarM2,
    BarM3,
    BarM4,
    BarR0,
    BarR1,
    BarR2,
    BarR3,
    BarR4,
}

macro_rules! phosphor {
    ($( $variant:ident => $name:literal, $file:literal, $fallback:expr ; )*) => {
        impl Icon {
            const PHOSPHOR: &'static [Icon] = &[$(Icon::$variant),*];

            fn phosphor_name(self) -> Option<&'static str> {
                match self { $(Icon::$variant => Some(concat!("cd_", $name)),)* _ => None }
            }

            fn phosphor_fallback(self) -> Option<&'static str> {
                match self { $(Icon::$variant => Some($fallback),)* _ => None }
            }

            fn phosphor_svg(self) -> Option<&'static str> {
                match self {
                    $(Icon::$variant => Some(include_str!(concat!("../../assets/phosphor/", $file, ".svg"))),)*
                    _ => None,
                }
            }
        }
    };
}

phosphor! {
    Play => "play", "play", glyph::PLAY;
    Pause => "pause", "pause", glyph::PAUSE;
    PlayPause => "playpause", "play-pause", glyph::PLAY_PAUSE;
    Next => "next", "skip-forward", glyph::NEXT;
    Prev => "prev", "skip-back", glyph::PREV;
    Stop => "stop", "stop", glyph::STOP;
    Shuffle => "shuffle", "shuffle", glyph::SHUFFLE;
    LoopQueue => "loop", "repeat", glyph::LOOP_QUEUE;
    LoopTrack => "loopone", "repeat-once", glyph::LOOP_TRACK;
    Volume => "volume", "speaker-high", glyph::VOLUME;
    VolumeDown => "volumedown", "speaker-low", glyph::VOLUME_DOWN;
    Radio => "radio", "radio", glyph::RADIO;
    Note => "note", "music-note", glyph::NOTE;
    Queue => "queue", "queue", glyph::QUEUE;
    Search => "search", "magnifying-glass", glyph::SEARCH;
    Warning => "warning", "warning", glyph::WARNING;
    Cross => "cross", "x", glyph::CROSS;
    Check => "check", "check", glyph::CHECK;
    Lyrics => "lyrics", "microphone-stage", glyph::LYRICS;
    Info => "info", "info", glyph::INFO;
    Wave => "wave", "hand-waving", glyph::WAVE;
    Gear => "gear", "gear", glyph::GEAR;
    Album => "album", "vinyl-record", "💿";
    Artist => "artist", "user", "👤";
    Listening => "listening", "headphones", "🎧";
    List => "list", "list-dashes", "📋";
}

/// Which end of the bar a segment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Left,
    Middle,
    Right,
}

/// How much of a segment is filled, and where the playhead is. The dot never floats inside a
/// filled segment: at a boundary it is split across two emojis (`DotRight` on the filled one,
/// `DotLeft` on the empty one after it), so nothing ever appears filled past the playhead.
///
/// Per cap the states are, in id order:
/// - left: `Empty`, `StartDot`, `HalfDot`, `Full`, `DotRight`
/// - middle: `Empty`, `DotLeft`, `HalfDot`, `Full`, `DotRight`
/// - right: `Empty`, `DotLeft`, `HalfDot`, `Full`, `EndDot`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarState {
    Empty,
    /// Left cap only: nothing filled, the dot on the rounded start.
    StartDot,
    /// Nothing filled, the left half of a dot on the left edge (its other half is the previous
    /// segment's `DotRight`).
    DotLeft,
    /// Filled to the middle, the dot on the middle.
    HalfDot,
    Full,
    /// Fully filled, the right half of a dot on the right edge.
    DotRight,
    /// Right cap only: fully filled, the dot on the rounded end.
    EndDot,
}

impl Icon {
    pub const BARS: &'static [Icon] = &[
        Icon::BarL0,
        Icon::BarL1,
        Icon::BarL2,
        Icon::BarL3,
        Icon::BarL4,
        Icon::BarM0,
        Icon::BarM1,
        Icon::BarM2,
        Icon::BarM3,
        Icon::BarM4,
        Icon::BarR0,
        Icon::BarR1,
        Icon::BarR2,
        Icon::BarR3,
        Icon::BarR4,
    ];

    /// The states a cap can show, in id order.
    fn bar_states(cap: Cap) -> [BarState; 5] {
        use BarState::*;
        match cap {
            Cap::Left => [Empty, StartDot, HalfDot, Full, DotRight],
            Cap::Middle => [Empty, DotLeft, HalfDot, Full, DotRight],
            Cap::Right => [Empty, DotLeft, HalfDot, Full, EndDot],
        }
    }

    /// Every emoji the bot provisions.
    pub fn all() -> impl Iterator<Item = Icon> {
        Self::PHOSPHOR.iter().chain(Self::BARS.iter()).copied()
    }

    /// The segment emoji for a cap in a state. A state the cap cannot show (a start dot on a
    /// middle segment, say) maps to the nearest one it can.
    pub fn bar(cap: Cap, state: BarState) -> Icon {
        let states = Self::bar_states(cap);
        let state = match (cap, state) {
            (Cap::Left, BarState::DotLeft) => BarState::StartDot,
            (Cap::Middle | Cap::Right, BarState::StartDot) => BarState::DotLeft,
            (Cap::Left | Cap::Middle, BarState::EndDot) => BarState::DotRight,
            (Cap::Right, BarState::DotRight) => BarState::EndDot,
            _ => state,
        };
        let j = states.iter().position(|s| *s == state).unwrap_or(0);
        let base = match cap {
            Cap::Left => 0,
            Cap::Middle => 5,
            Cap::Right => 10,
        };
        Self::BARS[base + j]
    }

    fn bar_parts(self) -> Option<(Cap, BarState)> {
        let i = Self::BARS.iter().position(|b| *b == self)?;
        let cap = [Cap::Left, Cap::Middle, Cap::Right][i / 5];
        Some((cap, Self::bar_states(cap)[i % 5]))
    }

    /// The emoji's name on Discord.
    pub fn name(self) -> &'static str {
        if let Some(n) = self.phosphor_name() {
            return n;
        }
        match self {
            Icon::BarL0 => "cd_bar_l0",
            Icon::BarL1 => "cd_bar_l1",
            Icon::BarL2 => "cd_bar_l2",
            Icon::BarL3 => "cd_bar_l3",
            Icon::BarL4 => "cd_bar_l4",
            Icon::BarM0 => "cd_bar_m0",
            Icon::BarM1 => "cd_bar_m1",
            Icon::BarM2 => "cd_bar_m2",
            Icon::BarM3 => "cd_bar_m3",
            Icon::BarM4 => "cd_bar_m4",
            Icon::BarR0 => "cd_bar_r0",
            Icon::BarR1 => "cd_bar_r1",
            Icon::BarR2 => "cd_bar_r2",
            Icon::BarR3 => "cd_bar_r3",
            Icon::BarR4 => "cd_bar_r4",
            _ => unreachable!("every icon is either phosphor or a bar"),
        }
    }

    /// The Unicode glyph used when the emoji is not (yet) available. For bar segments the
    /// fallbacks concatenate into the box-drawing slider (`━━●───`), so a bar degrades to text.
    pub fn fallback(self) -> &'static str {
        if let Some(f) = self.phosphor_fallback() {
            return f;
        }
        match self.bar_parts().map(|(_, s)| s) {
            Some(BarState::Empty) | Some(BarState::DotLeft) => "─",
            Some(BarState::Full) => "━",
            Some(BarState::StartDot)
            | Some(BarState::HalfDot)
            | Some(BarState::DotRight)
            | Some(BarState::EndDot) => "●",
            None => "?",
        }
    }

    /// The SVG for this icon in `hex`.
    fn svg(self, hex: &str) -> String {
        if let Some(src) = self.phosphor_svg() {
            return if src.contains("fill=\"currentColor\"") {
                src.replace("fill=\"currentColor\"", &format!("fill=\"{hex}\""))
            } else {
                src.replacen("<svg", &format!("<svg fill=\"{hex}\""), 1)
            };
        }
        let (cap, state) = self.bar_parts().expect("a bar icon");
        bar_svg(cap, state, hex)
    }
}

/// One progress-bar cell: a 128-unit square with a 44-unit-tall bar through the middle, edge to
/// edge so consecutive emojis read as one bar; caps are rounded. The track is the app's ink at low
/// opacity, the fill is the accent, the dot is the accent ringed in ink so it shows on both. A dot
/// on an edge is centred on it, so its two halves land on neighbouring emojis.
fn bar_svg(cap: Cap, state: BarState, hex: &str) -> String {
    const Y: f32 = 42.0;
    const H: f32 = 44.0;
    const R: f32 = 22.0;
    let shape = match cap {
        Cap::Left => format!(
            "M {R} {Y} H 128 V {} H {R} A {R} {R} 0 0 1 {R} {Y} Z",
            Y + H
        ),
        Cap::Middle => format!("M 0 {Y} H 128 V {} H 0 Z", Y + H),
        Cap::Right => format!(
            "M 0 {Y} H {} A {R} {R} 0 0 1 {} {} H 0 Z",
            128.0 - R,
            128.0 - R,
            Y + H
        ),
    };
    let (fill_to, dot_x) = match state {
        BarState::Empty => (None, None),
        BarState::StartDot => (None, Some(R)),
        BarState::DotLeft => (None, Some(0.0)),
        BarState::HalfDot => (Some(64.0), Some(64.0)),
        BarState::Full => (Some(128.0), None),
        BarState::DotRight => (Some(128.0), Some(128.0)),
        BarState::EndDot => (Some(128.0), Some(128.0 - R)),
    };
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128"><defs><clipPath id="c"><path d="{shape}"/></clipPath></defs><path d="{shape}" fill="#f0eff5" fill-opacity="0.28"/>"##
    );
    if let Some(x) = fill_to {
        s.push_str(&format!(
            r##"<rect x="0" y="{Y}" width="{x}" height="{H}" fill="{hex}" clip-path="url(#c)"/>"##
        ));
    }
    if let Some(x) = dot_x {
        s.push_str(&format!(
            r##"<circle cx="{x}" cy="64" r="26" fill="{hex}" stroke="#f0eff5" stroke-width="7"/>"##
        ));
    }
    s.push_str("</svg>");
    s
}

/// `#rrggbb`, lowercase, from anything a person might type: with or without `#`, 3 or 6 digits.
pub fn normalize_hex(input: &str) -> Option<String> {
    let s = input.trim().trim_start_matches('#');
    let digits: String = match s.len() {
        6 => s.to_ascii_lowercase(),
        3 => s
            .chars()
            .flat_map(|c| [c, c])
            .collect::<String>()
            .to_ascii_lowercase(),
        _ => return None,
    };
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{digits}"))
}

/// `#rrggbb` as the integer Discord wants for an accent colour.
pub fn hex_to_u32(hex: &str) -> Option<u32> {
    let h = normalize_hex(hex)?;
    u32::from_str_radix(&h[1..], 16).ok()
}

/// Tint one icon and rasterize it.
pub fn render_png(icon: Icon, hex: &str) -> anyhow::Result<Vec<u8>> {
    Ok(render_pixmap(icon, hex)?.encode_png()?)
}

fn render_pixmap(icon: Icon, hex: &str) -> anyhow::Result<resvg::tiny_skia::Pixmap> {
    let hex = normalize_hex(hex).ok_or_else(|| anyhow::anyhow!("bad colour {hex:?}"))?;
    let svg = icon.svg(&hex);
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(SIZE, SIZE)
        .ok_or_else(|| anyhow::anyhow!("pixmap allocation failed"))?;
    let scale = SIZE as f32 / tree.size().width().max(1.0);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

/// The resolved set: which icons exist on Discord, by id, and the colour they were made in, which
/// is also the colour every container takes. Missing icons resolve to their glyph.
#[derive(Debug, Default, Clone)]
pub struct IconSet {
    ids: HashMap<Icon, u64>,
    accent: Option<u32>,
}

impl IconSet {
    /// An otherwise empty set in a colour, for when the upload failed but the colour is chosen.
    pub fn with_accent(mut self, hex: &str) -> Self {
        self.accent = hex_to_u32(hex);
        self
    }

    /// The container accent: the icon colour, or the brand pink before one is chosen.
    pub fn accent(&self) -> u32 {
        self.accent.unwrap_or(accent::BRAND)
    }

    pub fn get(&self, icon: Icon) -> Emoji {
        match self.ids.get(&icon) {
            Some(id) => Emoji::Custom {
                name: icon.name().to_string(),
                id: *id,
            },
            None => Emoji::Unicode(icon.fallback().to_string()),
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn is_complete(&self) -> bool {
        Icon::all().all(|i| self.ids.contains_key(&i))
    }

    #[cfg(test)]
    pub fn insert_for_test(&mut self, icon: Icon, id: u64) {
        self.ids.insert(icon, id);
    }
}

#[derive(Deserialize)]
struct EmojiObject {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct EmojiList {
    items: Vec<EmojiObject>,
}

/// Bring the application's emojis in line with `hex`: keep what exists, upload what is missing,
/// and when `replace` (the colour changed) recreate everything. Stale `cd_*` emojis from an older
/// icon list are removed. Stops at the first rate limit, leaving the rest for the next attempt;
/// what was created stays created, so a retry only has to finish the job.
pub async fn provision(rest: &Rest, hex: &str, replace: bool) -> Result<IconSet, RestError> {
    let hex = normalize_hex(hex).ok_or_else(|| RestError::Status {
        status: 400,
        message: format!("bad colour {hex:?}"),
    })?;
    let app_id = rest_app_id(rest).await?;
    let existing: EmojiList = rest.get(&format!("/applications/{app_id}/emojis")).await?;
    let mut by_name: HashMap<String, String> =
        existing.items.into_iter().map(|e| (e.name, e.id)).collect();
    let mut set = IconSet::default().with_accent(&hex);
    for icon in Icon::all() {
        if let Some(id) = by_name.remove(icon.name()) {
            if replace {
                rest.delete(&format!("/applications/{app_id}/emojis/{id}"))
                    .await?;
            } else {
                if let Ok(id) = id.parse() {
                    set.ids.insert(icon, id);
                }
                continue;
            }
        }
        let png = render_png(icon, &hex).map_err(|e| RestError::Status {
            status: 500,
            message: format!("rendering {}: {e}", icon.name()),
        })?;
        let image = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let created: EmojiObject = rest
            .post(
                &format!("/applications/{app_id}/emojis"),
                &json!({ "name": icon.name(), "image": image }),
            )
            .await?;
        if let Ok(id) = created.id.parse() {
            set.ids.insert(icon, id);
        }
    }
    for (name, id) in by_name {
        if name.starts_with(NAME_PREFIX) {
            let _ = rest
                .delete(&format!("/applications/{app_id}/emojis/{id}"))
                .await;
        }
    }
    Ok(set)
}

#[derive(Deserialize)]
struct AppInfo {
    id: String,
}

async fn rest_app_id(rest: &Rest) -> Result<String, RestError> {
    let app: AppInfo = rest.get("/oauth2/applications/@me").await?;
    Ok(app.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_normalisation() {
        assert_eq!(normalize_hex("#CD00AE").as_deref(), Some("#cd00ae"));
        assert_eq!(normalize_hex(" cd00ae ").as_deref(), Some("#cd00ae"));
        assert_eq!(normalize_hex("#f0a").as_deref(), Some("#ff00aa"));
        assert_eq!(normalize_hex("#cd00a"), None);
        assert_eq!(normalize_hex("#gg00ae"), None);
        assert_eq!(normalize_hex(""), None);
    }

    #[test]
    fn every_icon_renders_to_a_small_png_with_a_valid_name() {
        let mut names = std::collections::HashSet::new();
        for icon in Icon::all() {
            let png = render_png(icon, DEFAULT_HEX).unwrap_or_else(|e| panic!("{icon:?}: {e}"));
            assert!(png.starts_with(b"\x89PNG"), "{icon:?}");
            assert!(png.len() < 64 * 1024, "{icon:?} is {} bytes", png.len());
            let name = icon.name();
            assert!(name.starts_with(NAME_PREFIX) && name.len() <= 32, "{name}");
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{name}"
            );
            assert!(names.insert(name), "duplicate emoji name {name}");
        }
        assert_eq!(names.len(), 41);
    }

    #[test]
    fn tint_reaches_the_pixels() {
        let pixmap = render_pixmap(Icon::Stop, "#ff0000").unwrap();
        // The stop icon is a filled square: the centre pixel is opaque red, a corner is clear.
        let mid = pixmap.pixel(SIZE / 2, SIZE / 2).unwrap();
        assert_eq!(
            (mid.red(), mid.green(), mid.blue(), mid.alpha()),
            (255, 0, 0, 255)
        );
        assert_eq!(pixmap.pixel(1, 1).unwrap().alpha(), 0);
    }

    #[test]
    fn bar_segments_fill_and_dot_where_they_should() {
        let full = render_pixmap(Icon::bar(Cap::Middle, BarState::Full), "#ff0000").unwrap();
        let p = full.pixel(10, 64).unwrap();
        assert_eq!((p.red(), p.green(), p.blue()), (255, 0, 0));
        // Above the bar is clear; the bar runs edge to edge.
        assert_eq!(full.pixel(64, 10).unwrap().alpha(), 0);
        assert!(full.pixel(0, 64).unwrap().alpha() > 0);
        assert!(full.pixel(127, 64).unwrap().alpha() > 0);

        let half = render_pixmap(Icon::bar(Cap::Middle, BarState::HalfDot), "#ff0000").unwrap();
        // Left of the dot is accent, right of it is the translucent track.
        let l = half.pixel(20, 64).unwrap();
        assert_eq!((l.red(), l.green(), l.blue()), (255, 0, 0));
        let r = half.pixel(115, 64).unwrap();
        assert!(r.alpha() > 0 && r.alpha() < 128, "{}", r.alpha());

        // The left cap's rounded end leaves its far corner clear; the middle segment does not.
        let cap = render_pixmap(Icon::bar(Cap::Left, BarState::Empty), "#ff0000").unwrap();
        assert_eq!(cap.pixel(1, 43).unwrap().alpha(), 0);
        let mid = render_pixmap(Icon::bar(Cap::Middle, BarState::Empty), "#ff0000").unwrap();
        assert!(mid.pixel(1, 43).unwrap().alpha() > 0);

        // A split dot: the right half of one segment and the left half of the next meet at the
        // edge, and nothing is filled past it.
        let right = render_pixmap(Icon::bar(Cap::Middle, BarState::DotRight), "#ff0000").unwrap();
        let p = right.pixel(126, 64).unwrap();
        assert_eq!((p.red(), p.green(), p.blue()), (255, 0, 0));
        assert!(right.pixel(110, 64).unwrap().alpha() == 255);
        let left = render_pixmap(Icon::bar(Cap::Middle, BarState::DotLeft), "#ff0000").unwrap();
        let p = left.pixel(1, 64).unwrap();
        assert_eq!((p.red(), p.green(), p.blue()), (255, 0, 0));
        let track = left.pixel(100, 64).unwrap();
        assert!(track.alpha() > 0 && track.alpha() < 128);
        // The start state puts the dot on the rounded end with nothing filled.
        let start = render_pixmap(Icon::bar(Cap::Left, BarState::StartDot), "#ff0000").unwrap();
        let p = start.pixel(22, 64).unwrap();
        assert_eq!((p.red(), p.green(), p.blue()), (255, 0, 0));
        let track = start.pixel(100, 64).unwrap();
        assert!(track.alpha() > 0 && track.alpha() < 128);
    }

    #[test]
    fn states_a_cap_cannot_show_map_to_the_nearest() {
        assert_eq!(Icon::bar(Cap::Left, BarState::DotLeft), Icon::BarL1);
        assert_eq!(Icon::bar(Cap::Middle, BarState::StartDot), Icon::BarM1);
        assert_eq!(Icon::bar(Cap::Right, BarState::DotRight), Icon::BarR4);
        assert_eq!(Icon::bar(Cap::Left, BarState::EndDot), Icon::BarL4);
        assert_eq!(Icon::bar(Cap::Middle, BarState::Full), Icon::BarM3);
    }

    #[test]
    fn bar_fallbacks_spell_the_text_slider() {
        let text: String = [
            Icon::bar(Cap::Left, BarState::Full),
            Icon::bar(Cap::Middle, BarState::DotRight),
            Icon::bar(Cap::Middle, BarState::DotLeft),
            Icon::bar(Cap::Right, BarState::Empty),
        ]
        .iter()
        .map(|i| i.fallback())
        .collect();
        assert_eq!(text, "━●──");
    }

    #[test]
    fn accent_follows_the_hex() {
        assert_eq!(IconSet::default().accent(), accent::BRAND);
        assert_eq!(hex_to_u32("#E67451"), Some(0xE6_74_51));
        assert_eq!(
            IconSet::default().with_accent("#e67451").accent(),
            0xE6_74_51
        );
        assert_eq!(
            IconSet::default().with_accent("nope").accent(),
            accent::BRAND
        );
    }

    #[test]
    fn missing_icons_fall_back_to_glyphs() {
        let set = IconSet::default();
        assert!(!set.is_complete());
        assert_eq!(set.get(Icon::Play), Emoji::Unicode("▶".into()));
        let mut set = IconSet::default();
        set.insert_for_test(Icon::Play, 7);
        assert_eq!(
            set.get(Icon::Play),
            Emoji::Custom {
                name: "cd_play".into(),
                id: 7
            }
        );
        assert_eq!(set.get(Icon::Play).markup(), "<:cd_play:7>");
    }
}
