//! Local SQLite index writes, content-addressed upsert.
//!
//! Every unique file is stored once (keyed by SHA-256 `content_hash`).
//! The same file appearing in multiple library folders creates multiple `file_paths` rows and
//! multiple `library_tracks` memberships, but only ONE `files` row and ONE `tracks` row.

use std::path::Path;

use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::AppResult;
use crate::metadata::ProbedTrack;

/// A file's `(mtime_ns, size_bytes)` for the freshness check, best-effort. Both are `None` on a
/// stat error, which makes the rescan treat the file as changed and re-index it (the safe default).
pub async fn file_freshness(path: &Path) -> (Option<i64>, Option<i64>) {
    match tokio::fs::metadata(path).await {
        Ok(m) => {
            let mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64);
            (mtime_ns, Some(m.len() as i64))
        }
        Err(_) => (None, None),
    }
}

/// Upsert an artist, deduped by normalized name; backfills the MusicBrainz id. Returns its id.
async fn upsert_artist(
    db: &SqlitePool,
    name: &str,
    name_norm: &str,
    mbid: Option<&str>,
) -> AppResult<String> {
    let id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO artists (id, name, name_normalized, mbid) VALUES (?,?,?,?) \
         ON CONFLICT(name_normalized) DO UPDATE SET mbid = COALESCE(artists.mbid, excluded.mbid)",
    )
    .bind(&id)
    .bind(name)
    .bind(name_norm)
    .bind(mbid)
    .execute(db)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT id FROM artists WHERE name_normalized = ?")
            .bind(name_norm)
            .fetch_one(db)
            .await?,
    )
}

/// Upsert an album, deduped by (normalized title, album-artist). Backfills album-level fields as
/// they become known (without clobbering values already set). Returns its id.
#[allow(clippy::too_many_arguments)]
async fn upsert_album(
    db: &SqlitePool,
    title: &str,
    title_norm: &str,
    artist_id: &str,
    year: Option<i64>,
    genre: Option<&str>,
    label: Option<&str>,
    total_tracks: Option<i64>,
    total_discs: Option<i64>,
    compilation: bool,
    release_mbid: Option<&str>,
    cover_hash: Option<&str>,
) -> AppResult<String> {
    let id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO albums \
           (id, title, title_normalized, artist_id, year, genre, label, total_tracks, total_discs, \
            compilation, release_mbid, cover_hash) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?) \
         ON CONFLICT(title_normalized, artist_id) DO UPDATE SET \
           year         = COALESCE(albums.year, excluded.year), \
           genre        = COALESCE(albums.genre, excluded.genre), \
           label        = COALESCE(albums.label, excluded.label), \
           total_tracks = COALESCE(albums.total_tracks, excluded.total_tracks), \
           total_discs  = COALESCE(albums.total_discs, excluded.total_discs), \
           compilation  = MAX(albums.compilation, excluded.compilation), \
           release_mbid = COALESCE(albums.release_mbid, excluded.release_mbid), \
           cover_hash   = COALESCE(albums.cover_hash, excluded.cover_hash)",
    )
    .bind(&id)
    .bind(title)
    .bind(title_norm)
    .bind(artist_id)
    .bind(year)
    .bind(genre)
    .bind(label)
    .bind(total_tracks)
    .bind(total_discs)
    .bind(compilation as i64)
    .bind(release_mbid)
    .bind(cover_hash)
    .execute(db)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT id FROM albums WHERE title_normalized = ? AND artist_id = ?")
            .bind(title_norm)
            .bind(artist_id)
            .fetch_one(db)
            .await?,
    )
}

