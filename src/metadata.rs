//! Metadata extraction and fingerprinting.
//!
//! `probe(path)` reads tags with `lofty` and probes the codec/container with `symphonia`, then
//! computes the SHA-256 content hash. The result maps directly to the SQLite `tracks` row.
//!
//! AcoustID (chromaprint) fingerprinting is deferred to M6. In the interim we use content_hash
//! and normalized metadata as the primary match signals.

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
