//! Own-copy track matching.
//!
//! Resolves a `MatchQuery` against the local index strongest → weakest:
//! `content_hash` → `acoustid` → `recording_mbid` → fuzzy `(artist_norm, title_norm, duration ±2s)`.

use chordia_contracts::catalog::{MatchQuery, MatchResult, MatchStrength};
use sqlx::SqlitePool;

use crate::catalog;
use crate::error::AppResult;

pub async fn match_track(db: &SqlitePool, q: &MatchQuery) -> AppResult<MatchResult> {
    if let Some(ref hash) = q.content_hash {
        if let Some(row) = catalog::find_track_by_hash(db, hash).await? {
            return Ok(MatchResult {
                track: Some(row.into_contract()),
                matched_on: Some(MatchStrength::ContentHash),
            });
        }
    }
    if let Some(ref id) = q.acoustid {
        if let Some(row) = catalog::find_track_by_acoustid(db, id).await? {
            return Ok(MatchResult {
                track: Some(row.into_contract()),
                matched_on: Some(MatchStrength::Acoustid),
            });
        }
    }
    if let Some(ref mbid) = q.recording_mbid {
        if let Some(row) = catalog::find_track_by_mbid(db, mbid).await? {
            return Ok(MatchResult {
                track: Some(row.into_contract()),
                matched_on: Some(MatchStrength::RecordingMbid),
            });
        }
    }
    if let (Some(ref artist), Some(ref title), Some(dur)) =
        (&q.artist_norm, &q.title_norm, q.duration_ms)
    {
        if let Some(row) = catalog::find_track_fuzzy(db, artist, title, dur).await? {
            return Ok(MatchResult {
                track: Some(row.into_contract()),
                matched_on: Some(MatchStrength::FuzzyMetadata),
            });
        }
    }
    Ok(MatchResult {
        track: None,
        matched_on: None,
    })
}
