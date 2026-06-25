//! EBU R128 loudness analysis to ReplayGain 2.0 track gain (Phase C6).
//!
//! Computes each track's integrated loudness and true peak by shelling out to `ffmpeg`'s `ebur128`
//! filter (the same binary the transcoder uses), then derives a ReplayGain 2.0 track gain
//! (reference -18 LUFS) and a linear peak. The client applies the gain as a preamp when the
//! user enables "Normalize volume", capping it at `1/peak` so normalization never introduces
//! clipping.
//!
//! Runs as a background pass with no scan-loop coupling, so a large library is analyzed gradually
//! without slowing scans. It is best-effort: a file is skipped (left NULL, retried a future run) if
//! ffmpeg is missing or analysis fails. Album gain is intentionally out of scope for now.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::config::Config;
use crate::http::AppState;

/// ReplayGain 2.0 reference loudness (LUFS). Gain = REFERENCE minus integrated_loudness.
const REFERENCE_LUFS: f64 = -18.0;
/// EBU R128 reports its absolute gating floor (about -70 LUFS, a finite value, not -inf) for silent
/// or near-silent content. Treat loudness at or below this as silence with neutral gain, rather than
/// computing the enormous boost a literal -70 LUFS would imply.
const SILENCE_FLOOR_LUFS: f64 = -69.0;
/// Clamp the computed gain to a sane range so a pathological measurement can never produce an
/// extreme boost or cut. The client also clip-protects against peak, but a very low real peak
/// wouldn't bound a huge gain on its own.
const MAX_ABS_GAIN_DB: f64 = 30.0;
/// How many un-analyzed files to process per pass.
const BATCH: i64 = 25;
/// Idle wait when there's nothing to do, or analysis is disabled, or ffmpeg is unavailable.
const IDLE_SECS: u64 = 300;
/// Give up on a file after this many failed analysis attempts, so a permanently-undecodable file
/// stops re-running ffmpeg every cycle and can't perpetually starve the batch. Re-indexing changed
/// content resets attempts (a new `files` row defaults to 0).
const MAX_ATTEMPTS: i64 = 3;

/// A track's measured loudness, reduced to what playback normalization needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    /// ReplayGain 2.0 track gain in dB (reference -18 LUFS).
    pub gain_db: f64,
    /// Linear true-peak amplitude (10^(dBFS/20)).
    pub peak: f64,
}

/// Parse the integrated loudness (LUFS) and true peak (dBFS) from ffmpeg's `ebur128` Summary block.
///
/// The filter logs a continuous per-frame line and a final indented `Summary:` block; we read
/// only the summary (everything after the last `Summary:`) to avoid the per-frame `I:` values.
fn parse_ebur128(stderr: &str) -> Option<(f64, f64)> {
    let summary = stderr
        .rsplit_once("Summary:")
        .map(|(_, s)| s)
        .unwrap_or(stderr);
    let mut integrated: Option<f64> = None;
    let mut peak: Option<f64> = None;
    for line in summary.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("I:") {
            integrated = parse_leading_float(rest);
        } else if let Some(rest) = t.strip_prefix("Peak:") {
            peak = parse_leading_float(rest);
        }
    }
    Some((integrated?, peak?))
}

/// Parse the first whitespace-separated token of `s` as an f64 (e.g. `"   -14.7 LUFS"` gives -14.7).
fn parse_leading_float(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse::<f64>().ok()
}

/// Convert a parsed `(integrated_lufs, true_peak_dbfs)` into ReplayGain gain + linear peak.
///
/// Silence is handled at two levels. EBU R128 reports a finite -70 LUFS gating floor (the true
/// `-inf` only shows up on the peak line), so we treat anything at or below `SILENCE_FLOOR_LUFS`,
/// and any non-finite value, as silence with neutral 0 dB gain. The result is also clamped to
/// plus or minus `MAX_ABS_GAIN_DB` so no measurement yields an extreme boost.
fn to_loudness(integrated_lufs: f64, true_peak_dbfs: f64) -> Loudness {
    let gain_db = if integrated_lufs.is_finite() && integrated_lufs > SILENCE_FLOOR_LUFS {
        (REFERENCE_LUFS - integrated_lufs).clamp(-MAX_ABS_GAIN_DB, MAX_ABS_GAIN_DB)
    } else {
        0.0
    };
    let peak = if true_peak_dbfs.is_finite() {
        10f64.powf(true_peak_dbfs / 20.0)
    } else {
        1.0
    };
    Loudness { gain_db, peak }
}

