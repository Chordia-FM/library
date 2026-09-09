//! Text search over the local catalog.
//!
//! The index has never needed one: every lookup so far was an exact key (hash, AcoustID, MBID,
//! normalized artist + title). A Discord `/play daft punk one more` is the first free-text query this
//! server answers, and it has to answer within Discord's three-second interaction window.
//!
//! The approach is deliberately the simplest thing that is fast enough: `LIKE '%term%'` over the
//! `*_norm` columns the indexer already maintains, then ranking in Rust. A personal library is tens of
//! thousands of tracks, and a full scan of three short text columns at that size is a few tens of
//! milliseconds in SQLite — well under the budget, with nothing to keep in sync. FTS5 would be the
//! next step if a library ever outgrows this, and it slots in behind the same [`search`] signature.
//!
//! The query is normalized with [`crate::metadata::normalize`], the same function that produced the
//! stored keys, so "Daft Punk" and "daft-punk" hit the same row.

use sqlx::{AssertSqlSafe, SqlitePool};

use crate::catalog::{TrackRow, TRACK_COLS_NO_LIB, TRACK_JOINS};
use crate::error::AppResult;
use crate::metadata::normalize;

/// What kind of catalog entity a hit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitKind {
    Track,
    Album,
    Artist,
    /// A Chordia playlist on the Hub; never a search hit here, but what `/playlist` queues.
    Playlist,
}

/// One search result, shaped for a picker: a title line, a subtitle line, and the ids and facts a
/// caller needs to act on it without a second query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub kind: HitKind,
    pub id: String,
    /// Track title, album title, or artist name.
    pub title: String,
    /// Artist for a track; artist · year for an album; "N tracks" for an artist.
    pub subtitle: String,
    /// Track: its duration. Album: total duration. Artist: none.
    pub duration_ms: Option<i64>,
    pub cover_hash: Option<String>,
    /// Album/artist: how many tracks playing it would queue. Track: 1.
    pub track_count: i64,
    /// Ranking score; higher is better. Only meaningful relative to other hits of the same query.
    pub score: u32,
}

/// How many candidate rows each entity query may return before ranking. Enough that the right
/// answer is in the set for any sane query; small enough that ranking is free.
const CANDIDATES: i64 = 200;

/// Search tracks, albums and/or artists for `query`. Results are ranked across kinds and truncated to
/// `limit`. An empty (or all-punctuation) query returns nothing.
pub async fn search(
    db: &SqlitePool,
    query: &str,
    kinds: &[HitKind],
    limit: usize,
) -> AppResult<Vec<SearchHit>> {
    let q = normalize(query);
    let terms: Vec<&str> = q.split(' ').filter(|t| !t.is_empty()).collect();
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let patterns: Vec<String> = terms.iter().map(|t| like_pattern(t)).collect();

    let mut hits: Vec<SearchHit> = Vec::new();
    if kinds.contains(&HitKind::Track) {
        hits.extend(search_tracks(db, &q, &terms, &patterns).await?);
    }
    if kinds.contains(&HitKind::Album) {
        hits.extend(search_albums(db, &q, &terms, &patterns).await?);
    }
    if kinds.contains(&HitKind::Artist) {
        hits.extend(search_artists(db, &q, &terms, &patterns).await?);
    }
    // Stable: equal scores keep query order (tracks, albums, artists), which is also the order a
    // listener most often means.
    hits.sort_by_key(|h| std::cmp::Reverse(h.score));
    hits.truncate(limit);
    Ok(hits)
}

/// `%term%` with SQL LIKE wildcards in the term itself escaped (`ESCAPE '\'`). `normalize` already
/// strips punctuation, so this only matters for a query that somehow carries `%`/`_`/`\`.
fn like_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// `WHERE (expr) LIKE ? ESCAPE '\' AND (expr) LIKE ? ESCAPE '\' ...`, one clause per term.
fn where_all_terms(expr: &str, n: usize) -> String {
    let clause = format!("({expr}) LIKE ? ESCAPE '\\'");
    let clauses: Vec<&str> = std::iter::repeat_n(clause.as_str(), n).collect();
    clauses.join(" AND ")
}

