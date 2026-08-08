//! Metadata extraction and fingerprinting.
//!
//! `probe(path)` reads tags with `lofty` and probes the codec/container with `symphonia`, then
//! computes the SHA-256 content hash. The result maps directly to the SQLite `tracks` row.
//!
//! AcoustID (chromaprint) fingerprinting lives in [`crate::fingerprint`] as a background pass;
//! content_hash and normalized metadata computed here remain the synchronous match signals.

use std::io::Read;
use std::path::Path;

use lofty::picture::PictureType;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};
use sha2::{Digest, Sha256};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Embedded album art lifted from a file's tags, deduped by content hash.
#[derive(Debug, Clone)]
pub struct CoverArt {
    /// Raw image bytes.
    pub data: Vec<u8>,
    /// MIME type, e.g. `image/jpeg`.
    pub mime: String,
    /// Hex SHA-256 of the image bytes, used to dedupe art across an album or library.
    pub hash: String,
}

/// Everything extracted from a single audio file.
#[derive(Debug, Clone)]
pub struct ProbedTrack {
    pub title: String,
    pub artist: String,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub total_tracks: Option<u32>,
    pub total_discs: Option<u32>,
    pub composer: Option<String>,
    pub comment: Option<String>,
    pub isrc: Option<String>,
    /// Record label / publisher.
    pub label: Option<String>,
    pub bpm: Option<u32>,
    /// Whether the album is a compilation ("various artists").
    pub compilation: bool,
    /// Content advisory from the iTunes/ID3 rating tag: `"explicit"` / `"clean"`, `None` if unrated.
    pub advisory: Option<String>,
    /// Unsynchronized lyrics, if embedded.
    pub lyrics: Option<String>,
    /// MusicBrainz IDs embedded in the tags (Picard-tagged libraries). Save a network lookup.
    pub recording_mbid: Option<String>,
    pub release_mbid: Option<String>,
    pub mb_artist_id: Option<String>,
    /// Embedded front-cover art, if any.
    pub cover: Option<CoverArt>,
    /// Codec name, lowercase. e.g. `flac`, `mp3`, `alac`, `aac`, `vorbis`, `opus`, `pcm`.
    pub codec: String,
    pub sample_rate_hz: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub lossless: bool,
    /// Spatial or Atmos track, flagged passthrough_only and never transcoded.
    pub spatial: bool,
    pub duration_ms: u32,
    /// Hex SHA-256 of raw file bytes. Used for exact-file match and integrity checks.
    pub content_hash: String,
    // Normalized versions for fuzzy own-copy matching.
    pub artist_norm: String,
    pub title_norm: String,
    pub album_norm: Option<String>,
}

