//! The bot's equalizer: ten peaking bands on the standard ISO centres, the same model as the web
//! client's, applied to the PCM ffmpeg hands over. The filters live in the audio reader and read
//! their settings through a shared handle, so a change is heard at once, no restart.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chordia_contracts::user::{EqBand, EqConfig};

/// The band centres, in Hz; `EqConfig::default()` carries the same ones.
pub const FREQS: [f32; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
const DEFAULT_Q: f32 = 1.4;
/// The most a band (or the preamp) may be pushed, in dB.
pub const MAX_DB: f32 = 15.0;
/// The mixer's rate: what the ffmpeg path decodes to.
const SAMPLE_RATE: f32 = 48_000.0;

/// A built-in preset: the web client's list, so a name means the same thing everywhere.
pub struct Preset {
    pub name: &'static str,
    pub preamp: f32,
    pub gains: [f32; 10],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Flat",
        preamp: 0.0,
        gains: [0.0; 10],
    },
    Preset {
        name: "Bass Boost",
        preamp: -2.0,
        gains: [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "Bass Reducer",
        preamp: 0.0,
        gains: [-6.0, -5.0, -4.0, -2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "Treble Boost",
        preamp: -2.0,
        gains: [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0],
    },
    Preset {
        name: "Treble Reducer",
        preamp: 0.0,
        gains: [0.0, 0.0, 0.0, 0.0, 0.0, -1.0, -2.0, -4.0, -5.0, -6.0],
    },
    Preset {
        name: "Vocal",
        preamp: -1.0,
        gains: [-2.0, -1.0, 0.0, 2.0, 4.0, 4.0, 3.0, 1.0, 0.0, -1.0],
    },
    Preset {
        name: "Rock",
        preamp: -2.0,
        gains: [5.0, 4.0, 3.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 4.0],
    },
    Preset {
        name: "Pop",
        preamp: -1.0,
        gains: [-1.0, 0.0, 2.0, 4.0, 4.0, 3.0, 1.0, 0.0, -1.0, -2.0],
    },
    Preset {
        name: "Jazz",
        preamp: -1.0,
        gains: [4.0, 3.0, 1.0, 2.0, -1.0, -1.0, 0.0, 1.0, 3.0, 4.0],
    },
    Preset {
        name: "Classical",
        preamp: -1.0,
        gains: [4.0, 3.0, 2.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 4.0],
    },
    Preset {
        name: "Electronic",
        preamp: -2.0,
        gains: [5.0, 4.0, 1.0, 0.0, -2.0, 2.0, 1.0, 1.0, 4.0, 5.0],
    },
    Preset {
        name: "Hip-Hop",
        preamp: -2.0,
        gains: [6.0, 5.0, 3.0, 2.0, -1.0, -1.0, 1.0, 2.0, 2.0, 3.0],
    },
    Preset {
        name: "Acoustic",
        preamp: -1.0,
        gains: [4.0, 4.0, 3.0, 1.0, 2.0, 2.0, 3.0, 3.0, 2.0, 2.0],
    },
    Preset {
        name: "Loudness",
        preamp: -3.0,
        gains: [6.0, 4.0, 0.0, 0.0, -2.0, 0.0, 0.0, 3.0, 6.0, 6.0],
    },
];

pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

/// The preset as a live configuration.
pub fn preset_config(p: &Preset) -> EqConfig {
    EqConfig {
        enabled: true,
        preamp: p.preamp,
        bands: FREQS
            .iter()
            .zip(p.gains.iter())
            .map(|(freq, gain)| EqBand {
                freq: *freq,
                gain: *gain,
                q: DEFAULT_Q,
            })
            .collect(),
    }
}

/// Which preset the configuration is, if it is one exactly.
pub fn preset_of(cfg: &EqConfig) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| {
        (cfg.preamp - p.preamp).abs() < 0.05
            && cfg.bands.len() == 10
            && cfg
                .bands
                .iter()
                .zip(p.gains.iter())
                .all(|(b, g)| (b.gain - g).abs() < 0.05)
    })
}

/// The configuration in a word: off, a preset's name, or custom.
pub fn label(cfg: &EqConfig) -> String {
    if !cfg.enabled {
        return "off".to_string();
    }
    match preset_of(cfg) {
        Some(p) => p.name.to_string(),
        None => "custom".to_string(),
    }
}

/// Whether the configuration changes anything at all: on, and not flat.
pub fn active(cfg: &EqConfig) -> bool {
    cfg.enabled && (cfg.preamp.abs() > 0.01 || cfg.bands.iter().any(|b| b.gain.abs() > 0.01))
}

