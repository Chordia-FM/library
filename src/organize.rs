//! On-disk file organisation.
//!
//! When a library has `organize = 1`, audio files are laid out from user templates. There are three
//! templates, chosen by what we know about a track:
//!   - album: the default, used for tracks that belong to an album.
//!   - single: tracks with no album (fall back to the album template if unset).
//!   - unknown: tracks we couldn't identify (no artist tag); fall back to the album template.
//!
//! A path segment whose variables all resolve empty is dropped (a single with no album skips the
//! album folder), and dangling separators left by an empty variable (e.g. `06 - ` becomes ``) are
//! trimmed. The file keeps its original extension.
//!
//! With `dedupe` on, two files that map to the same destination collapse to the single
//! highest-quality copy (the rest are deleted); with it off they get ` (2)`, ` (3)`, … suffixes.
//!
//! Content is never altered, only the location, so `files`/`tracks`/`cover_art` and the Hub
//! catalog (all keyed by `content_hash`) are unaffected. We move the file and update
//! `file_paths.path`; streaming resolves through `file_paths`, so playback keeps working.

use std::path::{Path, PathBuf};

use chordia_contracts::artists::primary_artist;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::index;

/// Track metadata used to render a destination path.
#[derive(Debug, Default, Clone, sqlx::FromRow)]
struct TrackMeta {
    title: String,
    artist: String,
    album_artist: Option<String>,
    album: Option<String>,
    track_no: Option<i64>,
    disc_no: Option<i64>,
    year: Option<i64>,
    genre: Option<String>,
}

/// What a track is, for picking which template applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    Album,
    Single,
    Unknown,
}

/// The probe stores this placeholder when a file carries no artist tag, our "unidentified" signal.
const UNKNOWN_ARTIST: &str = "Unknown Artist";

fn classify(meta: &TrackMeta) -> ContentKind {
    let artist = meta.artist.trim();
    if artist.is_empty() || artist.eq_ignore_ascii_case(UNKNOWN_ARTIST) {
        return ContentKind::Unknown;
    }
    match meta
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(_) => ContentKind::Album,
        None => ContentKind::Single,
    }
}

/// Resolved organise settings for a library.
pub struct OrgSettings {
    /// Album / default template (required when organise is on).
    pub album: String,
    /// Template for tracks with no album. Falls back to `album` when empty.
    pub single: Option<String>,
    /// Template for unidentified tracks. Falls back to `album` when empty.
    pub unknown: Option<String>,
    /// Collapse files that map to the same destination to the single highest-quality copy.
    pub dedupe: bool,
}

impl OrgSettings {
    fn template_for(&self, kind: ContentKind) -> &str {
        let pick = |o: &Option<String>| -> Option<String> {
            o.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match kind {
            ContentKind::Album => &self.album,
            ContentKind::Single => {
                if pick(&self.single).is_some() {
                    self.single.as_deref().unwrap()
                } else {
                    &self.album
                }
            }
            ContentKind::Unknown => {
                if pick(&self.unknown).is_some() {
                    self.unknown.as_deref().unwrap()
                } else {
                    &self.album
                }
            }
        }
    }
}

/// Everything needed to render one track's destination.
struct RenderCtx {
    meta: TrackMeta,
    /// Original file stem (no extension), used for `{filename}`.
    filename: String,
}

impl RenderCtx {
    /// Resolve a single `{var}` to its value, or empty string when absent/unknown.
    fn resolve_var(&self, name: &str) -> String {
        let m = &self.meta;
        let album_artist = m
            .album_artist
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(m.artist.as_str());
        match name.trim().to_ascii_lowercase().as_str() {
            // Use the *primary* artist so a multi-artist track still files under the main artist's
            // folder (e.g. "Drake feat. Rihanna" → "Drake"), matching the Hub's primary.
            "albumartist" | "album_artist" => primary_artist(album_artist),
            "artist" => primary_artist(&m.artist),
            "album" => m.album.clone().unwrap_or_default(),
            "title" => m.title.clone(),
            "filename" => self.filename.clone(),
            "track" | "trackno" | "track_no" => {
                m.track_no.map(|n| format!("{n:02}")).unwrap_or_default()
            }
            "disc" | "discno" | "disc_no" | "side" => {
                m.disc_no.map(|n| n.to_string()).unwrap_or_default()
            }
            "year" => m.year.map(|y| y.to_string()).unwrap_or_default(),
            "genre" => m.genre.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Substitute every `{var}` in one template segment. Unterminated `{` is kept literally.
    fn substitute(&self, segment: &str) -> String {
        let mut out = String::with_capacity(segment.len());
        let mut chars = segment.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '{' {
                out.push(c);
                continue;
            }
            let mut var = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '}' {
                    closed = true;
                    break;
                }
                var.push(c2);
            }
            if closed {
                out.push_str(&self.resolve_var(&var));
            } else {
                out.push('{');
                out.push_str(&var);
            }
        }
        out
    }