/// Probe an audio file: extract tags, codec info, and compute the content hash.
pub fn probe(path: &Path) -> anyhow::Result<ProbedTrack> {
    // Tags (lofty).
    let tagged = Probe::open(path)
        .map_err(|e| anyhow::anyhow!("lofty open '{path:?}': {e}"))?
        .read()
        .map_err(|e| anyhow::anyhow!("lofty read '{path:?}': {e}"))?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let title = tag
        .and_then(|t| t.title().map(|v| v.to_string()))
        .unwrap_or_else(|| stem(path));
    // Preserve multiple discrete artist values (e.g. ID3v2.4 multi-value frames) by joining with
    // "; ", which the Hub splits back into individual artist profiles. Falls back to the single
    // accessor, then to a placeholder.
    let artist = tag
        .map(|t| {
            t.get_strings(&ItemKey::TrackArtist)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty())
        .or_else(|| tag.and_then(|t| t.artist().map(|v| v.to_string())))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let album_artist = tag.and_then(|t| t.get_string(&ItemKey::AlbumArtist).map(String::from));
    let album = tag.and_then(|t| t.album().map(|v| v.to_string()));
    let year = tag.and_then(|t| t.year());
    let genre = tag.and_then(|t| t.genre().map(|v| v.to_string()));
    let track_no = tag.and_then(|t| t.track());
    let disc_no = tag.and_then(|t| t.disk());
    let total_tracks = tag.and_then(|t| t.track_total());
    let total_discs = tag.and_then(|t| t.disk_total());

    let str_tag = |key: &ItemKey| tag.and_then(|t| t.get_string(key).map(String::from));
    let composer = str_tag(&ItemKey::Composer);
    let comment = str_tag(&ItemKey::Comment);
    let isrc = str_tag(&ItemKey::Isrc);
    let label = str_tag(&ItemKey::Label);
    let lyrics = str_tag(&ItemKey::Lyrics);
    let bpm = str_tag(&ItemKey::Bpm)
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.round() as u32);
    let compilation = str_tag(&ItemKey::FlagCompilation)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // iTunes/ID3 advisory rating: "1" = explicit, "2" = clean, "0"/absent = unrated. lofty maps
    // the ID3v2 `ITUNESADVISORY` frame and MP4 `rtng` atom to `ParentalAdvisory`, but has no
    // Vorbis-comment mapping, so fall back to the raw `ITUNESADVISORY` key for FLAC/OGG.
    let advisory = str_tag(&ItemKey::ParentalAdvisory)
        .or_else(|| str_tag(&ItemKey::Unknown("ITUNESADVISORY".to_string())))
        .and_then(|v| match v.trim() {
            "1" => Some("explicit".to_string()),
            "2" => Some("clean".to_string()),
            _ => None,
        });
    let recording_mbid = str_tag(&ItemKey::MusicBrainzRecordingId);
    let release_mbid = str_tag(&ItemKey::MusicBrainzReleaseId);
    let mb_artist_id = str_tag(&ItemKey::MusicBrainzArtistId);
    let cover = tag.and_then(extract_cover);

    // Codec probe (symphonia).
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow::anyhow!("symphonia probe '{path:?}': {e}"))?;

    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("no audio track in '{path:?}'"))?;

    let params = &track.codec_params;
    let codec = codec_name(params.codec);
    let sample_rate_hz = params.sample_rate.unwrap_or(44100);
    let bit_depth = params.bits_per_sample.unwrap_or(16) as u32;
    let channels = params.channels.map(|c| c.count() as u32).unwrap_or(2);
    let duration_ms = params
        .time_base
        .zip(params.n_frames)
        .map(|(tb, frames)| {
            let secs = frames as f64 * tb.numer as f64 / tb.denom as f64;
            (secs * 1000.0) as u32
        })
        .unwrap_or(0);

    let lossless = is_lossless(&codec);
    let spatial = is_spatial(&codec, path);

    // Content hash (SHA-256).
    let mut raw = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = raw.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let content_hash = hex::encode(hasher.finalize());

    Ok(ProbedTrack {
        artist_norm: normalize(&artist),
        title_norm: normalize(&title),
        album_norm: album.as_deref().map(normalize),
        title,
        artist,
        album_artist,
        album,
        year,
        genre,
        track_no,
        disc_no,
        total_tracks,
        total_discs,
        composer,
        comment,
        isrc,
        label,
        bpm,
        compilation,
        advisory,
        lyrics,
        recording_mbid,
        release_mbid,
        mb_artist_id,
        cover,
        codec,
        sample_rate_hz,
        bit_depth,
        channels,
        lossless,
        spatial,
        duration_ms,
        content_hash,
    })
}

/// Pick the front cover (or first available picture) from a tag and hash it for dedup.
fn extract_cover(tag: &Tag) -> Option<CoverArt> {
    let pics = tag.pictures();
    let pic = pics
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pics.first())?;
    let data = pic.data().to_vec();
    if data.is_empty() {
        return None;
    }
    let mime = pic
        .mime_type()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "image/jpeg".to_string());
    let hash = hex::encode(Sha256::digest(&data));
    Some(CoverArt { data, mime, hash })
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string()
}

