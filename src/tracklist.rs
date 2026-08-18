//! Parser for the `ALBUM - Tracklist.txt` sidecar some downloaders leave beside an album.
//!
//! It carries more than any tag does: per-track personnel with roles, the label, an album-level
//! genre, a real release date, and an explicit marker on both the album and every track. Tags carry
//! some of that badly and most of it not at all, so this is the richest description of a release we
//! ever get for free.
//!
//! Deliberately a PURE function over `&str` — no filesystem, no database, no config. The discovery
//! and storage sit above it, so the fiddly half (a hand-written text format from a third-party tool,
//! with alignment padding and optional fields) can be tested exhaustively against real files without
//! a scan, a temp dir or a pool.
//!
//! ## What it will not do
//!
//! It never guesses. A line that does not match is skipped rather than half-parsed, a missing header
//! field is `None` rather than an empty string, and a file whose header is unrecognisable returns
//! `None` for the whole parse. Credits are the kind of data nobody double-checks once it renders, so
//! wrong is materially worse than absent.

use std::time::Duration;

/// The `[E]` an explicit title carries, on both the album line and each track.
const EXPLICIT_MARKER: &str = "[E]";

/// One credited name and everything it was credited for.
///
/// Roles are a list because one person routinely holds several on the same track — "Ant, Producer,
/// Beats, MainArtist" is one person with three roles, and splitting him into three rows would make
/// a credits panel repeat his name three times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    pub name: String,
    pub roles: Vec<String>,
}

impl Credit {
    /// Whether this credit names an organisation rather than a person.
    ///
    /// Publishers arrive in the same list as performers ("Upside Down Heart music, MusicPublisher"),
    /// and without this they leak into artist lists and search as though they were people. The role
    /// is the only signal the format gives.
    pub fn is_organisation(&self) -> bool {
        self.roles
            .iter()
            .any(|r| r.eq_ignore_ascii_case("MusicPublisher"))
    }
}

/// One track's row plus the credits indented beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEntry {
    /// 1-based, as printed. The join key to the indexed track — NOT the title, which carries the
    /// explicit marker and will not match a tag exactly.
    pub number: u32,
    pub title: String,
    pub explicit: bool,
    /// From the `[MM:SS]` column. Used to verify the file describes the album it sits beside.
    pub duration: Option<Duration>,
    pub credits: Vec<Credit>,
}

/// The header block above the tracks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumMeta {
    pub title: Option<String>,
    pub explicit: bool,
    pub composer: Option<String>,
    pub main_artist: Option<String>,
    pub label: Option<String>,
    pub genre: Option<String>,
    /// `YYYY-MM-DD`, kept as text. sqlx here is built without the chrono feature, and the Hub binds
    /// dates as `$n::date` from strings anyway.
    pub release_date: Option<String>,
    /// Free text, e.g. `FLAC (24-Bit / 44.1 kHz)`. Worth keeping to cross-check what was probed.
    pub quality: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracklist {
    pub album: AlbumMeta,
    pub tracks: Vec<TrackEntry>,
}

impl Tracklist {
    /// Whether these durations plausibly describe `probed`, matched by track number.
    ///
    /// The guard exists because a `Tracklist.txt` left in the wrong folder parses perfectly and
    /// writes confident, wrong credits — the kind of corruption nobody notices, because every field
    /// looks like a real value. Compared with a tolerance: the file prints whole seconds and
    /// encoders disagree about the last frame.
    pub fn matches(&self, probed: &[(u32, Duration)], tolerance: Duration) -> bool {
        if self.tracks.is_empty() || probed.is_empty() {
            return false;
        }
        let mut compared = 0usize;
        for (number, actual) in probed {
            let Some(entry) = self.tracks.iter().find(|t| t.number == *number) else {
                continue;
            };
            let Some(expected) = entry.duration else {
                continue;
            };
            let delta = expected.max(*actual) - expected.min(*actual);
            if delta > tolerance {
                return false;
            }
            compared += 1;
        }
        // A file that shares no track numbers with the album is not a match, however few
        // disagreements that produced.
        compared > 0
    }
}