    /// Identity-defining variables that the template needs to produce a correct path. If the
    /// template references one of these and it resolves empty (e.g. an untagged track number), we
    /// don't yet have enough metadata to place the file, so organize skips it rather than emit a
    /// degraded name. Decorative vars (disc/year/genre/comment/filename) are not gated.
    const REQUIRED_VARS: &'static [&'static str] =
        &["track", "trackno", "track_no", "title", "album", "artist", "albumartist", "album_artist"];

    /// The identity-defining variables a template references that are currently missing (empty). A
    /// non-empty result means "don't organize this file yet, we lack the tags it needs".
    fn missing_required_vars(&self, template: &str) -> Vec<String> {
        let mut missing = Vec::new();
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '{' {
                continue;
            }
            let mut var = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '}' {
                    closed = true;
                    break;
                }
                var.push(c2);
            }
            let name = var.trim().to_ascii_lowercase();
            if closed
                && Self::REQUIRED_VARS.contains(&name.as_str())
                && self.resolve_var(&name).trim().is_empty()
                && !missing.contains(&name)
            {
                missing.push(name);
            }
        }
        missing
    }

    /// Render `template` into sanitized relative path segments (folders + filename stem, no
    /// extension). Empty folder segments collapse; the final segment (filename) always falls back
    /// to the title, then `"Unknown"`.
    fn render(&self, template: &str) -> Vec<String> {
        let raw: Vec<&str> = template
            .split('/')
            .filter(|s| !s.trim().is_empty())
            .collect();
        let last = raw.len().saturating_sub(1);
        let mut out: Vec<String> = Vec::new();
        for (i, seg) in raw.iter().enumerate() {
            let cleaned = tidy(&self.substitute(seg));
            if i == last {
                let name = if cleaned.is_empty() {
                    tidy(&self.meta.title)
                } else {
                    cleaned
                };
                out.push(if name.is_empty() {
                    "Unknown".to_string()
                } else {
                    name
                });
            } else if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
        if out.is_empty() {
            out.push("Unknown".to_string());
        }
        out
    }
}

/// Windows reserved device names. A path component equal to one of these is unusable.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// True for characters we trim from the ends of a segment (separators left dangling by empty vars).
fn is_edge_sep(c: char) -> bool {
    // Non-ASCII separators as \u escapes (en dash, em dash, middot) so a linter can't normalise a
    // literal em dash into a duplicate '-' arm (which silently drops em-dash trimming).
    c.is_whitespace() || matches!(c, '-' | '_' | ',' | '\u{2013}' | '\u{2014}' | '\u{00b7}')
}

/// Clean up a rendered segment, then sanitize it for the filesystem:
///   1. drop empty bracket groups left by empty vars (`[]`, `()`, `{}`),
///   2. collapse runs of whitespace,
///   3. trim dangling separators from both ends (so `06 - ` → `` and ` - Title` → `Title`),
///   4. replace path-illegal characters, avoid reserved names, clamp length.
fn tidy(s: &str) -> String {
    let mut t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    for (open, close) in [("(", ")"), ("[", "]"), ("{", "}")] {
        let empty = format!("{open}{close}");
        while t.contains(&empty) {
            t = t.replace(&empty, "");
        }
    }
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    sanitize_segment(t.trim_matches(is_edge_sep))
}