/// A configuration kept to what the filters can do: ten bands on the standard centres, gains
/// and preamp within [`MAX_DB`], a sane Q.
pub fn tidy(cfg: &EqConfig) -> EqConfig {
    let mut bands: Vec<EqBand> = FREQS
        .iter()
        .enumerate()
        .map(|(i, freq)| {
            let given = cfg.bands.get(i);
            EqBand {
                freq: *freq,
                gain: given.map(|b| b.gain).unwrap_or(0.0).clamp(-MAX_DB, MAX_DB),
                q: given.map(|b| b.q).unwrap_or(DEFAULT_Q).clamp(0.2, 10.0),
            }
        })
        .collect();
    for b in &mut bands {
        b.gain = (b.gain * 2.0).round() / 2.0;
    }
    EqConfig {
        enabled: cfg.enabled,
        preamp: ((cfg.preamp.clamp(-MAX_DB, MAX_DB)) * 2.0).round() / 2.0,
        bands,
    }
}

/// A gain as text: `+3`, `0`, `-2.5`.
pub fn gain_text(g: f32) -> String {
    if g.abs() < 0.05 {
        "0".to_string()
    } else if (g - g.round()).abs() < 0.05 {
        format!("{:+}", g.round() as i32)
    } else {
        format!("{g:+.1}")
    }
}

/// A band centre as text: `31`, `1k`.
pub fn freq_text(f: f32) -> String {
    if f >= 1000.0 {
        format!("{}k", (f / 1000.0) as u32)
    } else {
        format!("{}", f as u32)
    }
}

/// Every band's gain in one line: `31 +6 · 62 +4 · …`.
pub fn summary(cfg: &EqConfig) -> String {
    cfg.bands
        .iter()
        .map(|b| format!("{} {}", freq_text(b.freq), gain_text(b.gain)))
        .collect::<Vec<_>>()
        .join(" · ")
}

// ---- the shared handle and the filters ---------------------------------------------------------------

/// What the player and the audio reader share: the configuration and a stamp that moves with
/// every change, so the reader picks up new coefficients between two buffers.
pub struct Shared {
    config: Mutex<EqConfig>,
    version: AtomicU64,
}

impl Shared {
    pub fn new(cfg: EqConfig) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(cfg),
            version: AtomicU64::new(1),
        })
    }

    pub fn get(&self) -> EqConfig {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set(&self, cfg: EqConfig) {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
        self.version.fetch_add(1, Ordering::Release);
    }

    fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// One RBJ peaking biquad, direct form I, with its state for two channels.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: [f32; 2],
    x2: [f32; 2],
    y1: [f32; 2],
    y2: [f32; 2],
}

impl Biquad {
    fn peaking(f0: f32, gain_db: f32, q: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / SAMPLE_RATE;
        let alpha = w0.sin() / (2.0 * q);
        let cw = w0.cos();
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cw) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cw) / a0,
            a2: (1.0 - alpha / a) / a0,
            x1: [0.0; 2],
            x2: [0.0; 2],
            y1: [0.0; 2],
            y2: [0.0; 2],
        }
    }

    #[inline]
    fn run(&mut self, ch: usize, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1[ch] + self.b2 * self.x2[ch]
            - self.a1 * self.y1[ch]
            - self.a2 * self.y2[ch];
        self.x2[ch] = self.x1[ch];
        self.x1[ch] = x;
        self.y2[ch] = self.y1[ch];
        self.y1[ch] = y;
        y
    }
}

/// Interleaved stereo f32 PCM in, the same out through the equalizer. Coefficients follow the
/// shared configuration; a flat or disabled one passes samples through untouched.
pub struct Reader<R: Read> {
    inner: R,
    shared: Arc<Shared>,
    seen: u64,
    filters: Vec<Biquad>,
    preamp: f32,
    bypass: bool,
    /// Which channel the next sample belongs to.
    channel: usize,
    /// Bytes read that do not yet make a whole sample.
    tail: Vec<u8>,
    /// Filtered bytes not yet handed over.
    ready: Vec<u8>,
    ready_at: usize,
}

impl<R: Read> Reader<R> {
    pub fn new(inner: R, shared: Arc<Shared>) -> Self {
        let mut r = Self {
            inner,
            shared,
            seen: 0,
            filters: Vec::new(),
            preamp: 1.0,
            bypass: true,
            channel: 0,
            tail: Vec::new(),
            ready: Vec::new(),
            ready_at: 0,
        };
        r.refresh();
        r
    }

