//! Radio: the next track when the queue runs out and autoplay is on.
//!
//! Nearest first: another track by the same artist, then by the album's artist, then in the same
//! genre, then anything. Recently played tracks (the player's history) are skipped so a small
//! library does not loop the same three songs; when even that leaves nothing, the exclusion is
//! dropped rather than the music.

use sqlx::SqlitePool;

use crate::catalog::{self, TrackRow};

/// The most history entries used as an exclusion list.
const EXCLUDE_CAP: usize = 50;

pub async fn pick(db: &SqlitePool, seed: &TrackRow, exclude: &[String]) -> Option<TrackRow> {
    let exclude: Vec<&str> = exclude
        .iter()
        .rev()
        .take(EXCLUDE_CAP)
        .map(String::as_str)
        .chain(std::iter::once(seed.id.as_str()))
        .collect();
    let ladder: [(&str, Option<&str>); 4] = [
        ("ar.name_normalized = ?", Some(seed.artist_norm.as_str())),
        (
            "aa.name_normalized = ?",
            seed.album_artist
                .as_deref()
                .map(|_| seed.artist_norm.as_str()),
        ),
        ("al.genre = ?", seed.genre.as_deref()),
        ("1 = 1", None),
    ];
    for (clause, bind) in ladder {
        if let Some(id) = candidate(db, clause, bind, &exclude).await {
            if let Ok(Some(track)) = catalog::get_track_row(db, &id).await {
                return Some(track);
            }
        }
    }
    // Everything is in recent history: any track other than the one that just ended.
    let id = candidate(db, "1 = 1", None, &[seed.id.as_str()]).await?;
    catalog::get_track_row(db, &id).await.ok().flatten()
}

/// One random track id matching `clause`, not among `exclude`. A track counts only when a library
/// still holds it.
async fn candidate(
    db: &SqlitePool,
    clause: &str,
    bind: Option<&str>,
    exclude: &[&str],
) -> Option<String> {
    let needs_bind = clause.contains('?');
    if needs_bind && bind.is_none() {
        return None;
    }
    let placeholders = std::iter::repeat_n("?", exclude.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT t.id FROM tracks t \
         JOIN library_tracks lt ON lt.track_id = t.id \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         LEFT JOIN artists aa ON aa.id = al.artist_id \
         WHERE {clause} AND t.id NOT IN ({placeholders}) \
         ORDER BY RANDOM() LIMIT 1"
    );
    let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
    if let Some(b) = bind {
        q = q.bind(b.to_string());
    }
    for id in exclude {
        q = q.bind(id.to_string());
    }
    q.fetch_optional(db).await.ok().flatten()
}