/// Rank one candidate. `primary` is the field the hit is named by (track title, album title, artist
/// name); `secondary` is the supporting field (the artist for tracks/albums). Scores are bands so
/// that an exact primary match always beats a prefix, a prefix beats a word-start, and so on; the
/// length term only breaks ties inside a band, favouring the shortest primary (the closest match).
fn score(query: &str, terms: &[&str], primary: &str, secondary: &str) -> u32 {
    let base: u32 = if primary == query {
        100
    } else if primary.starts_with(query) {
        80
    } else if primary.contains(query)
        && primary
            .find(query)
            .map(|i| i == 0 || primary.as_bytes()[i - 1] == b' ')
            .unwrap_or(false)
    {
        60
    } else if terms.iter().all(|t| primary.contains(t)) {
        40
    } else {
        20
    };
    let artist_bonus: u32 = if !secondary.is_empty() && terms.iter().any(|t| secondary.contains(t))
    {
        5
    } else {
        0
    };
    let extra = primary.len().saturating_sub(query.len()).min(999) as u32;
    (base + artist_bonus) * 1000 + (999 - extra)
}

#[derive(sqlx::FromRow)]
struct TrackHitRow {
    id: String,
    title: String,
    artist: String,
    duration_ms: i64,
    cover_hash: Option<String>,
    title_norm: String,
    artist_norm: String,
}

async fn search_tracks(
    db: &SqlitePool,
    q: &str,
    terms: &[&str],
    patterns: &[String],
) -> AppResult<Vec<SearchHit>> {
    let sql = format!(
        "SELECT t.id, t.title, COALESCE(ar.name, '') AS artist, t.duration_ms, \
                COALESCE(t.cover_hash, al.cover_hash) AS cover_hash, \
                t.title_norm, COALESCE(ar.name_normalized, '') AS artist_norm \
         FROM tracks t \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         WHERE {} LIMIT {CANDIDATES}",
        where_all_terms(
            "t.title_norm || ' ' || COALESCE(ar.name_normalized, '') || ' ' || \
             COALESCE(al.title_normalized, '')",
            patterns.len(),
        )
    );
    let mut query = sqlx::query_as::<_, TrackHitRow>(AssertSqlSafe(sql));
    for p in patterns {
        query = query.bind(p);
    }
    let rows = query.fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| SearchHit {
            score: score(q, terms, &r.title_norm, &r.artist_norm),
            kind: HitKind::Track,
            id: r.id,
            title: r.title,
            subtitle: r.artist,
            duration_ms: Some(r.duration_ms),
            cover_hash: r.cover_hash,
            track_count: 1,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct AlbumHitRow {
    id: String,
    title: String,
    artist: String,
    year: Option<i64>,
    cover_hash: Option<String>,
    track_count: i64,
    duration_ms: Option<i64>,
    title_norm: String,
    artist_norm: String,
}

async fn search_albums(
    db: &SqlitePool,
    q: &str,
    terms: &[&str],
    patterns: &[String],
) -> AppResult<Vec<SearchHit>> {
    let sql = format!(
        "SELECT al.id, al.title, COALESCE(aa.name, '') AS artist, al.year, al.cover_hash, \
                COUNT(t.id) AS track_count, SUM(t.duration_ms) AS duration_ms, \
                COALESCE(al.title_normalized, '') AS title_norm, \
                COALESCE(aa.name_normalized, '') AS artist_norm \
         FROM albums al \
         LEFT JOIN artists aa ON aa.id = al.artist_id \
         JOIN tracks t ON t.album_id = al.id \
         WHERE {} \
         GROUP BY al.id LIMIT {CANDIDATES}",
        where_all_terms(
            "COALESCE(al.title_normalized, '') || ' ' || COALESCE(aa.name_normalized, '')",
            patterns.len(),
        )
    );
    let mut query = sqlx::query_as::<_, AlbumHitRow>(AssertSqlSafe(sql));
    for p in patterns {
        query = query.bind(p);
    }
    let rows = query.fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| SearchHit {
            score: score(q, terms, &r.title_norm, &r.artist_norm),
            kind: HitKind::Album,
            id: r.id,
            title: r.title,
            subtitle: match r.year {
                Some(y) if !r.artist.is_empty() => format!("{} · {y}", r.artist),
                Some(y) => y.to_string(),
                None => r.artist,
            },
            duration_ms: r.duration_ms,
            cover_hash: r.cover_hash,
            track_count: r.track_count,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct ArtistHitRow {
    id: String,
    name: String,
    name_norm: String,
    track_count: i64,
}

async fn search_artists(
    db: &SqlitePool,
    q: &str,
    terms: &[&str],
    patterns: &[String],
) -> AppResult<Vec<SearchHit>> {
    // An artist counts the tracks credited to them directly plus the tracks on their albums — the
    // same set `artist_tracks` plays — and an artist with none of either is not offered.
    let sql = format!(
        "SELECT ar.id, ar.name, COALESCE(ar.name_normalized, '') AS name_norm, \
                (SELECT COUNT(*) FROM tracks t LEFT JOIN albums al ON al.id = t.album_id \
                 WHERE t.artist_id = ar.id OR al.artist_id = ar.id) AS track_count \
         FROM artists ar \
         WHERE {} AND track_count > 0 LIMIT {CANDIDATES}",
        where_all_terms("COALESCE(ar.name_normalized, '')", patterns.len())
    );
    let mut query = sqlx::query_as::<_, ArtistHitRow>(AssertSqlSafe(sql));
    for p in patterns {
        query = query.bind(p);
    }
    let rows = query.fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| SearchHit {
            score: score(q, terms, &r.name_norm, ""),
            kind: HitKind::Artist,
            id: r.id,
            title: r.name,
            subtitle: format!("{} tracks", r.track_count),
            duration_ms: None,
            cover_hash: None,
            track_count: r.track_count,
        })
        .collect())
}

/// Every track on an album, in disc/track order — what `/play` queues for an album hit.
pub async fn album_tracks(db: &SqlitePool, album_id: &str) -> AppResult<Vec<TrackRow>> {
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} FROM tracks t {TRACK_JOINS} \
         WHERE t.album_id = ? \
         ORDER BY t.disc_no, t.track_no, t.title_norm"
    );
    Ok(sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(album_id)
        .fetch_all(db)
        .await?)
}

