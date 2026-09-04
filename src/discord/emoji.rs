//! The bot's own icon set, as Discord **application emojis**.
//!
//! Default Unicode symbols are what made the first controller look cheap: `⏯` and `🔀` render in
//! whatever emoji font the viewer has, at whatever colour that font chose. An application can own
//! up to 2000 emojis of its own that work in every server it is in without using anyone's emoji
//! slots, so the bot uploads a matching set once and uses those everywhere: headers, buttons and
//! list markers.
//!
//! The artwork is [Phosphor](https://phosphoricons.com) (MIT; vendored under `assets/phosphor/`),
//! the `fill` weight, tinted in one colour and rendered to 128 px PNGs with resvg at startup. The
//! colour is a per-bot setting (`emoji_hex`, default the Chordia pink); changing it regenerates and
//! re-uploads the whole set, so a bot can match its server's palette.
//!
//! Emojis are named `cd_<icon>`. On every boot the set is listed (one request); uploads only happen
//! when something is missing or the colour changed, and the last applied colour is remembered so a
//! restart never re-uploads. If Discord refuses (rate limit, outage), views fall back to the
//! Unicode glyphs and nothing else changes.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use serde_json::json;
use serenity::all::{EmojiId, Http};

use crate::discord::identity::Identity;
use crate::discord::ui::fmt::glyph;
use crate::discord::ui::v2::Emoji;

/// The Chordia accent, the same pink the web app's `--primary` resolves to.
pub const DEFAULT_HEX: &str = "#cd00ae";
/// Discord recommends 128×128 for custom emoji and caps the file at 256 KiB; a tinted glyph is a
/// few kilobytes.
const SIZE: u32 = 128;
const NAME_PREFIX: &str = "cd_";

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
}

macro_rules! icons {
    ($( $variant:ident => $name:literal, $file:literal, $fallback:expr ; )*) => {
        impl Icon {
            pub const ALL: &'static [Icon] = &[$(Icon::$variant),*];

            /// The emoji's name on Discord.
            pub fn name(self) -> &'static str {
                match self { $(Icon::$variant => concat!("cd_", $name),)* }
            }

            /// The Unicode glyph used when the emoji is not (yet) available.
            pub fn fallback(self) -> &'static str {
                match self { $(Icon::$variant => $fallback,)* }
            }

            fn svg(self) -> &'static str {
                match self {
                    $(Icon::$variant => include_str!(concat!("../../assets/phosphor/", $file, ".svg")),)*
                }
            }
        }
    };
}

icons! {
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

/// Tint one Phosphor icon and rasterize it.
pub fn render_png(icon: Icon, hex: &str) -> anyhow::Result<Vec<u8>> {
    Ok(render_pixmap(icon, hex)?.encode_png()?)
}

fn render_pixmap(icon: Icon, hex: &str) -> anyhow::Result<resvg::tiny_skia::Pixmap> {
    let hex = normalize_hex(hex).ok_or_else(|| anyhow::anyhow!("bad colour {hex:?}"))?;
    let src = icon.svg();
    let svg = if src.contains("fill=\"currentColor\"") {
        src.replace("fill=\"currentColor\"", &format!("fill=\"{hex}\""))
    } else {
        src.replacen("<svg", &format!("<svg fill=\"{hex}\""), 1)
    };
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

/// The resolved set: which icons exist on Discord, by id. Missing ones resolve to their glyph.
#[derive(Debug, Default, Clone)]
pub struct IconSet {
    ids: HashMap<Icon, u64>,
}

impl IconSet {
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
        Icon::ALL.iter().all(|i| self.ids.contains_key(i))
    }

    #[cfg(test)]
    pub fn insert_for_test(&mut self, icon: Icon, id: u64) {
        self.ids.insert(icon, id);
    }
}

/// Bring the application's emojis in line with `hex`: keep what exists, upload what is missing,
/// and when `replace` (the colour changed) recreate everything. Stale `cd_*` emojis from an older
/// icon list are removed.
pub async fn provision(http: &Http, hex: &str, replace: bool) -> anyhow::Result<IconSet> {
    let hex = normalize_hex(hex).ok_or_else(|| anyhow::anyhow!("bad colour {hex:?}"))?;
    let existing = http.get_application_emojis().await?;
    let mut by_name: HashMap<String, EmojiId> =
        existing.into_iter().map(|e| (e.name, e.id)).collect();
    let mut set = IconSet::default();
    for icon in Icon::ALL {
        if let Some(id) = by_name.remove(icon.name()) {
            if replace {
                http.delete_application_emoji(id).await?;
            } else {
                set.ids.insert(*icon, id.get());
                continue;
            }
        }
        let png = render_png(*icon, &hex)?;
        let image = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let created = http
            .create_application_emoji(&json!({ "name": icon.name(), "image": image }))
            .await?;
        set.ids.insert(*icon, created.id.get());
    }
    for (name, id) in by_name {
        if name.starts_with(NAME_PREFIX) {
            let _ = http.delete_application_emoji(id).await;
        }
    }
    Ok(set)
}

/// On Ready: make the set match the bot's settings and install it on the identity. Failure leaves
/// the Unicode fallbacks in place and is logged, never fatal.
pub async fn ensure(identity: &Identity, http: &Http) {
    let mut settings = identity.settings();
    let hex = settings
        .emoji_hex
        .clone()
        .and_then(|h| normalize_hex(&h))
        .unwrap_or_else(|| DEFAULT_HEX.to_string());
    let replace = settings.emoji_hex_applied.as_deref() != Some(hex.as_str());
    match provision(http, &hex, replace).await {
        Ok(set) => {
            tracing::info!(bot = identity.index, icons = set.len(), colour = %hex, replaced = replace, "application emojis ready");
            identity.set_icons(Arc::new(set));
            if replace {
                settings.emoji_hex_applied = Some(hex);
                identity.set_settings(settings);
                identity.save_settings().await;
            }
        }
        Err(e) => {
            tracing::warn!(bot = identity.index, error = %e, "application emojis unavailable; using text glyphs");
        }
    }
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
    fn every_icon_renders_to_a_small_png() {
        for icon in Icon::ALL {
            let png = render_png(*icon, DEFAULT_HEX).unwrap_or_else(|e| panic!("{icon:?}: {e}"));
            assert!(png.starts_with(b"\x89PNG"), "{icon:?}");
            assert!(png.len() < 64 * 1024, "{icon:?} is {} bytes", png.len());
            assert!(icon.name().starts_with(NAME_PREFIX));
            assert!(icon.name().len() <= 32);
        }
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
    fn missing_icons_fall_back_to_glyphs() {
        let set = IconSet::default();
        assert!(!set.is_complete());
        assert_eq!(set.get(Icon::Play), Emoji::Unicode("▶".into()));
        let mut set = IconSet::default();
        set.ids.insert(Icon::Play, 7);
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