/// Upsert a probed audio file into the content-addressed index.
///
/// 1. Upsert `files`: physical audio content, keyed by `content_hash`.
/// 2. Upsert `artists`/`albums`: canonical metadata entities (deduped), then `tracks` referencing
///    them by FK (one track row per unique `content_hash`; only track-specific fields live on it).
/// 3. Upsert `file_paths`: this filesystem path maps to a `content_hash`.
/// 4. Upsert `library_tracks`: library membership for the track.
///
/// Returns the track's UUID.
pub async fn upsert_track(
    db: &SqlitePool,
    library_id: &str,
    path: &Path,
    t: &ProbedTrack,
) -> AppResult<String> {
    let path_str = path.to_string_lossy();

    // Step 1: files
    sqlx::query(
        "INSERT INTO files (content_hash, codec, sample_rate_hz, bit_depth, channels, \
                            lossless, spatial, duration_ms) \
         VALUES (?,?,?,?,?,?,?,?) \
         ON CONFLICT(content_hash) DO UPDATE SET \
           codec=excluded.codec, sample_rate_hz=excluded.sample_rate_hz, \
           bit_depth=excluded.bit_depth, channels=excluded.channels, \
           lossless=excluded.lossless, spatial=excluded.spatial, duration_ms=excluded.duration_ms",
    )
    .bind(&t.content_hash)
    .bind(&t.codec)
    .bind(t.sample_rate_hz as i64)
    .bind(t.bit_depth as i64)
    .bind(t.channels as i64)
    .bind(t.lossless as i64)
    .bind(t.spatial as i64)
    .bind(t.duration_ms as i64)
    .execute(db)
    .await?;

    // Step 1b: cover_art (deduped embedded artwork)
    let cover_hash: Option<&str> = if let Some(cover) = &t.cover {
        sqlx::query("INSERT OR IGNORE INTO cover_art (hash, mime, bytes) VALUES (?,?,?)")
            .bind(&cover.hash)
            .bind(&cover.mime)
            .bind(&cover.data)
            .execute(db)
            .await?;
        Some(cover.hash.as_str())
    } else {
        None
    };

    // Step 2: artists + albums (canonical entities)
    // Track's primary artist (the raw credit tag, which the Hub splits into individual credits).
    let artist_id = upsert_artist(db, &t.artist, &t.artist_norm, t.mb_artist_id.as_deref()).await?;
    // Album (if any), attributed to the album-artist tag when present, else the track's artist.
    let album_id: Option<String> =
        if let Some(album_title) = t.album.as_deref().filter(|s| !s.trim().is_empty()) {
            let album_norm = t
                .album_norm
                .clone()
                .unwrap_or_else(|| crate::metadata::normalize(album_title));
            let album_artist_id = match t.album_artist.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(aa) => upsert_artist(db, aa, &crate::metadata::normalize(aa), None).await?,
                None => artist_id.clone(),
            };
            Some(
                upsert_album(
                    db,
                    album_title,
                    &album_norm,
                    &album_artist_id,
                    t.year.map(|y| y as i64),
                    t.genre.as_deref(),
                    t.label.as_deref(),
                    t.total_tracks.map(|n| n as i64),
                    t.total_discs.map(|n| n as i64),
                    t.compilation,
                    t.release_mbid.as_deref(),
                    cover_hash,
                )
                .await?,
            )
        } else {
            None
        };

    // Step 2b: tracks (track-specific fields + FK to artist/album)
    let existing_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM tracks WHERE content_hash = ?")
            .bind(&t.content_hash)
            .fetch_optional(db)
            .await?;

    let track_id = if let Some(id) = existing_id {
        // `acoustid` is not touched (owned by the fingerprint worker). `recording_mbid`/`track_no`/
        // `disc_no` are COALESCE-preserved: the AcoustID worker backfills them from the release, so a
        // re-index (e.g. after an organize rename) must not clobber those with the file's empty tags.
        // It may only fill them when still unset. Otherwise renaming an untagged FLAC wipes its
        // resolved recording id + track number and dedupe loses the key it needs.
        sqlx::query(
            "UPDATE tracks SET title=?,artist_id=?,album_id=?,\
             track_no=COALESCE(track_no, ?),disc_no=COALESCE(disc_no, ?),composer=?,\
             comment=?,isrc=?,bpm=?,lyrics=?,recording_mbid=COALESCE(recording_mbid, ?),\
             cover_hash=?,title_norm=?,duration_ms=? WHERE id=?",
        )
        .bind(&t.title)
        .bind(&artist_id)
        .bind(album_id.as_deref())
        .bind(t.track_no.map(|n| n as i64))
        .bind(t.disc_no.map(|n| n as i64))
        .bind(t.composer.as_deref())
        .bind(t.comment.as_deref())
        .bind(t.isrc.as_deref())
        .bind(t.bpm.map(|n| n as i64))
        .bind(t.lyrics.as_deref())
        .bind(t.recording_mbid.as_deref())
        .bind(cover_hash)
        .bind(&t.title_norm)
        .bind(t.duration_ms as i64)
        .bind(&id)
        .execute(db)
        .await?;
        id
    } else {
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO tracks \
             (id,content_hash,title,artist_id,album_id,track_no,disc_no,composer,comment,isrc,\
              bpm,lyrics,recording_mbid,cover_hash,title_norm,duration_ms) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&id)
        .bind(&t.content_hash)
        .bind(&t.title)
        .bind(&artist_id)
        .bind(album_id.as_deref())
        .bind(t.track_no.map(|n| n as i64))
        .bind(t.disc_no.map(|n| n as i64))
        .bind(t.composer.as_deref())
        .bind(t.comment.as_deref())
        .bind(t.isrc.as_deref())
        .bind(t.bpm.map(|n| n as i64))
        .bind(t.lyrics.as_deref())
        .bind(t.recording_mbid.as_deref())
        .bind(cover_hash)
        .bind(&t.title_norm)
        .bind(t.duration_ms as i64)
        .execute(db)
        .await?;
        id
    };

    // Step 3: file_paths
    // Record mtime+size so a periodic/startup rescan can skip unchanged files without re-hashing.
    let (mtime_ns, size_bytes) = file_freshness(path).await;
    sqlx::query(
        "INSERT INTO file_paths (id, content_hash, library_id, path, mtime_ns, size_bytes) \
         VALUES (?,?,?,?,?,?) \
         ON CONFLICT(path) DO UPDATE SET content_hash=excluded.content_hash, \
                                         library_id=excluded.library_id, \
                                         mtime_ns=excluded.mtime_ns, \
                                         size_bytes=excluded.size_bytes",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&t.content_hash)
    .bind(library_id)
    .bind(&*path_str)
    .bind(mtime_ns)
    .bind(size_bytes)
    .execute(db)
    .await?;

    // Step 4: library_tracks
    sqlx::query("INSERT OR IGNORE INTO library_tracks (library_id, track_id) VALUES (?,?)")
        .bind(library_id)
        .bind(&track_id)
        .execute(db)
        .await?;

    Ok(track_id)
}

