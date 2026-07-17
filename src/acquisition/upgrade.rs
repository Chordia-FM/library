//! Quality-upgrade sweep: periodically propose re-acquiring the worst all-lossy albums in
//! lossless quality. Fully automatic by design — the Hub queues `upgrade` jobs for the owner, the
//! pipeline only grabs strictly-better (lossless) releases and stays quiet when none exist, and
//! the existing dedupe pass retires the old lossy files into `superseded/` once the better copy
//! has been imported and indexed.
//!
//! Pacing is deliberately gentle: at most `upgrade_cap` albums per `upgrade_interval_days` per
//! library, worst-first — and each proposed album is stamped in `upgrade_attempts` so the next
//! sweep advances to the NEXT-worst albums instead of re-trying the same ones every week. Stamped
//! albums become eligible again after `upgrade_retry_days`, when new sources may exist.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chordia_contracts::acquisition::{UpgradeProposal, UpgradeProposals};
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::http::AppState;
use crate::pairing::HubClient;

/// How often eligibility is re-evaluated; the per-library `upgrade_interval_days` gates real work.
const CHECK_SECS: u64 = 6 * 60 * 60;
/// Let the initial scan / catalog settle before the first sweep.
const STARTUP_DELAY_SECS: u64 = 300;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One sweep hit: the proposal plus the local album id for the rotation stamp.
struct SweepRow {
    album_id: String,
    proposal: UpgradeProposal,
}

pub fn start_upgrade_scan(state: AppState) {
    let cfg = &state.config.acquisition;
    if !cfg.enabled || !cfg.upgrade_enabled {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(STARTUP_DELAY_SECS)).await;
        loop {
            if let Err(e) = run_once(&state).await {
                warn!(error = %e, "upgrade sweep failed");
            }
            tokio::time::sleep(Duration::from_secs(CHECK_SECS)).await;
        }
    });
}

async fn run_once(state: &AppState) -> anyhow::Result<()> {
    let Some(creds) = state.credentials.read().await.clone() else {
        return Ok(());
    };
    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());
    let cfg = &state.config.acquisition;
    let interval_ms = i64::from(cfg.upgrade_interval_days) * 86_400_000;
    let retry_cutoff = now_ms() - i64::from(cfg.upgrade_retry_days) * 86_400_000;

    let libs: Vec<(String, String)> =
        sqlx::query_as("SELECT id, hub_library_id FROM libraries WHERE hub_library_id IS NOT NULL")
            .fetch_all(&state.db)
            .await?;
    for (local_id, hub_id) in libs {
        let last: Option<i64> =
            sqlx::query_scalar("SELECT last_run_ms FROM upgrade_scan WHERE library_id = ?")
                .bind(&local_id)
                .fetch_optional(&state.db)
                .await?;
        if last.is_some_and(|l| now_ms() - l < interval_ms) {
            continue;
        }
        let Ok(hub_uuid) = hub_id.parse::<Uuid>() else {
            continue;
        };
        let rows = sweep_library(
            &state.db,
            &local_id,
            hub_uuid,
            i64::from(cfg.upgrade_cap),
            retry_cutoff,
        )
        .await?;
        if !rows.is_empty() {
            let proposals = UpgradeProposals {
                proposals: rows.iter().map(|r| r.proposal.clone()).collect(),
            };
            match hub
                .propose_upgrades(&creds.server_api_key, &proposals)
                .await
            {
                Ok(queued) => {
                    info!(
                        library = %local_id,
                        proposed = rows.len(),
                        queued,
                        "upgrade sweep proposed worst lossy albums"
                    );
                }
                Err(e) => {
                    // Don't stamp anything on failure: the next check retries the same sweep.
                    warn!(library = %local_id, error = %e, "upgrade proposal failed");
                    continue;
                }
            }
            // Rotation stamp: these albums sit out until the retry cooldown passes.
            for r in &rows {
                sqlx::query(
                    "INSERT INTO upgrade_attempts (album_id, last_proposed_ms) VALUES (?, ?) \
                     ON CONFLICT(album_id) DO UPDATE SET last_proposed_ms = excluded.last_proposed_ms",
                )
                .bind(&r.album_id)
                .bind(now_ms())
                .execute(&state.db)
                .await?;
            }
        }
        sqlx::query(
            "INSERT INTO upgrade_scan (library_id, last_run_ms) VALUES (?, ?) \
             ON CONFLICT(library_id) DO UPDATE SET last_run_ms = excluded.last_run_ms",
        )
        .bind(&local_id)
        .bind(now_ms())
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

/// The library's worst all-lossy albums (≥3 tracks so singles/EPs don't churn), worst-first by a
/// coarse resolution proxy, excluding albums proposed within the retry cooldown.
async fn sweep_library(
    db: &SqlitePool,
    local_id: &str,
    hub_id: Uuid,
    cap: i64,
    retry_cutoff_ms: i64,
) -> anyhow::Result<Vec<SweepRow>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        album_id: String,
        title: String,
        release_mbid: Option<String>,
        artist: String,
        artist_mbid: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT al.id AS album_id, al.title, al.release_mbid, \
                ar.name AS artist, ar.mbid AS artist_mbid \
         FROM albums al \
         JOIN artists ar ON ar.id = al.artist_id \
         JOIN tracks t ON t.album_id = al.id \
         JOIN library_tracks lt ON lt.track_id = t.id AND lt.library_id = ?1 \
         JOIN files f ON f.content_hash = t.content_hash \
         WHERE al.id NOT IN \
               (SELECT album_id FROM upgrade_attempts WHERE last_proposed_ms > ?2) \
         GROUP BY al.id \
         HAVING COUNT(DISTINCT t.id) >= 3 AND MAX(f.lossless) = 0 \
         ORDER BY AVG(f.sample_rate_hz * MAX(f.bit_depth, 1)) ASC, al.title \
         LIMIT ?3",
    )
    .bind(local_id)
    .bind(retry_cutoff_ms)
    .bind(cap)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SweepRow {
            album_id: r.album_id,
            proposal: UpgradeProposal {
                library_id: hub_id,
                album_title: r.title,
                artist: Some(r.artist),
                artist_mbid: r.artist_mbid,
                release_mbid: r.release_mbid,
                current_quality: Some("lossy".to_string()),
            },
        })
        .collect())
}