    fn refresh(&mut self) {
        let version = self.shared.version();
        if version == self.seen {
            return;
        }
        self.seen = version;
        let cfg = self.shared.get();
        self.bypass = !active(&cfg);
        self.preamp = 10f32.powf(cfg.preamp / 20.0);
        // Keep the filter states where they are for bands that stay, so a nudge does not click.
        let mut next: Vec<Biquad> = cfg
            .bands
            .iter()
            .map(|b| Biquad::peaking(b.freq, b.gain, b.q))
            .collect();
        for (n, old) in next.iter_mut().zip(self.filters.iter()) {
            n.x1 = old.x1;
            n.x2 = old.x2;
            n.y1 = old.y1;
            n.y2 = old.y2;
        }
        self.filters = next;
    }

    fn process(&mut self, bytes: &mut [u8]) {
        self.refresh();
        if self.bypass {
            return;
        }
        for chunk in bytes.chunks_exact_mut(4) {
            let mut x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * self.preamp;
            let ch = self.channel;
            for f in &mut self.filters {
                x = f.run(ch, x);
            }
            chunk.copy_from_slice(&x.to_le_bytes());
            self.channel ^= 1;
        }
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.ready_at >= self.ready.len() {
            // Pull a chunk, keep what is not a whole sample for next time, filter the rest.
            let mut chunk = std::mem::take(&mut self.tail);
            let start = chunk.len();
            chunk.resize(start + buf.len().max(4096), 0);
            let n = self.inner.read(&mut chunk[start..])?;
            if n == 0 {
                return Ok(0);
            }
            chunk.truncate(start + n);
            let whole = chunk.len() / 4 * 4;
            self.tail = chunk.split_off(whole);
            if chunk.is_empty() {
                // A read shorter than one sample: ask again.
                return self.read(buf);
            }
            self.process(&mut chunk);
            self.ready = chunk;
            self.ready_at = 0;
        }
        let n = (self.ready.len() - self.ready_at).min(buf.len());
        buf[..n].copy_from_slice(&self.ready[self.ready_at..self.ready_at + n]);
        self.ready_at += n;
        Ok(n)
    }
}

// ---- the picture ---------------------------------------------------------------------------------------

/// The combined response in dB at `f` Hz (analytic, the same sum the web client draws).
fn response_db(cfg: &EqConfig, f: f32) -> f32 {
    let mut db = cfg.preamp;
    for b in &cfg.bands {
        let a = 10f32.powf(b.gain / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * b.freq / SAMPLE_RATE;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * b.q.max(0.05));
        let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a);
        let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a);
        let w = 2.0 * std::f32::consts::PI * f / SAMPLE_RATE;
        let (cos_w, sin_w, cos_2w, sin_2w) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num_re = b0 + b1 * cos_w + b2 * cos_2w;
        let num_im = -(b1 * sin_w + b2 * sin_2w);
        let den_re = a0 + a1 * cos_w + a2 * cos_2w;
        let den_im = -(a1 * sin_w + a2 * sin_2w);
        db += 20.0 * (num_re.hypot(num_im) / den_re.hypot(den_im)).log10();
    }
    db
}

const W: f32 = 900.0;
const H: f32 = 360.0;
/// Left, right, top, bottom: room for the dB axis, the title and the band labels.
const PAD: (f32, f32, f32, f32) = (64.0, 28.0, 52.0, 52.0);
const RANGE: f32 = 15.0;
const OFF_HEX: &str = "#8a8f98";