/// Remove a path from the index.  Cleans up orphaned tracks and files if no other paths remain.
pub async fn remove_track(db: &SqlitePool, library_id: &str, path: &Path) -> AppResult<()> {
    let path_str = path.to_string_lossy();

    let row: Option<(String,)> =
        sqlx::query_as("SELECT content_hash FROM file_paths WHERE path = ? AND library_id = ?")
            .bind(&*path_str)
            .bind(library_id)
            .fetch_optional(db)
            .await?;

    let content_hash = match row {
        Some((h,)) => h,
        None => return Ok(()),
    };

    sqlx::query("DELETE FROM file_paths WHERE path = ?")
        .bind(&*path_str)
        .execute(db)
        .await?;

    let still_in_library: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_paths WHERE content_hash = ? AND library_id = ?",
    )
    .bind(&content_hash)
    .bind(library_id)
    .fetch_one(db)
    .await?;

    if still_in_library == 0 {
        if let Some(track_id) =
            sqlx::query_scalar::<_, String>("SELECT id FROM tracks WHERE content_hash = ?")
                .bind(&content_hash)
                .fetch_optional(db)
                .await?
        {
            sqlx::query("DELETE FROM library_tracks WHERE library_id = ? AND track_id = ?")
                .bind(library_id)
                .bind(&track_id)
                .execute(db)
                .await?;

            let any_membership: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM library_tracks WHERE track_id = ?")
                    .bind(&track_id)
                    .fetch_one(db)
                    .await?;

            if any_membership == 0 {
                sqlx::query("DELETE FROM tracks WHERE id = ?")
                    .bind(&track_id)
                    .execute(db)
                    .await?;
                // Intentionally keep the content-addressed `files` row (codec + ReplayGain loudness,
                // keyed by content_hash). It's invisible to track queries once orphaned, the loudness
                // worker skips it (no file_paths row to join), and if the same bytes are re-indexed
                // (e.g. an organize move that briefly removed then re-added the path, or the user
                // re-adding the file) the expensive analysis is preserved instead of recomputed.
            }
        }
    }

    Ok(())
}

/// Ensure a library row exists for the given name+path.  Returns its UUID.
pub async fn upsert_library(db: &SqlitePool, name: &str, path: &Path) -> AppResult<String> {
    let path_str = path.to_string_lossy();
    if let Some((id,)) = sqlx::query_as::<_, (String,)>("SELECT id FROM libraries WHERE path = ?")
        .bind(&*path_str)
        .fetch_optional(db)
        .await?
    {
        return Ok(id);
    }
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO libraries (id, name, path) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(name)
        .bind(&*path_str)
        .execute(db)
        .await?;
    Ok(id)
}