/// Simple normalization: lowercase + collapse whitespace + strip common punctuation.
pub fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Qualifiers that mark the SAME album with extra/changed tracks, folded into the base album (the
/// edition is kept on the track), so e.g. "X" and "X (Deluxe)" share one album.
const EDITION_KEYWORDS: &[&str] = &[
    "deluxe",
    "expanded",
    "special",
    "anniversary",
    "collector",
    "complete",
    "definitive",
    "ultimate",
    "platinum",
    "bonus",
];
/// Qualifiers that mark a genuinely DIFFERENT work: never folded (kept as a distinct album). This
/// includes remasters/reissues: they're an ALTERNATE master of the same tracks, not bonus content, so
/// folding them would make dedupe collide them with the originals (keeping only one master) or list
/// every track twice. Keeping them separate preserves the original AND every remaster as its own album.
const VERSION_KEYWORDS: &[&str] = &[
    "live",
    "acoustic",
    "instrumental",
    "remix",
    "demo",
    "karaoke",
    "mono",
    "stereo",
    "commentary",
    "cappella",
    "acapella",
    "unplugged",
    "orchestral",
    "session",
    "remaster",
    "reissue",
];

/// Split an album title into `(base_title, edition)`. A trailing `(…)`/`[…]` edition qualifier
/// ("Deluxe", "Special Edition", …) is pulled off so those tracks fold into the base album; "version"
/// qualifiers (Live, Acoustic, Remaster, …) and ordinary parentheses (years, "Pt. 2", "feat. …") are
/// left intact so genuinely-different works (including remasters/reissues) stay separate.
pub fn parse_edition(title: &str) -> (String, Option<String>) {
    let trimmed = title.trim();
    let Some((base, inner)) = trailing_bracket(trimmed) else {
        return (trimmed.to_string(), None);
    };
    let low = inner.to_lowercase();
    if VERSION_KEYWORDS.iter().any(|k| low.contains(k)) {
        return (trimmed.to_string(), None);
    }
    let is_edition = EDITION_KEYWORDS.iter().any(|k| low.contains(k))
        || low.ends_with("edition")
        || low.ends_with("version");
    let base = base.trim();
    if is_edition && !base.is_empty() {
        (base.to_string(), Some(inner.trim().to_string()))
    } else {
        (trimmed.to_string(), None)
    }
}

/// Split an album title into `(base_title, version)` where version is `Some("instrumental")` /
/// `Some("live")` when the TRAILING bracket marks one — "X (Instrumental)", "X [Live at Wembley
/// 1986]". Aligned with the Hub's `classify_version`: only instrumental/live count as versions
/// here; everything else (remaster, acoustic, years, "feat. …") returns `(title, None)`. Matching
/// is token-based, not substring, so "X (Delivered)" or "X (Alive)" never match. Backs the
/// `{version}` organize template variable; album identity (title_normalized) is untouched.
pub fn parse_version(title: &str) -> (String, Option<&'static str>) {
    let trimmed = title.trim();
    let Some((base, inner)) = trailing_bracket(trimmed) else {
        return (trimmed.to_string(), None);
    };
    let base = base.trim();
    if base.is_empty() {
        return (trimmed.to_string(), None);
    }
    let low = inner.to_lowercase();
    let mut tokens = low
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty());
    let version = if tokens
        .clone()
        .any(|t| t == "instrumental" || t == "instrumentals")
    {
        Some("instrumental")
    } else if tokens.any(|t| t == "live") {
        Some("live")
    } else {
        None
    };
    match version {
        Some(v) => (base.to_string(), Some(v)),
        None => (trimmed.to_string(), None),
    }
}

/// Extract a trailing `(…)` or `[…]` group as `(before, inner)`; `None` if the string doesn't end in
/// a closing bracket.
fn trailing_bracket(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_end();
    let (open, close) = match s.chars().last()? {
        ')' => ('(', ')'),
        ']' => ('[', ']'),
        _ => return None,
    };
    let open_idx = s.rfind(open)?;
    Some((
        &s[..open_idx],
        &s[open_idx + open.len_utf8()..s.len() - close.len_utf8()],
    ))
}

fn is_lossless(codec: &str) -> bool {
    matches!(
        codec,
        "flac" | "alac" | "pcm" | "wav" | "aiff" | "ape" | "wavpack"
    )
}

fn is_spatial(codec: &str, path: &Path) -> bool {
    // E-AC-3 JOC (Atmos) or TrueHD with Atmos object track. This is simplified and flags by
    // codec name; full detection would inspect the bitstream.
    if matches!(codec, "eac3" | "truehd") {
        return true;
    }
    // Dolby Atmos in MP4/M4A, checked by extension for now.
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if ext.eq_ignore_ascii_case("atmos") {
            return true;
        }
    }
    false
}

fn codec_name(codec: symphonia::core::codecs::CodecType) -> String {
    use symphonia::core::codecs::*;
    match codec {
        CODEC_TYPE_FLAC => "flac",
        CODEC_TYPE_MP3 => "mp3",
        CODEC_TYPE_AAC => "aac",
        CODEC_TYPE_ALAC => "alac",
        CODEC_TYPE_VORBIS => "vorbis",
        CODEC_TYPE_OPUS => "opus",
        CODEC_TYPE_PCM_S16LE | CODEC_TYPE_PCM_S16BE | CODEC_TYPE_PCM_S24LE
        | CODEC_TYPE_PCM_S24BE | CODEC_TYPE_PCM_S32LE | CODEC_TYPE_PCM_S32BE
        | CODEC_TYPE_PCM_F32LE | CODEC_TYPE_PCM_F32BE => "pcm",
        _ => "unknown",
    }
    .to_string()
}

/// The canonical facts a download job already knew, written into the file itself.
///
/// `None` leaves the file's existing value alone — we only overwrite what we actually know.
#[derive(Default)]
pub struct CanonicalTags<'a> {
    pub title: Option<&'a str>,
    pub album: Option<&'a str>,
    pub track_no: Option<u16>,
    pub disc_no: Option<u16>,
    pub isrc: Option<&'a str>,
    pub recording_mbid: Option<&'a str>,
    pub release_mbid: Option<&'a str>,
}

impl CanonicalTags<'_> {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.album.is_none()
            && self.track_no.is_none()
            && self.disc_no.is_none()
            && self.isrc.is_none()
            && self.recording_mbid.is_none()
            && self.release_mbid.is_none()
    }
}

