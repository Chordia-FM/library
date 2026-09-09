//! What autocomplete offers before anything is typed: what the asker requested here lately,
//! then what this server plays most, then what is newest in the library; up to Discord's
//! twenty-five.

use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::search::{self, HitKind, SearchHit};

/// How many of each pool are looked at before the room is shared out.
const POOL: i64 = 100;

/// Suggestions for `user` in this guild, of these kinds, at most `limit`. Several kinds share the
/// room tracks-first: three fifths tracks, a fifth albums, the rest artists.
pub async fn suggest(
    db: &SqlitePool,
    app_id: &str,
    guild_id: &str,
    user: u64,
    kinds: &[HitKind],
    limit: usize,
) -> AppResult<Vec<SearchHit>> {
    let mut ids: Vec<String> = Vec::new();
    for pool in [
        mine(db, app_id, guild_id, user).await?,
        popular(db, app_id, guild_id).await?,
        newest(db).await?,
    ] {
        for id in pool {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let (tracks, albums, artists) = if kinds.len() == 1 {
        (limit, limit, limit)
    } else {
        let t = limit * 3 / 5;
        let a = limit / 5;
        (t, a, limit - t - a)
    };
    let mut out = Vec::new();
    if kinds.contains(&HitKind::Track) {
        out.extend(search::track_hits(db, &ids).await?.into_iter().take(tracks));
    }
    if kinds.contains(&HitKind::Album) || kinds.contains(&HitKind::Artist) {
        let owners = search::owners_of(db, &ids).await?;
        if kinds.contains(&HitKind::Album) {
            let album_ids = distinct(owners.iter().filter_map(|(_, al, _)| al.clone()));
            out.extend(
                search::album_hits(db, &album_ids)
                    .await?
                    .into_iter()
                    .take(albums),
            );
        }
        if kinds.contains(&HitKind::Artist) {
            let artist_ids = distinct(owners.iter().filter_map(|(_, _, ar)| ar.clone()));
            out.extend(
                search::artist_hits(db, &artist_ids)
                    .await?
                    .into_iter()
                    .take(artists),
            );
        }
    }
    out.truncate(limit);
    Ok(out)
}

fn distinct(ids: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// What `user` asked for here, newest first, each track once.
async fn mine(db: &SqlitePool, app_id: &str, guild_id: &str, user: u64) -> AppResult<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT track_id FROM discord_plays \
         WHERE app_id = ? AND guild_id = ? AND requested_by = ? \
         ORDER BY started_at DESC LIMIT ?",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(user.to_string())
    .bind(POOL)
    .fetch_all(db)
    .await?;
    Ok(distinct(rows.into_iter()))
}

/// What this server plays most, the most-played first and the latest of a tie first.
async fn popular(db: &SqlitePool, app_id: &str, guild_id: &str) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT track_id FROM discord_plays WHERE app_id = ? AND guild_id = ? \
         GROUP BY track_id ORDER BY COUNT(*) DESC, MAX(started_at) DESC LIMIT ?",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(POOL)
    .fetch_all(db)
    .await?)
}

/// What the library indexed last.
async fn newest(db: &SqlitePool) -> AppResult<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM tracks ORDER BY created_at DESC, rowid DESC LIMIT ?")
            .bind(POOL)
            .fetch_all(db)
            .await?,
    )
}
