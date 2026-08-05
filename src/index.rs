//! Local SQLite index writes, content-addressed upsert.
//!
//! Every unique file is stored once (keyed by SHA-256 `content_hash`).
//! The same file appearing in multiple library folders creates multiple `file_paths` rows and
//! multiple `library_tracks` memberships, but only ONE `files` row and ONE `tracks` row.

use std::path::Path;

use sqlx::{SqliteConnection, SqlitePool};
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
/// Takes a connection (not the pool) so it can run inside `upsert_track`'s per-file transaction.
async fn upsert_artist(
    db: &mut SqliteConnection,
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
    .execute(&mut *db)
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
    db: &mut SqliteConnection,
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
    .execute(&mut *db)
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

    // One transaction per file: indexing a file is 6-8 statements, and autocommitting each one
    // paid a write lock + WAL append apiece and let a crash publish a partially-indexed file.
    // A single commit keeps the file atomic and makes bulk scans markedly cheaper.
    let mut tx = db.begin().await?;

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
    .execute(&mut *tx)
    .await?;

    // Step 1b: cover_art (deduped embedded artwork)
    let cover_hash: Option<&str> = if let Some(cover) = &t.cover {
        sqlx::query("INSERT OR IGNORE INTO cover_art (hash, mime, bytes) VALUES (?,?,?)")
            .bind(&cover.hash)
            .bind(&cover.mime)
            .bind(&cover.data)
            .execute(&mut *tx)
            .await?;
        Some(cover.hash.as_str())
    } else {
        None
    };

    // Step 2: artists + albums (canonical entities)
    // Track's primary artist (the raw credit tag, which the Hub splits into individual credits).
    let artist_id = upsert_artist(
        &mut tx,
        &t.artist,
        &t.artist_norm,
        t.mb_artist_id.as_deref(),
    )
    .await?;
    // Album (if any), attributed to the album-artist tag when present, else the track's artist.
    // A deluxe/special/expanded edition folds into its BASE album (the edition is kept on the track),
    // so "X" and "X (Deluxe)" share one album row and the deluxe extras sit alongside the originals.
    let mut track_edition: Option<String> = None;
    let album_id: Option<String> =
        if let Some(album_title) = t.album.as_deref().filter(|s| !s.trim().is_empty()) {
            let (base_title, edition) = crate::metadata::parse_edition(album_title);
            track_edition = edition;
            let album_norm = crate::metadata::normalize(&base_title);
            // Own the album by its PRIMARY artist (first credit), matching how the Hub attributes it,
            // so a collab like "Blog Era Boyz" credited "mgk & Wiz Khalifa" files under mgk, not whoever
            // a given file's ALBUMARTIST tag happens to name. We take the album-artist tag when present,
            // else the track credit, and reduce either to its first artist via `primary_artist`.
            let album_artist_raw = t
                .album_artist
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(t.artist.as_str());
            let album_artist = chordia_contracts::artists::primary_artist(album_artist_raw);
            let album_artist_id = upsert_artist(
                &mut tx,
                &album_artist,
                &crate::metadata::normalize(&album_artist),
                None,
            )
            .await?;
            Some(
                upsert_album(
                    &mut tx,
                    &base_title,
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
            .fetch_optional(&mut *tx)
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
             cover_hash=?,title_norm=?,duration_ms=?,edition=?,advisory=? WHERE id=?",
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
        .bind(track_edition.as_deref())
        .bind(t.advisory.as_deref())
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        id
    } else {
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO tracks \
             (id,content_hash,title,artist_id,album_id,track_no,disc_no,composer,comment,isrc,\
              bpm,lyrics,recording_mbid,cover_hash,title_norm,duration_ms,edition,advisory) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
        .bind(track_edition.as_deref())
        .bind(t.advisory.as_deref())
        .execute(&mut *tx)
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
    .execute(&mut *tx)
    .await?;

    // Step 4: library_tracks
    sqlx::query("INSERT OR IGNORE INTO library_tracks (library_id, track_id) VALUES (?,?)")
        .bind(library_id)
        .bind(&track_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
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

/// Attach a track to an album resolved by acoustic fingerprint.
///
/// The identification worker already learns the album, its release MBID and its year from the
/// AcoustID/MusicBrainz match, and its own doc calls the track/disc numbers "the ones untagged files
/// lack". It then wrote those back only `if let Some(album_id) = &r.album_id` — i.e. only when the
/// track ALREADY had an album, which is precisely the case that does not need them. A tagless rip,
/// the whole reason fingerprinting exists, had its resolved album thrown away and stayed orphaned.
///
/// Returns the album id when one was created or matched, `None` when there was nothing to attach to.
/// Uses the same `upsert_album` as the tag path, so an album the fingerprint names and one the tags
/// name converge on a single row via `albums_title_artist_uniq` rather than becoming duplicates.
pub(crate) async fn attach_album_from_identity(
    db: &SqlitePool,
    track_id: &str,
    artist_id: &str,
    title: &str,
    year: Option<i64>,
    release_mbid: Option<&str>,
) -> AppResult<Option<String>> {
    let title = title.trim();
    if title.is_empty() {
        return Ok(None);
    }
    let mut conn = db.acquire().await?;
    let album_id = upsert_album(
        &mut conn,
        title,
        &crate::metadata::normalize(title),
        artist_id,
        year,
        None,
        None,
        None,
        None,
        false,
        release_mbid,
        None,
    )
    .await?;

    // COALESCE, not an overwrite: a track that gained a real ALBUM tag between the fingerprint being
    // queued and this write must keep the tag's answer. The fingerprint is a fallback for files that
    // say nothing, not an authority over files that do.
    sqlx::query("UPDATE tracks SET album_id = COALESCE(album_id, ?) WHERE id = ?")
        .bind(&album_id)
        .bind(track_id)
        .execute(db)
        .await?;
    Ok(Some(album_id))
}

#[cfg(test)]
mod attach_album_tests {
    use super::*;

    async fn db_with_schema() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for ddl in [
            "CREATE TABLE artists (id TEXT PRIMARY KEY, name TEXT NOT NULL, \
             name_normalized TEXT NOT NULL)",
            "CREATE TABLE albums (id TEXT PRIMARY KEY, title TEXT NOT NULL, \
             title_normalized TEXT NOT NULL, artist_id TEXT NOT NULL, year INTEGER, genre TEXT, \
             label TEXT, total_tracks INTEGER, total_discs INTEGER, \
             compilation INTEGER NOT NULL DEFAULT 0, release_mbid TEXT, cover_hash TEXT)",
            "CREATE UNIQUE INDEX albums_title_artist_uniq ON albums(title_normalized, artist_id)",
            "CREATE TABLE tracks (id TEXT PRIMARY KEY, album_id TEXT)",
            "INSERT INTO artists VALUES ('art1', 'Charlie Puth', 'charlie puth')",
        ] {
            sqlx::query(ddl).execute(&db).await.unwrap();
        }
        db
    }

    async fn album_of(db: &SqlitePool, track: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>("SELECT album_id FROM tracks WHERE id = ?")
            .bind(track)
            .fetch_one(db)
            .await
            .unwrap()
    }

    /// The reported case: a tagless track whose album the fingerprint resolved.
    #[tokio::test]
    async fn an_orphaned_track_gets_the_identified_album() {
        let db = db_with_schema().await;
        sqlx::query("INSERT INTO tracks VALUES ('t1', NULL)")
            .execute(&db)
            .await
            .unwrap();

        let id = attach_album_from_identity(&db, "t1", "art1", "Voicenotes", Some(2018), None)
            .await
            .unwrap();

        assert!(
            id.is_some(),
            "no album was created for the identified track"
        );
        assert_eq!(album_of(&db, "t1").await, id, "the track was not attached");
    }

    /// A track that already has an album keeps it: the fingerprint is a fallback for files that say
    /// nothing, never an authority over files that do.
    #[tokio::test]
    async fn an_existing_album_is_not_overwritten() {
        let db = db_with_schema().await;
        sqlx::query("INSERT INTO tracks VALUES ('t2', 'already-here')")
            .execute(&db)
            .await
            .unwrap();

        attach_album_from_identity(&db, "t2", "art1", "Voicenotes", None, None)
            .await
            .unwrap();

        assert_eq!(
            album_of(&db, "t2").await,
            Some("already-here".to_string()),
            "the fingerprint overwrote an album the tags had already established"
        );
    }

    /// Tag-derived and fingerprint-derived albums must converge on one row, not duplicate.
    #[tokio::test]
    async fn the_same_album_from_two_sources_is_one_row() {
        let db = db_with_schema().await;
        for t in ["t3", "t4"] {
            sqlx::query("INSERT INTO tracks VALUES (?, NULL)")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
        }

        let a = attach_album_from_identity(&db, "t3", "art1", "Voicenotes", None, None)
            .await
            .unwrap();
        let b = attach_album_from_identity(&db, "t4", "art1", "voicenotes", None, None)
            .await
            .unwrap();

        assert_eq!(a, b, "the same album under different casing made two rows");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM albums")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1, "expected one album row, found {n}");
    }
}
