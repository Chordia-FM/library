//! Scrobble buffering + forwarding (M5 / Phase A3).
//!
//! - **queue**    - a durable SQLite-backed queue (`pending_scrobbles`) of `ListeningEvent`s. The
//!   owner's arrive via the management-token `POST /v1/scrobbles` endpoint, so the Hub can safely
//!   attribute them to the server's owner. The Discord bot's arrive with the Discord id of the
//!   listener who heard them ([`enqueue_attributed`]); the Hub decides at send time whether that
//!   person is a Chordia user it may count them for.
//! - **reporter** - a background loop that flushes batches to the Hub (`POST /v1/scrobbles:ingest`
//!   for the owner's, `:ingest-attributed` for the bot's, both server-API-key authed) with retry +
//!   backoff; the Hub dedupes on `event_id`, so a re-send after a partial failure never
//!   double-counts. Rows are deleted only once the Hub acks, or once the Hub says the listener is
//!   nobody it may count for.

use std::sync::Arc;
use std::time::Duration;

use chordia_contracts::discord::{
    AttributedEvent, AttributedScrobbleBatch, ResolveListenersRequest,
};
use chordia_contracts::scrobble::{ListeningEvent, ScrobbleBatch};
use sqlx::SqlitePool;
use std::collections::HashMap;
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

/// Durably enqueue a play the Discord bot heard `discord_user_id` listen to.
pub async fn enqueue_attributed(
    db: &SqlitePool,
    event: &ListeningEvent,
    discord_user_id: u64,
) -> AppResult<()> {
    let payload = serde_json::to_string(event)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serializing scrobble: {e}")))?;
    sqlx::query(
        "INSERT OR IGNORE INTO pending_scrobbles (event_id, payload, created_at, discord_user_id) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(event.event_id.to_string())
    .bind(payload)
    .bind(now_millis())
    .bind(discord_user_id.to_string())
    .execute(db)
    .await?;
    Ok(())
}

/// One queued row: the owner's when `discord_user_id` is absent, a listener's otherwise.
struct Queued {
    id: String,
    event: ListeningEvent,
    discord_user_id: Option<String>,
}

/// Oldest queued events (up to `BATCH_SIZE`). Rows whose payload no longer parses against the
/// current contract are dropped so they can't wedge the queue forever.
async fn take_batch(db: &SqlitePool) -> AppResult<Vec<Queued>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT event_id, payload, discord_user_id FROM pending_scrobbles \
         ORDER BY created_at LIMIT ?",
    )
    .bind(BATCH_SIZE)
    .fetch_all(db)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    let mut poison = Vec::new();
    for (id, payload, discord_user_id) in rows {
        match serde_json::from_str::<ListeningEvent>(&payload) {
            Ok(event) => out.push(Queued {
                id,
                event,
                discord_user_id,
            }),
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

            let (owner, listened): (Vec<Queued>, Vec<Queued>) =
                batch.into_iter().partition(|q| q.discord_user_id.is_none());

            let mut acked: Vec<String> = Vec::new();
            let mut failed = false;
            if !owner.is_empty() {
                let ids: Vec<String> = owner.iter().map(|q| q.id.clone()).collect();
                let payload = ScrobbleBatch {
                    events: owner.into_iter().map(|q| q.event).collect(),
                };
                match hub.forward_scrobbles(&api_key, &payload).await {
                    Ok(()) => acked.extend(ids),
                    Err(e) => {
                        warn!(error = %e, "forwarding scrobbles failed - retrying after backoff");
                        failed = true;
                    }
                }
            }
            if !listened.is_empty() && !failed {
                match forward_attributed(&hub, &api_key, listened).await {
                    Ok(done) => acked.extend(done),
                    Err(e) => {
                        warn!(error = %e, "forwarding listeners' scrobbles failed - retrying after backoff");
                        failed = true;
                    }
                }
            }

            let n = acked.len();
            if let Err(e) = delete_ids(&state.db, &acked).await {
                // The Hub already accepted them (and dedupes on event_id), so a re-send is safe -
                // but back off so a persistent DB error can't hot-loop.
                warn!(error = %e, "deleting forwarded scrobbles failed - backing off");
                tokio::time::sleep(Duration::from_secs(BACKOFF_SECS)).await;
            } else if failed {
                tokio::time::sleep(Duration::from_secs(BACKOFF_SECS)).await;
            } else if n > 0 {
                info!(count = n, "forwarded scrobbles to Hub");
                // Loop immediately to drain any backlog; no sleep on a clean success.
            }
        }
    });
}

/// Send the bot's plays: the listeners' Discord ids become Hub users through the Hub's own trust
/// rule, and a play whose listener the Hub will not count for is done with (deleted, not resent).
/// Returns the row ids that are finished either way.
async fn forward_attributed(
    hub: &HubClient,
    api_key: &str,
    rows: Vec<Queued>,
) -> anyhow::Result<Vec<String>> {
    let mut discord_ids: Vec<String> = rows
        .iter()
        .filter_map(|q| q.discord_user_id.clone())
        .collect();
    discord_ids.sort();
    discord_ids.dedup();
    let resolved = hub
        .resolve_listeners(api_key, &ResolveListenersRequest { discord_ids })
        .await?;
    let users: HashMap<String, uuid::Uuid> = resolved
        .listeners
        .into_iter()
        .map(|l| (l.discord_id, l.user_id))
        .collect();

    let mut done: Vec<String> = Vec::new();
    let mut events: Vec<AttributedEvent> = Vec::new();
    let mut sent_ids: Vec<String> = Vec::new();
    for q in rows {
        match q.discord_user_id.as_deref().and_then(|d| users.get(d)) {
            Some(user_id) => {
                sent_ids.push(q.id);
                events.push(AttributedEvent {
                    user_id: *user_id,
                    event: q.event,
                });
            }
            // Not a Chordia user this server may count for (or they opted out): nothing to keep.
            None => done.push(q.id),
        }
    }
    if !events.is_empty() {
        hub.forward_attributed_scrobbles(api_key, &AttributedScrobbleBatch { events })
            .await?;
        done.extend(sent_ids);
    }
    Ok(done)
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
            "CREATE TABLE pending_scrobbles (event_id TEXT PRIMARY KEY, payload TEXT NOT NULL, created_at INTEGER NOT NULL, discord_user_id TEXT)",
        )
        .execute(&db)
        .await
        .unwrap();
        db
    }

    fn event() -> ListeningEvent {
        ListeningEvent {
            event_id: Uuid::now_v7(),
            // Added to the contract after this fixture was written, which broke the bin's test
            // target — `cargo check` never builds it, so the breakage was invisible to the usual gate.
            title: Some("title".into()),
            artist: Some("artist".into()),
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
            playlist_id: None,
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
        assert_eq!(remaining[0].event.event_id, b.event_id);
        assert!(remaining[0].discord_user_id.is_none(), "the owner's play");
    }

    #[tokio::test]
    async fn a_listeners_play_keeps_who_heard_it() {
        let db = mem_db().await;
        let heard = event();
        enqueue_attributed(&db, &heard, 424242).await.unwrap();
        enqueue(&db, &event()).await.unwrap();
        let batch = take_batch(&db).await.unwrap();
        let theirs = batch
            .iter()
            .find(|q| q.event.event_id == heard.event_id)
            .unwrap();
        assert_eq!(theirs.discord_user_id.as_deref(), Some("424242"));
        assert_eq!(
            batch.iter().filter(|q| q.discord_user_id.is_none()).count(),
            1
        );
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