/// Parse a tracklist file. `None` when the text is not one.
pub fn parse(text: &str) -> Option<Tracklist> {
    let mut album = AlbumMeta::default();
    let mut tracks: Vec<TrackEntry> = Vec::new();
    let mut saw_header = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('=') {
            continue;
        }

        // `* Name, Role, Role` — belongs to the track above it. Checked first because a credit line
        // can otherwise look like a header field once it contains a colon.
        if let Some(rest) = trimmed.strip_prefix("* ") {
            if let Some(track) = tracks.last_mut() {
                if let Some(credit) = parse_credit(rest) {
                    track.credits.push(credit);
                }
            }
            continue;
        }

        if let Some(track) = parse_track_line(trimmed) {
            tracks.push(track);
            continue;
        }

        if let Some((key, value)) = trimmed.split_once(':') {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            saw_header = true;
            // The keys are padded to a fixed width for alignment, so they are trimmed before
            // matching and compared case-insensitively — the format is hand-written and its
            // producers are not consistent.
            match key.trim().to_ascii_uppercase().as_str() {
                "ALBUM" => {
                    let (title, explicit) = strip_explicit(value);
                    album.title = Some(title);
                    album.explicit = explicit;
                }
                "COMPOSER" => album.composer = Some(value.to_string()),
                "MAIN ART." | "MAIN ART" | "MAIN ARTIST" => {
                    album.main_artist = Some(value.to_string());
                }
                "LABEL" => album.label = Some(value.to_string()),
                "GENRE" => album.genre = Some(value.to_string()),
                "RELEASE" => album.release_date = Some(value.to_string()),
                "QUALITY" => album.quality = Some(value.to_string()),
                _ => {}
            }
        }
    }

    // Neither half alone is a tracklist: a header with no tracks describes nothing, and stray
    // numbered lines are as likely to be a README as a credits file.
    if !saw_header || tracks.is_empty() {
        return None;
    }
    Some(Tracklist { album, tracks })
}

/// `01. Title [E]                    [03:58]` → number, title, explicit, duration.
fn parse_track_line(line: &str) -> Option<TrackEntry> {
    let (number, rest) = line.split_once('.')?;
    let number: u32 = number.trim().parse().ok()?;

    // The duration is the LAST bracketed group, because the title can contain brackets of its own —
    // `[E]` always, and occasionally a version marker.
    let open = rest.rfind('[')?;
    let close = rest[open..].find(']')? + open;
    let duration = parse_duration(&rest[open + 1..close]);
    // Only a real `[MM:SS]` ends the line; anything else is part of the title and the row has no
    // duration column at all.
    let title_part = if duration.is_some() {
        &rest[..open]
    } else {
        rest
    };

    let (title, explicit) = strip_explicit(title_part.trim());
    if title.is_empty() {
        return None;
    }
    Some(TrackEntry {
        number,
        title,
        explicit,
        duration,
        credits: Vec::new(),
    })
}

/// `MM:SS`, or `HH:MM:SS` for the occasional long mix.
fn parse_duration(raw: &str) -> Option<Duration> {
    let parts: Vec<&str> = raw.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut secs = 0u64;
    for part in &parts {
        let value: u64 = part.trim().parse().ok()?;
        secs = secs * 60 + value;
    }
    Some(Duration::from_secs(secs))
}

/// `Name, Role, Role` → the name and its roles.
fn parse_credit(raw: &str) -> Option<Credit> {
    let mut parts = raw.split(',').map(str::trim).filter(|p| !p.is_empty());
    let name = parts.next()?.to_string();
    let roles: Vec<String> = parts.map(str::to_string).collect();
    // A bare name with no role says nothing about what the person did, and a credits panel with a
    // blank role column is worse than one entry fewer.
    if name.is_empty() || roles.is_empty() {
        return None;
    }
    Some(Credit { name, roles })
}

/// Split the trailing `[E]` off a title.
fn strip_explicit(value: &str) -> (String, bool) {
    match value.trim().strip_suffix(EXPLICIT_MARKER) {
        Some(rest) => (rest.trim().to_string(), true),
        None => (value.trim().to_string(), false),
    }
}