/// Make `s` safe as a single path component on both Windows and Unix.
fn sanitize_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if (c as u32) < 0x20 => {} // strip control characters
            _ => out.push(c),
        }
    }
    let mut result: String = out
        .trim()
        .trim_matches('.')
        .trim()
        .chars()
        .take(120) // keep components comfortably under filesystem limits
        .collect();
    result = result.trim().to_string();
    if RESERVED.contains(&result.to_ascii_uppercase().as_str()) {
        result.push('_');
    }
    result
}

/// Normalise a path for equality checks: forward slashes, no trailing slash, lowercase.
fn norm(p: &Path) -> String {
    p.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Build the ideal target path for `ctx` under `root`, with `ext` (no dot) appended to the stem.
fn build_target(root: &Path, template: &str, ctx: &RenderCtx, ext: &str) -> PathBuf {
    let segments = ctx.render(template);
    let last = segments.len() - 1;
    let mut target = root.to_path_buf();
    for (i, seg) in segments.iter().enumerate() {
        if i == last {
            target.push(if ext.is_empty() {
                seg.clone()
            } else {
                format!("{seg}.{ext}")
            });
        } else {
            target.push(seg);
        }
    }
    target
}

/// Comparable quality of a file: higher is better. (lossless, bit depth, sample rate, channels,
/// byte size), chosen so a lossless/hi-res/larger file wins.
async fn quality(db: &SqlitePool, content_hash: &str, path: &Path) -> (i64, i64, i64, i64, u64) {
    let (lossless, bit_depth, sample_rate_hz, channels): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT lossless, bit_depth, sample_rate_hz, channels FROM files WHERE content_hash = ?",
    )
    .bind(content_hash)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .unwrap_or((0, 0, 0, 0));
    let size = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    (lossless, bit_depth, sample_rate_hz, channels, size)
}

/// Delete a file from disk and drop it from the index (used when de-duplicating).
async fn discard(db: &SqlitePool, library_id: &str, path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
    let _ = index::remove_track(db, library_id, path).await;
}

/// Move `from` → `to`, falling back to copy+remove if `rename` fails (e.g. across volumes).
async fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if tokio::fs::rename(from, to).await.is_ok() {
        return Ok(());
    }
    tokio::fs::copy(from, to).await?;
    tokio::fs::remove_file(from).await
}

/// Move `current` → `target`, repoint `file_paths`, and prune emptied source directories.
async fn move_and_update(
    db: &SqlitePool,
    library_id: &str,
    root: &Path,
    current: &Path,
    target: &Path,
    current_str: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(p) = target.parent() {
        tokio::fs::create_dir_all(p).await?;
    }
    move_file(current, target).await?;
    let target_str = target.to_string_lossy().to_string();
    sqlx::query("UPDATE file_paths SET path = ? WHERE path = ? AND library_id = ?")
        .bind(&target_str)
        .bind(current_str)
        .bind(library_id)
        .execute(db)
        .await?;
    prune_empty_dirs(current.parent(), root).await;
    info!(from = %current_str, to = %target_str, "organize: moved");
    Ok(target.to_path_buf())
}

/// First non-colliding variant of `ideal` (`stem (2).ext`, `stem (3).ext`, …), skipping `current`.
async fn next_free(ideal: &Path, ext: &str, current: &Path) -> PathBuf {
    let parent = ideal.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = ideal
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    let mut cand = ideal.to_path_buf();
    let mut n = 2;
    while norm(&cand) != norm(current) && tokio::fs::try_exists(&cand).await.unwrap_or(false) {
        let name = if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        cand = parent.join(name);
        n += 1;
    }
    cand
}