/// Write canonical metadata into a file's tags.
///
/// **Order matters to the caller, not to this function.** `content_hash` is a SHA-256 of the whole
/// file, tags included, so retagging changes a track's identity. Do this BEFORE the file is
/// indexed — tagging afterwards makes the next scan compute a different hash, insert a second track
/// row and orphan the first.
///
/// Writes in place. lofty rewrites only the tag block, and the alternative — copy, tag, rename —
/// costs a full duplicate of every imported file for a failure mode (interrupted mid-write) that
/// leaves a re-downloadable file damaged rather than anything unrecoverable.
pub fn write_canonical_tags(path: &Path, want: &CanonicalTags<'_>) -> anyhow::Result<()> {
    if want.is_empty() {
        return Ok(());
    }
    let mut tagged = Probe::open(path)?.read()?;
    let kind = tagged.primary_tag_type();
    if tagged.primary_tag().is_none() {
        tagged.insert_tag(Tag::new(kind));
    }
    let tag = tagged
        .primary_tag_mut()
        .ok_or_else(|| anyhow::anyhow!("no writable tag for {}", path.display()))?;

    let mut set = |key: ItemKey, value: Option<String>| {
        if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
            tag.insert_text(key, v);
        }
    };
    set(ItemKey::TrackTitle, want.title.map(str::to_string));
    set(ItemKey::AlbumTitle, want.album.map(str::to_string));
    set(ItemKey::TrackNumber, want.track_no.map(|n| n.to_string()));
    set(ItemKey::DiscNumber, want.disc_no.map(|n| n.to_string()));
    set(ItemKey::Isrc, want.isrc.map(str::to_string));
    set(
        ItemKey::MusicBrainzRecordingId,
        want.recording_mbid.map(str::to_string),
    );
    set(
        ItemKey::MusicBrainzReleaseId,
        want.release_mbid.map(str::to_string),
    );

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    tagged.save_to(&mut file, lofty::config::WriteOptions::default())?;
    Ok(())
}

#[cfg(test)]
mod canonical_tag_tests {
    use super::*;

    /// A real (tiny, silent) FLAC, so lofty writes a genuine tag block rather than a stub and the
    /// test exercises the same reader the scanner uses.
    const TINY_FLAC: &[u8] = include_bytes!("testdata/tiny.flac");

    fn fixture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("chordia-tagtest");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, TINY_FLAC).unwrap();
        path
    }

    /// The round trip that makes this fix durable: canonical facts go into the FILE, and `probe` —
    /// what the scanner calls — reads them back. Correcting only the database left the file still
    /// saying "Back On Top 24", so a forced re-index read the wrong title straight back out.
    #[test]
    fn canonical_tags_survive_a_reprobe() {
        let path = fixture("roundtrip.flac");
        write_canonical_tags(
            &path,
            &CanonicalTags {
                title: Some("Historic Cemetery"),
                album: Some("Back on Top"),
                track_no: Some(6),
                disc_no: Some(1),
                isrc: Some("USAT21703201"),
                recording_mbid: Some("eeb02927-7e25-47d4-b142-287c38e0241c"),
                release_mbid: Some("0c2ff228-c00c-4c17-bd57-4536a5201ac4"),
            },
        )
        .expect("write tags");

        let got = probe(&path).expect("probe after");
        assert_eq!(got.title, "Historic Cemetery");
        assert_eq!(got.album.as_deref(), Some("Back on Top"));
        assert_eq!(got.track_no, Some(6));
        assert_eq!(got.disc_no, Some(1));
        assert_eq!(got.isrc.as_deref(), Some("USAT21703201"));
        assert_eq!(
            got.recording_mbid.as_deref(),
            Some("eeb02927-7e25-47d4-b142-287c38e0241c")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Why the caller must retag BEFORE indexing. `content_hash` covers the whole file, tags
    /// included, so writing tags moves a track's identity — do it after the scan and the next pass
    /// computes a different hash, inserts a second row and orphans the first.
    #[test]
    fn retagging_moves_the_content_hash() {
        let path = fixture("hash.flac");
        let before = probe(&path).expect("probe before").content_hash;
        write_canonical_tags(
            &path,
            &CanonicalTags {
                album: Some("Back on Top"),
                ..Default::default()
            },
        )
        .expect("write tags");
        let after = probe(&path).expect("probe after").content_hash;
        assert_ne!(before, after, "tags are inside the content hash");
        let _ = std::fs::remove_file(&path);
    }

    /// Knowing nothing must write nothing. An empty request that still rewrote the file would move
    /// every import's content hash for no reason at all.
    #[test]
    fn an_empty_request_never_touches_the_file() {
        let path = fixture("untouched.flac");
        let before = probe(&path).expect("probe before").content_hash;
        write_canonical_tags(&path, &CanonicalTags::default()).expect("no-op");
        assert_eq!(probe(&path).expect("probe after").content_hash, before);
        let _ = std::fs::remove_file(&path);
    }
}
