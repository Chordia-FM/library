//! Scrobble buffering + forwarding (M5 / Phase A3).
//!
//! - **queue**    - a durable SQLite-backed queue (`pending_scrobbles`) of the owner's
//!   `ListeningEvent`s. The library reports on its owner's behalf and buffers here whenever the Hub
//!   is unreachable. Owner-scoped: events arrive via the management-token `POST /v1/scrobbles`
//!   endpoint, so the Hub can safely attribute them to the server's owner.
//! - **reporter** - a background loop that flushes batches to the Hub `POST /v1/scrobbles:ingest`
//!   (server-API-key authed) with retry + backoff; the Hub dedupes on `event_id`, so a re-send
//!   after a partial failure never double-counts. Rows are deleted only once the Hub acks.

use std::sync::Arc;
use std::time::Duration;

use chordia_contracts::scrobble::{ListeningEvent, ScrobbleBatch};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::error::{AppError, AppResult};
use crate::http::AppState;
use crate::pairing::HubClient;

/// Max events forwarded per Hub request.
const BATCH_SIZE: i64 = 100;
/// Idle poll interval when the queue is empty / not paired.
const IDLE_SECS: u64 = 30;
/// Backoff after a failed forward (Hub down).
const BACKOFF_SECS: u64 = 60;

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Durably enqueue an event for forwarding. Idempotent on `event_id`.
pub async fn enqueue(db: &SqlitePool, event: &ListeningEvent) -> AppResult<()> {
    let payload = serde_json::to_string(event)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serializing scrobble: {e}")))?;
    sqlx::query(
        "INSERT OR IGNORE INTO pending_scrobbles (event_id, payload, created_at) VALUES (?, ?, ?)",
    )
    .bind(event.event_id.to_string())
    .bind(payload)
    .bind(now_millis())
    .execute(db)
    .await?;
    Ok(())
}

/// Oldest queued events (up to `BATCH_SIZE`), as `(event_id, event)`. Rows whose payload no longer
/// parses against the current contract are dropped so they can't wedge the queue forever.
async fn take_batch(db: &SqlitePool) -> AppResult<Vec<(String, ListeningEvent)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_id, payload FROM pending_scrobbles ORDER BY created_at LIMIT ?",
    )
    .bind(BATCH_SIZE)
    .fetch_all(db)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    let mut poison = Vec::new();
    for (id, payload) in rows {
        match serde_json::from_str::<ListeningEvent>(&payload) {
            Ok(ev) => out.push((id, ev)),
            Err(e) => {
                warn!(event_id = %id, error = %e, "dropping unparseable queued scrobble");
                poison.push(id);
            }
        }
    }
    if !poison.is_empty() {
        delete_ids(db, &poison).await?;
    }
    Ok(out)
}

/// Delete acked (or poison) rows by id.
async fn delete_ids(db: &SqlitePool, ids: &[String]) -> AppResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("DELETE FROM pending_scrobbles WHERE event_id IN ({placeholders})");
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in ids {
        q = q.bind(id);
    }
    q.execute(db).await?;
    Ok(())
}

/// Spawn the background reporter that forwards buffered events to the Hub.
pub fn start_reporter(state: AppState) {
    tokio::spawn(async move {
        let hub = Arc::new(HubClient::new(
            state.config.backend_url.clone(),
            state.http.clone(),
        ));
        loop {
            // Need credentials to authenticate the forward; idle until paired.
            let api_key = match state.credentials.read().await.as_ref() {
                Some(c) => c.server_api_key.clone(),
                None => {
                    tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                    continue;
                }
            };

            let batch = match take_batch(&state.db).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "reading scrobble queue failed");
                    tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                    continue;
                }
            };
            if batch.is_empty() {
                tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                continue;
            }

            let ids: Vec<String> = batch.iter().map(|(id, _)| id.clone()).collect();
            let events: Vec<ListeningEvent> = batch.into_iter().map(|(_, ev)| ev).collect();
            let payload = ScrobbleBatch { events };

            match hub.forward_scrobbles(&api_key, &payload).await {
                Ok(()) => {
                    let n = ids.len();
                    if let Err(e) = delete_ids(&state.db, &ids).await {
                        // The Hub already accepted them (and dedupes on event_id), so a re-send is
                        // safe - but back off so a persistent DB error can't hot-loop.
                        warn!(error = %e, "deleting forwarded scrobbles failed - backing off");
                        tokio::time::sleep(Duration::from_secs(BACKOFF_SECS)).await;
                    } else {
                        info!(count = n, "forwarded scrobbles to Hub");
                        // Loop immediately to drain any backlog; no sleep on a clean success.
                    }
                }
                Err(e) => {
                    warn!(error = %e, "forwarding scrobbles failed - retrying after backoff");
                    tokio::time::sleep(Duration::from_secs(BACKOFF_SECS)).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chordia_contracts::catalog::TrackFingerprint;
    use chordia_contracts::scrobble::{ClientType, PlaybackSource};
    use uuid::Uuid;

    async fn mem_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE pending_scrobbles (event_id TEXT PRIMARY KEY, payload TEXT NOT NULL, created_at INTEGER NOT NULL)",
        )
        .execute(&db)
        .await
        .unwrap();
        db
    }

    fn event() -> ListeningEvent {
        ListeningEvent {
            event_id: Uuid::now_v7(),
            fingerprint: TrackFingerprint {
                acoustid: None,
                recording_mbid: None,
                content_hash: "deadbeef".into(),
                artist_norm: "artist".into(),
                title_norm: "title".into(),
                album_norm: None,
                duration_ms: 200_000,
            },
            started_at: 1_700_000_000_000,
            ms_played: 180_000,
            duration_ms: 200_000,
            source: PlaybackSource::OwnLibrary,
            client_type: ClientType::Desktop,
            library_id: None,
            room_id: None,
        }
    }

    #[tokio::test]
    async fn enqueue_take_delete_roundtrip() {
        let db = mem_db().await;
        let a = event();
        let b = event();
        enqueue(&db, &a).await.unwrap();
        enqueue(&db, &b).await.unwrap();
        // Re-enqueue is idempotent (no duplicate row).
        enqueue(&db, &a).await.unwrap();

        let batch = take_batch(&db).await.unwrap();
        assert_eq!(batch.len(), 2, "two distinct events queued");

        delete_ids(&db, &[a.event_id.to_string()]).await.unwrap();
        let remaining = take_batch(&db).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1.event_id, b.event_id);
    }

    #[tokio::test]
    async fn poison_rows_are_dropped() {
        let db = mem_db().await;
        sqlx::query(
            "INSERT INTO pending_scrobbles (event_id, payload, created_at) VALUES (?, ?, ?)",
        )
        .bind("bad")
        .bind("{not valid json")
        .bind(1)
        .execute(&db)
        .await
        .unwrap();
        let batch = take_batch(&db).await.unwrap();
        assert!(batch.is_empty(), "unparseable rows are skipped");
        // And purged, so they don't wedge the queue.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_scrobbles")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