/// Tracks credited to an artist or on one of their albums, album by album — what `/play` queues for
/// an artist hit. Capped because a discography can be enormous.
pub async fn artist_tracks(
    db: &SqlitePool,
    artist_id: &str,
    limit: i64,
) -> AppResult<Vec<TrackRow>> {
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} FROM tracks t {TRACK_JOINS} \
         WHERE t.artist_id = ? OR al.artist_id = ? \
         ORDER BY al.year, al.title_normalized, t.disc_no, t.track_no \
         LIMIT ?"
    );
    Ok(sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(artist_id)
        .bind(artist_id)
        .bind(limit)
        .fetch_all(db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("abc"), "%abc%");
        assert_eq!(like_pattern("a%b_c\\"), "%a\\%b\\_c\\\\%");
    }

    #[test]
    fn where_clause_has_one_like_per_term() {
        let w = where_all_terms("x", 3);
        assert_eq!(w.matches("LIKE ?").count(), 3);
        assert_eq!(w.matches(" AND ").count(), 2);
    }

    #[test]
    fn score_bands_are_ordered() {
        let q = "one more time";
        let terms = ["one", "more", "time"];
        let exact = score(q, &terms, "one more time", "");
        let prefix = score(q, &terms, "one more time radio edit", "");
        let word = score(q, &terms, "daft punk one more time", "");
        let all_terms = score(q, &terms, "time one more", "");
        let secondary_only = score(q, &terms, "digital love", "one more time");
        assert!(exact > prefix && prefix > word && word > all_terms && all_terms > secondary_only);
    }

    #[test]
    fn score_prefers_shorter_within_a_band() {
        let terms = ["one"];
        assert!(score("one", &terms, "one more", "") > score("one", &terms, "one more time", ""));
    }

    #[test]
    fn score_artist_match_is_a_bonus_not_a_band() {
        let terms = ["daft", "one"];
        let with_artist = score("daft one", &terms, "one more time", "daft punk");
        let without = score("daft one", &terms, "one more time", "nobody");
        assert!(with_artist > without);
        // Still below the next band up.
        assert!(with_artist < score("daft one", &terms, "daft one", ""));
    }
}