/// Remove now-empty directories from `start` upward, stopping at (and never removing) `root`.
async fn prune_empty_dirs(start: Option<&Path>, root: &Path) {
    let mut dir = start.map(Path::to_path_buf);
    while let Some(d) = dir {
        if norm(&d) == norm(root) || !d.starts_with(root) {
            break;
        }
        match tokio::fs::remove_dir(&d).await {
            Ok(()) => dir = d.parent().map(Path::to_path_buf),
            Err(_) => break, // not empty (or error), so stop climbing
        }
    }
}

/// `(root, settings)` if `library_id` has organise enabled with a non-empty album template.
pub async fn library_settings(db: &SqlitePool, library_id: &str) -> Option<(PathBuf, OrgSettings)> {
    let (organize, album, single, unknown, dedupe, path): (
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        String,
    ) = sqlx::query_as(
        "SELECT organize, organize_template, organize_template_single, \
                organize_template_unknown, dedupe, path FROM libraries WHERE id = ?",
    )
    .bind(library_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()?;
    if organize == 0 {
        return None;
    }
    let album = album?;
    if album.trim().is_empty() {
        return None;
    }
    Some((
        PathBuf::from(path),
        OrgSettings {
            album,
            single,
            unknown,
            dedupe: dedupe != 0,
        },
    ))
}

/// Organise one already-indexed file into its template location, updating `file_paths`.
/// Returns the new path when the file was moved. Returns `None` when the file isn't indexed here,
/// is already in place, or was discarded as a duplicate.
pub async fn organize_file(
    db: &SqlitePool,
    library_id: &str,
    root: &Path,
    settings: &OrgSettings,
    current: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let current_str = current.to_string_lossy().to_string();

    let Some((content_hash,)) = sqlx::query_as::<_, (String,)>(
        "SELECT content_hash FROM file_paths WHERE path = ? AND library_id = ?",
    )
    .bind(&current_str)
    .bind(library_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };

    let Some(meta) = sqlx::query_as::<_, TrackMeta>(
        "SELECT t.title, COALESCE(ar.name, '') AS artist, aa.name AS album_artist, \
                al.title AS album, t.track_no, t.disc_no, al.year AS year, al.genre AS genre \
         FROM tracks t \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         LEFT JOIN artists aa ON aa.id = al.artist_id \
         WHERE t.content_hash = ?",
    )
    .bind(&content_hash)
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };

    let ext = current
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    let stem = current
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let kind = classify(&meta);
    let template = settings.template_for(kind).to_string();
    let ctx = RenderCtx {
        meta,
        filename: stem,
    };

    // Don't organize a file until we have the metadata its template needs. Acting on incomplete tags
    // would silently produce a degraded path (e.g. a track-numberless name), so we leave the file in
    // place until it's properly tagged/identified.
    let missing = ctx.missing_required_vars(&template);
    if !missing.is_empty() {
        tracing::debug!(
            file = %current_str, ?missing,
            "organize: skipping, missing metadata the template requires",
        );
        return Ok(None);
    }

    let ideal = build_target(root, &template, &ctx, &ext);
    if norm(&ideal) == norm(current) {
        return Ok(None); // already in place
    }

    // Decide the final target, handling a destination that's already taken by a different file.
    let occupied = tokio::fs::try_exists(&ideal).await.unwrap_or(false);
    let target = if !occupied {
        ideal.clone()
    } else if settings.dedupe {
        let ideal_str = ideal.to_string_lossy().to_string();
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT content_hash FROM file_paths WHERE path = ? AND library_id = ?")
                .bind(&ideal_str)
                .bind(library_id)
                .fetch_optional(db)
                .await?;
        match existing {
            // Destination holds an indexed track: keep whichever is higher quality.
            Some((existing_hash,)) => {
                let cur_q = quality(db, &content_hash, current).await;
                let ex_q = quality(db, &existing_hash, &ideal).await;
                if cur_q > ex_q {
                    discard(db, library_id, &ideal).await; // replace the lower-quality copy
                    ideal.clone()
                } else {
                    discard(db, library_id, current).await; // current is the redundant copy
                    info!(path = %current_str, "organize: dropped duplicate (lower quality)");
                    return Ok(None);
                }
            }
            // Occupied by an unindexed stray file, so don't delete it; disambiguate instead.
            None => next_free(&ideal, &ext, current).await,
        }
    } else {
        next_free(&ideal, &ext, current).await
    };

    move_and_update(db, library_id, root, current, &target, &current_str)
        .await
        .map(Some)
}

