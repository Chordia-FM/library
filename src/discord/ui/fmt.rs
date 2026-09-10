//! Text formatting shared by every view: durations, the progress bar, quality badges, the glyph
//! table, markdown escaping and the presence template. One vocabulary, so every command reads as
//! one product.

use crate::discord::source::TrackFacts;

/// Unicode glyphs used in headers and buttons. No custom emoji: those are per-guild and the bot
/// serves many.
pub mod glyph {
    pub const PLAY: &str = "▶";
    pub const PAUSE: &str = "⏸";
    pub const PLAY_PAUSE: &str = "⏯";
    pub const NEXT: &str = "⏭";
    pub const PREV: &str = "⏮";
    pub const STOP: &str = "⏹";
    pub const SHUFFLE: &str = "🔀";
    pub const LOOP_QUEUE: &str = "🔁";
    pub const LOOP_TRACK: &str = "🔂";
    pub const VOLUME: &str = "🔊";
    pub const VOLUME_DOWN: &str = "🔉";
    pub const RADIO: &str = "📻";
    pub const NOTE: &str = "♪";
    pub const QUEUE: &str = "📜";
    pub const SEARCH: &str = "🔍";
    pub const WARNING: &str = "⚠";
    pub const CROSS: &str = "✖";
    pub const CHECK: &str = "✔";
    pub const LYRICS: &str = "🎤";
    pub const INFO: &str = "ℹ";
    pub const WAVE: &str = "👋";
    pub const GEAR: &str = "⚙";
}

/// Container accent colours. The brand colour is the one the app's own icons are drawn in
/// (`--primary` as sRGB, see `frontend/scripts/generate-brand-assets.ts`); the rest are Discord's
/// own semantic palette so a red error looks like every other bot's red error.
pub mod accent {
    pub const BRAND: u32 = 0xF2_25_8C;
    pub const PAUSED: u32 = 0x6B_72_80;
    pub const ERROR: u32 = 0xED_42_45;
    pub const NOTICE: u32 = 0xFE_E7_5C;
}