/// Why an analysis didn't produce a result. This distinguishes recoverable environment problems
/// from per-file failures so the worker can react differently (defer, or count an attempt).
pub enum AnalyzeError {
    /// ffmpeg couldn't be spawned (not installed, or bad path). Environmental and recoverable, so
    /// the worker defers the whole pass rather than burning a retry attempt on every file.
    Unavailable,
    /// ffmpeg ran but analysis failed (corrupt or undecodable file, missing summary). Likely
    /// permanent for this file, so the worker counts it as an attempt.
    Failed(anyhow::Error),
}

/// Analyze one file with `ffmpeg -af ebur128=peak=true`. Decodes the whole track for accurate,
/// gated integrated loudness, so it's not cheap, which is why this runs as a background pass.
///
/// stderr is streamed line-by-line and only the EBU R128 `Summary:` block (plus a short tail for
/// error context) is retained, so a long file's ~10 lines/sec of per-frame logging can't
/// accumulate unbounded in memory. `-loglevel error` isn't usable here because it would suppress
/// the Summary too, so we filter rather than silence.
pub async fn analyze(path: &Path, ffmpeg_path: &str) -> Result<Loudness, AnalyzeError> {
    use std::process::Stdio;

    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut child = tokio::process::Command::new(ffmpeg_path)
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(path)
        // First audio stream only (skip embedded cover-art video); measure true peak.
        .args(["-map", "0:a:0", "-af", "ebur128=peak=true", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| AnalyzeError::Unavailable)?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AnalyzeError::Failed(anyhow::anyhow!("no ffmpeg stderr")))?;
    let mut lines = BufReader::new(stderr).lines();
    let mut in_summary = false;
    let mut summary = String::new();
    let mut last_line = String::new(); // kept for error context on failure
    while let Ok(Some(line)) = lines.next_line().await {
        if line.contains("Summary:") {
            in_summary = true;
        }
        if in_summary {
            summary.push_str(&line);
            summary.push('\n');
        } else {
            last_line = line;
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| AnalyzeError::Failed(anyhow::anyhow!("waiting for ffmpeg: {e}")))?;
    if !status.success() {
        return Err(AnalyzeError::Failed(anyhow::anyhow!(
            "ffmpeg ebur128 failed ({status}): {}",
            last_line.trim()
        )));
    }
    let (integrated, peak) = parse_ebur128(&summary)
        .ok_or_else(|| AnalyzeError::Failed(anyhow::anyhow!("no ebur128 summary in output")))?;
    Ok(to_loudness(integrated, peak))
}

/// Spawn the background loudness-analysis pass. Disabled when `[loudness] enabled = false`.
pub fn start_analysis(state: AppState) {
    let cfg: Arc<Config> = state.config.clone();
    tokio::spawn(async move {
        loop {
            if !cfg.loudness.enabled {
                tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                continue;
            }
            match analyze_batch(&state, &cfg.transcode.ffmpeg_path).await {
                // Nothing analyzed (no pending files, or ffmpeg missing so all failed), so idle.
                Ok(0) => tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await,
                Ok(n) => info!(analyzed = n, "loudness: analyzed tracks"),
                Err(e) => {
                    warn!(error = %e, "loudness pass failed");
                    tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                }
            }
        }
    });
}

/// Analyze up to `BATCH` files with no loudness yet. Returns how many were measured this pass.
async fn analyze_batch(state: &AppState, ffmpeg_path: &str) -> anyhow::Result<u32> {
    // Files needing analysis + a filesystem path to read. Skip files that have already failed
    // MAX_ATTEMPTS times, and order by attempt count so a block of failing files can't starve
    // never-tried ones (lower attempts first).
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT f.content_hash, fp.path FROM files f \
         JOIN file_paths fp ON fp.content_hash = f.content_hash \
         WHERE f.rg_gain_db IS NULL AND f.rg_attempts < ? \
         GROUP BY f.content_hash \
         ORDER BY f.rg_attempts LIMIT ?",
    )
    .bind(MAX_ATTEMPTS)
    .bind(BATCH)
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let mut analyzed = 0u32;
    for (content_hash, path) in rows {
        match analyze(Path::new(&path), ffmpeg_path).await {
            Ok(l) => {
                sqlx::query("UPDATE files SET rg_gain_db = ?, rg_peak = ? WHERE content_hash = ?")
                    .bind(l.gain_db)
                    .bind(l.peak)
                    .bind(&content_hash)
                    .execute(&state.db)
                    .await?;
                analyzed += 1;
            }
            // ffmpeg not installed or bad path: environmental and recoverable, so defer the whole
            // pass without burning attempts, so all files stay retriable once ffmpeg is available.
            Err(AnalyzeError::Unavailable) => {
                warn!(ffmpeg = %ffmpeg_path, "loudness: ffmpeg unavailable, deferring pass");
                break;
            }
            // Per-file failure (corrupt or undecodable): count the attempt so we eventually stop
            // re-decoding it every cycle and it can't monopolise the batch (skipped after MAX_ATTEMPTS).
            Err(AnalyzeError::Failed(e)) => {
                warn!(content_hash = %content_hash, error = %e, "loudness analysis failed");
                let _ = sqlx::query(
                    "UPDATE files SET rg_attempts = rg_attempts + 1 WHERE content_hash = ?",
                )
                .bind(&content_hash)
                .execute(&state.db)
                .await;
            }
        }
    }
    Ok(analyzed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Parsed_ebur128_0 @ 0x55] t: 0.4  TARGET:-23 LUFS    M: -22.0 S:-120.7     I: -22.0 LUFS       LRA:  0.0 LU
[Parsed_ebur128_0 @ 0x55] Summary:

  Integrated loudness:
    I:         -14.7 LUFS
    Threshold: -25.4 LUFS

  Loudness range:
    LRA:         6.0 LU
    Threshold: -35.5 LUFS
    LRA low:   -19.4 LUFS
    LRA high:  -13.4 LUFS

  True peak:
    Peak:       -0.5 dBFS
";

    #[test]
    fn parses_summary_not_perframe() {
        let (i, p) = parse_ebur128(SAMPLE).expect("should parse summary");
        // The per-frame `I: -22.0` must be ignored in favour of the summary's `I: -14.7`.
        assert_eq!(i, -14.7);
        assert_eq!(p, -0.5);
    }

    #[test]
    fn computes_replaygain() {
        let l = to_loudness(-14.7, -0.5);
        // Gain = -18 - (-14.7) = -3.3 dB (this track is louder than reference, so attenuate).
        assert!((l.gain_db - -3.3).abs() < 1e-9);
        // Peak: 10^(-0.5/20) ≈ 0.944.
        assert!((l.peak - 0.944_060_876).abs() < 1e-6);
    }

    #[test]
    fn silence_maps_to_neutral_gain() {
        // EBU R128 reports a finite -70 LUFS floor for silence (peak is -inf). Both must map to a
        // neutral result rather than a huge +52 dB boost or stored inf.
        let floor = to_loudness(-70.0, f64::NEG_INFINITY);
        assert_eq!(floor.gain_db, 0.0);
        assert_eq!(floor.peak, 1.0);
        // Anything below the -69 floor is treated as silence with neutral gain.
        assert_eq!(to_loudness(-69.5, -60.0).gain_db, 0.0);
        // The non-finite path stays handled too.
        assert_eq!(to_loudness(f64::NEG_INFINITY, -1.0).gain_db, 0.0);
    }

    #[test]
    fn gain_is_clamped() {
        // A very quiet (but above-floor) master would imply a >30 dB boost; clamp it.
        assert_eq!(to_loudness(-55.0, -10.0).gain_db, 30.0);
        // And an extremely loud one is clamped on the cut side.
        assert_eq!(to_loudness(20.0, 0.0).gain_db, -30.0);
    }

    #[test]
    fn missing_fields_yield_none() {
        assert!(parse_ebur128("no summary here").is_none());
        // Summary present but missing the Peak line.
        assert!(parse_ebur128("Summary:\n    I:  -10.0 LUFS\n").is_none());
    }
}