/// Re-lay every file in `library_id` under `settings`. Spawned when Organise is switched on.
pub async fn reorganize_library(
    db: &SqlitePool,
    library_id: &str,
    root: &Path,
    settings: &OrgSettings,
) {
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM file_paths WHERE library_id = ?")
        .bind(library_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();

    let mut moved = 0u32;
    let mut errors = 0u32;
    for p in paths {
        match organize_file(db, library_id, root, settings, Path::new(&p)).await {
            Ok(Some(_)) => moved += 1,
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                warn!(path = %p, error = %e, "organize: failed");
            }
        }
    }
    info!(library_id, moved, errors, "reorganize complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RenderCtx {
        RenderCtx {
            meta: TrackMeta {
                title: "Song".into(),
                artist: "The Artist".into(),
                album_artist: Some("Album Artist".into()),
                album: Some("The Album".into()),
                track_no: Some(3),
                disc_no: Some(1),
                year: Some(2020),
                genre: Some("Rock".into()),
            },
            filename: "10 raw file".into(),
        }
    }

    #[test]
    fn renders_album_track() {
        let segs = ctx().render("{albumartist}/{album}/{track} - {title}");
        assert_eq!(segs, vec!["Album Artist", "The Album", "03 - Song"]);
    }

    #[test]
    fn falls_back_to_artist_when_no_album_artist() {
        let mut c = ctx();
        c.meta.album_artist = None;
        let segs = c.render("{albumartist}/{title}");
        assert_eq!(segs, vec!["The Artist", "Song"]);
    }

    #[test]
    fn single_collapses_empty_album_and_strips_dangling_separator() {
        let mut c = ctx();
        c.meta.album = None;
        c.meta.track_no = None;
        // Even reusing the album template, the album folder collapses and the `06 - ` prefix is
        // stripped, so a single is never named `- Song`.
        let segs = c.render("{albumartist}/{album}/{track} - {title}");
        assert_eq!(segs, vec!["Album Artist", "Song"]);
    }

    #[test]
    fn single_template_groups_under_singles() {
        let mut c = ctx();
        c.meta.album = None;
        let segs = c.render("Singles/{artist}/{title}");
        assert_eq!(segs, vec!["Singles", "The Artist", "Song"]);
    }

    #[test]
    fn unknown_template_uses_filename() {
        let c = ctx();
        let segs = c.render("Unknown/{filename}");
        assert_eq!(segs, vec!["Unknown", "10 raw file"]);
    }

    #[test]
    fn empty_brackets_are_removed() {
        let mut c = ctx();
        c.meta.year = None;
        let segs = c.render("{title} [{year}]");
        assert_eq!(segs, vec!["Song"]);
    }

    #[test]
    fn sanitizes_illegal_characters() {
        let mut c = ctx();
        c.meta.album = Some("AC/DC: Live?".into());
        let segs = c.render("{album}/{title}");
        assert_eq!(segs[0], "AC_DC_ Live_");
    }

    #[test]
    fn classify_distinguishes_kinds() {
        let mut m = ctx().meta;
        assert_eq!(classify(&m), ContentKind::Album);
        m.album = None;
        assert_eq!(classify(&m), ContentKind::Single);
        m.artist = "Unknown Artist".into();
        assert_eq!(classify(&m), ContentKind::Unknown);
    }

    #[test]
    fn builds_target_with_extension() {
        let target = build_target(Path::new("/music"), "{album}/{title}", &ctx(), "flac");
        assert_eq!(norm(&target), "/music/the album/song.flac");
    }

    #[test]
    fn reserved_name_is_escaped() {
        assert_eq!(sanitize_segment("CON"), "CON_");
        assert_eq!(sanitize_segment("con"), "con_");
    }
}