/// `3:45`, or `1:02:03` past an hour.
pub fn duration(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Which of `cells` segments are filled and where the playhead sits, for a bar of emojis (or their
/// text fallbacks). Half-cell resolution. The playhead is a dot: on the rounded start before
/// anything has played, in the middle of a half-filled segment, or split across the edge between
/// a filled segment and the empty one after it, so nothing ever looks filled past it. At the very
/// end it sits on the rounded end.
pub fn progress_cells(position_ms: u64, duration_ms: u64, cells: usize) -> Vec<Segment> {
    let cells = cells.max(2);
    let last = cells - 1;
    if duration_ms > 0 && position_ms >= duration_ms {
        let mut v = vec![Segment::Full; cells];
        v[last] = Segment::EndDot;
        return v;
    }
    let x = if duration_ms == 0 {
        0.0
    } else {
        (position_ms as f64 / duration_ms as f64) * cells as f64
    };
    let k = (x.floor() as usize).min(last);
    let frac = x - k as f64;
    (0..cells)
        .map(|i| {
            use std::cmp::Ordering::*;
            match i.cmp(&k) {
                Less => Segment::Full,
                Equal if k == 0 && frac < 0.25 => Segment::StartDot,
                Equal if frac < 0.5 => Segment::HalfDot,
                Equal if k == last => Segment::EndDot,
                Equal => Segment::DotRight,
                Greater if i == k + 1 && frac >= 0.5 => Segment::DotLeft,
                Greater => Segment::Empty,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    Empty,
    StartDot,
    DotLeft,
    HalfDot,
    Full,
    DotRight,
    EndDot,
}

/// The fixed quality vocabulary: codec, rate/depth, lossless/spatial, the negotiated Opus bitrate,
/// ReplayGain. Always in this order, at most five.
pub fn badges(f: &TrackFacts) -> Vec<String> {
    let mut out = Vec::with_capacity(5);
    out.push(codec_label(&f.codec).to_string());
    if f.sample_rate_hz > 0 {
        let rate = sample_rate(f.sample_rate_hz);
        if f.bit_depth > 0 && f.lossless {
            out.push(format!("{rate} · {}-bit", f.bit_depth));
        } else {
            out.push(rate);
        }
    }
    if f.spatial {
        out.push("Atmos".into());
    } else if f.lossless {
        out.push("Lossless".into());
    }
    if let Some(kbps) = f.opus_kbps {
        out.push(format!("Opus {kbps}k"));
    }
    if let Some(g) = f.gain_db {
        out.push(format!("RG {}", gain(g)));
    }
    out.truncate(5);
    out
}

/// `44.1 kHz`, `48 kHz`; empty for an unknown rate.
pub fn sample_rate(hz: u32) -> String {
    if hz == 0 {
        return String::new();
    }
    let khz = hz as f64 / 1000.0;
    if khz.fract() == 0.0 {
        format!("{khz:.0} kHz")
    } else {
        format!("{khz:.1} kHz")
    }
}

/// `−7.1 dB` (a real minus sign), `+1.0 dB`.
pub fn gain(db: f32) -> String {
    format!("{}{:.1} dB", if db < 0.0 { "−" } else { "+" }, db.abs())
}

/// Badges as inline code chips: `` `FLAC` `44.1 kHz · 16-bit` `Lossless` ``.
pub fn badge_line(f: &TrackFacts) -> String {
    badges(f)
        .into_iter()
        .map(|b| format!("`{b}`"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn codec_label(codec: &str) -> &str {
    match codec {
        "flac" => "FLAC",
        "mp3" => "MP3",
        "aac" => "AAC",
        "alac" => "ALAC",
        "vorbis" => "Vorbis",
        "opus" => "Opus",
        "pcm" | "wav" => "PCM",
        "" | "unknown" => "Audio",
        other => other,
    }
}

/// Escape Discord markdown in user data (a track title is user data). Without this a title like
/// `F**K` bolds, and `# 1` becomes a heading.
pub fn escape_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(
            c,
            '*' | '_' | '~' | '`' | '|' | '>' | '#' | '\\' | '[' | ']'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Truncate to `max` characters with an ellipsis, on a character boundary.
pub fn ellipsize(s: &str, max: usize) -> String {
    super::v2::clip(s.to_string(), max)
}

/// `{title}`, `{artist}`, … substitution for the presence template. Unknown variables are left in
/// place so a typo is visible rather than silently blank.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Percent-encode a URL query value; RFC 3986 unreserved characters pass through.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `1 track` / `12 tracks`.
pub fn count(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> TrackFacts {
        TrackFacts {
            codec: "flac".into(),
            sample_rate_hz: 44100,
            bit_depth: 16,
            channels: 2,
            lossless: true,
            spatial: false,
            opus_kbps: Some(128),
            gain_db: Some(-6.2),
            peak: Some(0.98),
        }
    }

    #[test]
    fn durations() {
        assert_eq!(duration(0), "0:00");
        assert_eq!(duration(225_000), "3:45");
        assert_eq!(duration(3_723_000), "1:02:03");
    }

    #[test]
    fn progress_cells_fill_left_to_right_with_a_playhead() {
        use Segment::*;
        assert_eq!(
            progress_cells(0, 100, 4),
            vec![StartDot, Empty, Empty, Empty]
        );
        assert_eq!(
            progress_cells(10, 100, 4),
            vec![HalfDot, Empty, Empty, Empty]
        );
        assert_eq!(
            progress_cells(20, 100, 4),
            vec![DotRight, DotLeft, Empty, Empty]
        );
        assert_eq!(progress_cells(50, 100, 4), vec![Full, Full, HalfDot, Empty]);
        assert_eq!(
            progress_cells(40, 100, 4),
            vec![Full, DotRight, DotLeft, Empty]
        );
        assert_eq!(progress_cells(90, 100, 4), vec![Full, Full, Full, EndDot]);
        assert_eq!(progress_cells(100, 100, 4), vec![Full, Full, Full, EndDot]);
        assert_eq!(progress_cells(500, 100, 4), vec![Full, Full, Full, EndDot]);
        assert_eq!(progress_cells(5, 0, 4), vec![StartDot, Empty, Empty, Empty]);
        assert_eq!(progress_cells(0, 1, 1).len(), 2);
    }

    #[test]
    fn badge_vocabulary_and_order() {
        assert_eq!(
            badges(&facts()),
            vec![
                "FLAC",
                "44.1 kHz · 16-bit",
                "Lossless",
                "Opus 128k",
                "RG −6.2 dB"
            ]
        );
        let mut f = facts();
        f.codec = "mp3".into();
        f.lossless = false;
        f.bit_depth = 0;
        f.sample_rate_hz = 48000;
        f.gain_db = Some(1.0);
        f.opus_kbps = None;
        assert_eq!(badges(&f), vec!["MP3", "48 kHz", "RG +1.0 dB"]);
        f.spatial = true;
        assert!(badges(&f).contains(&"Atmos".to_string()));
    }

    #[test]
    fn url_encoding() {
        assert_eq!(urlencode("One More Time"), "One%20More%20Time");
        assert_eq!(urlencode("J's & #1 ü"), "J%27s%20%26%20%231%20%C3%BC");
    }

    #[test]
    fn markdown_is_escaped() {
        assert_eq!(escape_md("F**K # [x]"), "F\\*\\*K \\# \\[x\\]");
    }

    #[test]
    fn template_substitution() {
        assert_eq!(
            render_template(
                "{title} · {artist} ({nope})",
                &[("title", "A"), ("artist", "B")]
            ),
            "A · B ({nope})"
        );
    }
}
