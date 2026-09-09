//! Turning a catalog track into something songbird can play.
//!
//! The library already decodes nothing itself — it serves bytes and lets ffmpeg transcode. The bot
//! is the first in-process consumer of audio, and songbird brings the decoder: its `File` input runs
//! Symphonia (already in this crate's tree with every codec enabled) and its own codec registry adds
//! Opus. That covers FLAC, MP3, AAC/ALAC in MP4, Vorbis/Opus in Ogg and WAV — a music library — with
//! native seeking and no subprocess.
//!
//! Two cases fall through to `ffmpeg`, decoding to raw PCM on a pipe: a codec Symphonia does not
//! know, and a spatial (Atmos) file, whose object-based bitstream needs ffmpeg's downmix. That path
//! reuses `[transcode] ffmpeg_path`, so a library that can transcode can also play those.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use std::sync::Arc;

use songbird::input::{ChildContainer, Input, RawAdapter};
use symphonia::core::io::ReadOnlySource;

use crate::discord::eq;

use crate::catalog::{self, TrackRow};
use crate::error::{AppError, AppResult};
use crate::http::AppState;

/// The facts about a track's source and its delivery that views show as badges.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackFacts {
    pub codec: String,
    pub sample_rate_hz: u32,
    pub bit_depth: u8,
    pub channels: u8,
    pub lossless: bool,
    pub spatial: bool,
    /// The Opus bitrate negotiated for the voice channel, once known.
    pub opus_kbps: Option<u32>,
    pub gain_db: Option<f32>,
    pub peak: Option<f32>,
}

impl TrackFacts {
    pub fn from_row(row: &TrackRow) -> Self {
        Self {
            codec: row.codec.clone(),
            sample_rate_hz: row.sample_rate_hz.max(0) as u32,
            bit_depth: row.bit_depth.clamp(0, 255) as u8,
            channels: row.channels.clamp(0, 255) as u8,
            lossless: row.lossless != 0,
            spatial: row.spatial != 0,
            opus_kbps: None,
            gain_db: row.rg_gain_db.map(|g| g as f32),
            peak: row.rg_peak.map(|p| p as f32),
        }
    }
}

/// Codecs Symphonia (plus songbird's Opus decoder) handles in-process.
const NATIVE_CODECS: &[&str] = &["flac", "mp3", "aac", "alac", "vorbis", "opus", "pcm", "wav"];

/// Added to every track's ReplayGain before the peak cap.
///
/// ReplayGain's reference level (−18 LUFS) is a listening-room standard, and in a voice channel it
/// is simply quiet: a modern master carries a gain of −8 to −10 dB, so at "100 %" the bot played at
/// a third of full scale next to people talking and other bots at full blast. +9 dB moves the target
/// to about −9 LUFS, which puts a typical loud master back near unity and lifts quiet recordings up
/// to their true-peak ceiling, so tracks still land at one level, just a level that suits the room.
pub const REPLAYGAIN_PREAMP_DB: f64 = 9.0;

/// How far under full scale the true peak is kept, as a linear factor (≈ −1 dB). Sitting exactly
/// on the ceiling hands the loudest transients to the mixer's soft clipper, which is audible as
/// the level "breathing"; a decibel of air keeps it out of the way.
const PEAK_HEADROOM: f64 = 0.89;

/// The volume multiplier that applies a track's ReplayGain: the gain plus
/// [`REPLAYGAIN_PREAMP_DB`], capped so the true peak stays a decibel under full scale, and never
/// more than a doubling when the peak is unknown. `1.0` when the loudness pass has not reached this
/// track yet.
pub fn replaygain_multiplier(gain_db: Option<f64>, peak: Option<f64>) -> f32 {
    let Some(gain) = gain_db else {
        return 1.0;
    };
    let mut m = 10f64.powf((gain + REPLAYGAIN_PREAMP_DB) / 20.0);
    if let Some(p) = peak.filter(|p| *p > 0.0) {
        m = m.min(PEAK_HEADROOM / p);
    }
    m.clamp(0.0, 2.0) as f32
}