/// The fonts the picture's labels are set in: the bundled Manrope (the web client's face), with
/// whatever the host has for anything it lacks.
static FONTS: std::sync::LazyLock<Arc<resvg::usvg::fontdb::Database>> =
    std::sync::LazyLock::new(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_font_data(include_bytes!("../../assets/fonts/Manrope.ttf").to_vec());
        db.load_system_fonts();
        Arc::new(db)
    });

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The response curve as an SVG in the bot's colour (grey while off): the grid with its dB
/// marks, the curve over a soft fill, a dot per band with its centre under it and its gain
/// beside it, and the preset's name in the corner.
pub fn svg(cfg: &EqConfig, accent_hex: &str) -> String {
    let (l, r, t, b) = PAD;
    let plot_w = W - l - r;
    let plot_h = H - t - b;
    let lf = 20f32.log10();
    let hf = 20_000f32.log10();
    let x_of = |f: f32| l + (f.log10() - lf) / (hf - lf) * plot_w;
    let y_of = |g: f32| t + (1.0 - (g.clamp(-RANGE, RANGE) + RANGE) / (2.0 * RANGE)) * plot_h;
    let colour = if cfg.enabled { accent_hex } else { OFF_HEX };
    let font = r##"font-family="Manrope, sans-serif""##;
    let mut out = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}"><defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{colour}" stop-opacity="0.45"/><stop offset="1" stop-color="{colour}" stop-opacity="0.02"/></linearGradient></defs><rect width="{W}" height="{H}" rx="18" fill="#1e1f22"/>"##
    );
    for f in [
        50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0,
    ] {
        let x = x_of(f);
        out.push_str(&format!(
            r##"<line x1="{x:.1}" y1="{t}" x2="{x:.1}" y2="{:.1}" stroke="#ffffff" stroke-opacity="0.07"/>"##,
            t + plot_h
        ));
    }
    for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
        let y = y_of(db);
        let opacity = if db == 0.0 { "0.3" } else { "0.08" };
        out.push_str(&format!(
            r##"<line x1="{l}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="#ffffff" stroke-opacity="{opacity}"/><text x="{:.1}" y="{:.1}" {font} font-size="14" fill="#949ba4" text-anchor="end">{}</text>"##,
            W - r,
            l - 10.0,
            y + 5.0,
            gain_text(db)
        ));
    }
    let points = 240;
    let mut line = String::new();
    for i in 0..points {
        let f = 10f32.powf(lf + (i as f32 / (points - 1) as f32) * (hf - lf));
        let x = l + (i as f32 / (points - 1) as f32) * plot_w;
        let y = y_of(response_db(cfg, f));
        line.push_str(&format!("{}{x:.1} {y:.1} ", if i == 0 { "M" } else { "L" }));
    }
    let area = format!(
        "{line}L{:.1} {:.1} L{l} {:.1} Z",
        l + plot_w,
        t + plot_h,
        t + plot_h
    );
    out.push_str(&format!(r##"<path d="{area}" fill="url(#g)"/>"##));
    out.push_str(&format!(
        r##"<path d="{line}" fill="none" stroke="{colour}" stroke-width="4" stroke-linejoin="round" stroke-linecap="round"/>"##
    ));
    for band in &cfg.bands {
        let x = x_of(band.freq);
        let y = y_of(band.gain);
        // The gain sits above a boost and below a cut, so it never crosses the curve.
        let label_y = if band.gain >= 0.0 { y - 16.0 } else { y + 28.0 };
        out.push_str(&format!(
            r##"<circle cx="{x:.1}" cy="{y:.1}" r="7" fill="{colour}" stroke="#ffffff" stroke-width="2"/><text x="{x:.1}" y="{label_y:.1}" {font} font-size="17" font-weight="600" fill="#ffffff" text-anchor="middle">{}</text><text x="{x:.1}" y="{:.1}" {font} font-size="16" fill="#b5bac1" text-anchor="middle">{}</text>"##,
            gain_text(band.gain),
            H - 18.0,
            freq_text(band.freq)
        ));
    }
    let title = format!(
        "{} · {}",
        label(cfg),
        if cfg.enabled { "on" } else { "off" }
    );
    out.push_str(&format!(
        r##"<text x="{l}" y="34" {font} font-size="20" font-weight="700" fill="#ffffff" fill-opacity="0.9">{}</text>"##,
        esc(&title)
    ));
    if cfg.preamp.abs() > 0.05 {
        out.push_str(&format!(
            r##"<text x="{:.1}" y="34" {font} font-size="16" fill="#b5bac1" text-anchor="end">preamp {} dB</text>"##,
            W - r,
            gain_text(cfg.preamp)
        ));
    }
    out.push_str("</svg>");
    out
}

/// The curve as a PNG, for a media gallery.
pub fn picture(cfg: &EqConfig, accent_hex: &str) -> anyhow::Result<Vec<u8>> {
    let options = resvg::usvg::Options {
        fontdb: FONTS.clone(),
        font_family: "Manrope".to_string(),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(&svg(cfg, accent_hex), &options)?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(W as u32, H as u32)
        .ok_or_else(|| anyhow::anyhow!("pixmap allocation failed"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

/// Pictures already drawn, by everything that shapes one: the gains, the preamp, the switch and
/// the colour. The same settings in the same colour hand back the same bytes.
static PICTURES: Mutex<Vec<(String, Arc<Vec<u8>>)>> = Mutex::new(Vec::new());
const PICTURES_KEPT: usize = 24;

fn picture_key(cfg: &EqConfig, accent_hex: &str) -> String {
    let mut key = format!("{accent_hex}|{}|{:.1}", cfg.enabled, cfg.preamp);
    for b in &cfg.bands {
        key.push_str(&format!("|{:.0}:{:.1}:{:.2}", b.freq, b.gain, b.q));
    }
    key
}

/// The picture for these settings, drawn once and remembered; `None` when drawing failed.
pub fn picture_cached(cfg: &EqConfig, accent_hex: &str) -> Option<Arc<Vec<u8>>> {
    let key = picture_key(cfg, accent_hex);
    {
        let mut kept = PICTURES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = kept.iter().position(|(k, _)| *k == key) {
            let hit = kept.remove(i);
            let png = hit.1.clone();
            kept.push(hit);
            return Some(png);
        }
    }
    let png = match picture(cfg, accent_hex) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            tracing::warn!(error = %e, "drawing the equalizer");
            return None;
        }
    };
    let mut kept = PICTURES.lock().unwrap_or_else(|e| e.into_inner());
    kept.push((key, png.clone()));
    if kept.len() > PICTURES_KEPT {
        kept.remove(0);
    }
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip_and_flat_is_off_in_effect() {
        let rock = preset("rock").unwrap();
        let cfg = preset_config(rock);
        assert_eq!(preset_of(&cfg).unwrap().name, "Rock");
        assert_eq!(label(&cfg), "Rock");
        assert!(active(&cfg));
        let flat = preset_config(preset("Flat").unwrap());
        assert!(!active(&flat));
        assert_eq!(label(&flat), "Flat");
        let mut custom = cfg.clone();
        custom.bands[3].gain = 9.0;
        assert_eq!(label(&custom), "custom");
        custom.enabled = false;
        assert_eq!(label(&custom), "off");
        assert!(!active(&custom));
    }

    #[test]
    fn tidy_keeps_the_ten_centres_and_the_range() {
        let mut cfg = EqConfig::default();
        cfg.bands.truncate(3);
        cfg.bands[0].gain = 40.0;
        cfg.preamp = -99.0;
        let t = tidy(&cfg);
        assert_eq!(t.bands.len(), 10);
        assert_eq!(t.bands[0].gain, MAX_DB);
        assert_eq!(t.bands[9].freq, 16000.0);
        assert_eq!(t.preamp, -MAX_DB);
    }

    #[test]
    fn a_flat_reader_passes_bytes_through_and_a_boost_changes_them() {
        let samples: Vec<f32> = (0..2000)
            .map(|i| ((i / 2) as f32 * 0.05).sin() * 0.5)
            .collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let shared = Shared::new(EqConfig::default());
        let mut out = Vec::new();
        Reader::new(std::io::Cursor::new(bytes.clone()), shared.clone())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, bytes);
        // A bass boost on a low tone: louder, and still the same length.
        let mut boosted = preset_config(preset("Bass Boost").unwrap());
        boosted.preamp = 0.0;
        shared.set(boosted);
        let mut out = Vec::new();
        let mut reader = Reader::new(std::io::Cursor::new(bytes.clone()), shared);
        // Odd read sizes, so samples straddle the reads.
        let mut buf = vec![0u8; 1001];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out.len(), bytes.len());
        let energy = |b: &[u8]| -> f32 {
            b.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]).powi(2))
                .sum::<f32>()
        };
        assert!(energy(&out[4000..]) > energy(&bytes[4000..]) * 1.2);
    }

    #[test]
    fn the_picture_is_a_png_with_its_labels_and_is_drawn_once() {
        let cfg = preset_config(preset("Loudness").unwrap());
        let png = picture(&cfg, "#f2258c").unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        // The labels are in the drawing: every centre, the gains, the preset, the preamp.
        let drawing = svg(&cfg, "#f2258c");
        for want in [
            ">31<",
            ">16k<",
            ">+6<",
            ">-2<",
            "Loudness · on",
            "preamp -3 dB",
        ] {
            assert!(drawing.contains(want), "{want}");
        }
        let first = picture_cached(&cfg, "#f2258c").unwrap();
        let again = picture_cached(&cfg, "#f2258c").unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        // Another colour, or a nudge, is another picture; off is grey.
        let other = picture_cached(&cfg, "#00ff00").unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        let mut off = cfg.clone();
        off.enabled = false;
        assert!(svg(&off, "#f2258c").contains(OFF_HEX));
        assert!(!svg(&off, "#f2258c").contains("#f2258c"));
        assert_eq!(
            summary(&cfg),
            "31 +6 · 62 +4 · 125 0 · 250 0 · 500 -2 · 1k 0 · 2k 0 · 4k +3 · 8k +6 · 16k +6"
        );
    }
}