/// Build the songbird input for a track, plus its facts. `seek_ms` matters only for the ffmpeg
/// path, which cannot seek after the fact; the native path seeks through the track handle. With
/// an equalizer that changes anything, every file takes the ffmpeg path so the filters can sit
/// on its PCM; they read the shared settings, so later changes are heard without a restart.
pub async fn input_for(
    state: &AppState,
    row: &TrackRow,
    seek_ms: Option<u64>,
    eq: Option<Arc<eq::Shared>>,
) -> AppResult<(Input, TrackFacts)> {
    let path = catalog::get_track_path(&state.db, &row.id)
        .await?
        .ok_or_else(|| AppError::BadRequest("track has no file".into()))?;
    let path = PathBuf::from(path);
    if tokio::fs::metadata(&path).await.is_err() {
        return Err(AppError::BadRequest(format!(
            "file is missing on disk: {}",
            path.display()
        )));
    }
    let facts = TrackFacts::from_row(row);
    let filtered = eq.as_ref().is_some_and(|e| eq::active(&e.get()));
    let native = NATIVE_CODECS.contains(&row.codec.as_str()) && !facts.spatial && !filtered;
    let input = if native {
        Input::from(songbird::input::File::new(path))
    } else {
        ffmpeg_input(&state.config.transcode.ffmpeg_path, &path, seek_ms, eq)?
    };
    Ok((input, facts))
}

/// `ffmpeg` decoding to interleaved f32 PCM at 48 kHz stereo on stdout — exactly what songbird's
/// mixer wants, so it does no resampling of its own.
fn ffmpeg_input(
    ffmpeg: &str,
    path: &std::path::Path,
    seek_ms: Option<u64>,
    eq: Option<Arc<eq::Shared>>,
) -> AppResult<Input> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if let Some(ms) = seek_ms.filter(|ms| *ms > 0) {
        cmd.args(["-ss", &format!("{}.{:03}", ms / 1000, ms % 1000)]);
    }
    cmd.arg("-i")
        .arg(path)
        .args([
            "-vn", "-map", "0:a:0", "-f", "f32le", "-ar", "48000", "-ac", "2", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("spawning {ffmpeg}: {e}")))?;
    let pipe = ChildContainer::new(vec![child]);
    Ok(match eq {
        Some(shared) => Input::from(RawAdapter::new(
            ReadOnlySource::new(eq::Reader::new(pipe, shared)),
            48_000,
            2,
        )),
        None => Input::from(RawAdapter::new(ReadOnlySource::new(pipe), 48_000, 2)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaygain_math() {
        assert_eq!(replaygain_multiplier(None, None), 1.0);
        // A loud master (−9.2 dB) would land at ×0.977 after the preamp; the peak cap (×0.89 at a
        // true peak of 1.0) keeps it a decibel under full scale.
        let m = replaygain_multiplier(Some(-9.2), Some(1.0));
        assert!((m - 0.89).abs() < 0.01, "{m}");
        let m = replaygain_multiplier(Some(-12.0), Some(1.0));
        assert!((m - 0.708).abs() < 0.01, "{m}");
        // −15 dB + 9 = −6 dB ≈ ×0.501
        let m = replaygain_multiplier(Some(-15.0), None);
        assert!((m - 0.501).abs() < 0.01, "{m}");
        // +6 dB (+9) would be ×5.6, but a peak of 0.8 caps it at 0.89/0.8.
        let m = replaygain_multiplier(Some(6.0), Some(0.8));
        assert!((m - 1.1125).abs() < 0.001, "{m}");
        // Unknown peak: never more than a doubling.
        assert_eq!(replaygain_multiplier(Some(20.0), None), 2.0);
        // A zero/negative peak is treated as unknown rather than dividing by it.
        assert_eq!(replaygain_multiplier(Some(20.0), Some(0.0)), 2.0);
    }

    #[test]
    fn spatial_and_unknown_codecs_take_the_ffmpeg_path() {
        let native = |codec: &str, spatial: bool| NATIVE_CODECS.contains(&codec) && !spatial;
        assert!(native("flac", false));
        assert!(native("opus", false));
        assert!(!native("flac", true));
        assert!(!native("unknown", false));
        assert!(!native("wma", false));
    }
}
