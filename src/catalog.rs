//! Catalog queries - list libraries / tracks from the content-addressed SQLite index.

use chordia_contracts::catalog::{AudioProperties, Track, TrackFingerprint};
use sqlx::{AssertSqlSafe, SqlitePool};
use uuid::Uuid;

use crate::error::AppResult;

/// Full track row, assembled by joining tracks + files (+ optionally library_tracks).
#[derive(sqlx::FromRow)]
pub struct TrackRow {
    pub id: String,
    /// The library this track was fetched in context of (may be empty string for match queries).
    pub library_id: String,
    pub content_hash: String,
    pub title: String,
    pub artist: String,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub duration_ms: i64,
    pub acoustid: Option<String>,
    pub recording_mbid: Option<String>,
    pub artist_norm: String,
    pub title_norm: String,
    pub album_norm: Option<String>,
    // from files
    pub codec: String,
    pub sample_rate_hz: i64,
    pub bit_depth: i64,
    pub channels: i64,
    pub lossless: i64,
    pub spatial: i64,
    pub rg_gain_db: Option<f64>,
    pub rg_peak: Option<f64>,
}

impl TrackRow {
    pub fn into_contract(self) -> Track {
        Track {
            id: parse_uuid(&self.id),
            library_id: parse_uuid(&self.library_id),
            title: self.title,
            artist: self.artist,
            album_artist: self.album_artist,
            album: self.album,
            year: self.year.map(|y| y as u16),
            genre: self.genre,
            track_no: self.track_no.map(|n| n as u16),
            disc_no: self.disc_no.map(|n| n as u16),
            duration_ms: self.duration_ms as u32,
            audio: AudioProperties {
                codec: self.codec,
                sample_rate_hz: self.sample_rate_hz as u32,
                bit_depth: self.bit_depth as u8,
                channels: self.channels as u8,
                lossless: self.lossless != 0,
                spatial: self.spatial != 0,
                gain_db: self.rg_gain_db.map(|v| v as f32),
                peak: self.rg_peak.map(|v| v as f32),
            },
            fingerprint: TrackFingerprint {
                acoustid: self.acoustid,
                recording_mbid: self.recording_mbid,
                content_hash: self.content_hash,
                artist_norm: self.artist_norm,
                title_norm: self.title_norm,
                album_norm: self.album_norm,
                duration_ms: self.duration_ms as u32,
            },
        }
    }
}

fn parse_uuid(s: &str) -> Uuid {
    s.parse().unwrap_or(Uuid::nil())
}

// Artist/album fields are now reached by FK join: `ar` = the track's primary artist, `al` = its
// album, `aa` = the album's artist. The denormalized `tracks.artist/album/...` columns are gone.

/// Joins that hydrate the artist/album fields + file facts. Goes after `FROM tracks t` (or after the
/// `library_tracks`→`tracks` join).
const TRACK_JOINS: &str = "JOIN files f ON f.content_hash = t.content_hash \
     LEFT JOIN artists ar ON ar.id = t.artist_id \
     LEFT JOIN albums al ON al.id = t.album_id \
     LEFT JOIN artists aa ON aa.id = al.artist_id";

/// SQL fragment shared by all "with library context" track queries.
const TRACK_COLS_WITH_LIB: &str =
    "t.id, lt.library_id, t.content_hash, t.title, COALESCE(ar.name, '') AS artist, \
     aa.name AS album_artist, al.title AS album, al.year AS year, al.genre AS genre, \
     t.track_no, t.disc_no, t.duration_ms, t.acoustid, t.recording_mbid, \
     COALESCE(ar.name_normalized, '') AS artist_norm, t.title_norm, al.title_normalized AS album_norm, \
     f.codec, f.sample_rate_hz, f.bit_depth, f.channels, f.lossless, f.spatial, \
     f.rg_gain_db, f.rg_peak";

/// SQL fragment for match queries (no specific library context - returns first library found).
const TRACK_COLS_NO_LIB: &str = "t.id, COALESCE((SELECT lt2.library_id FROM library_tracks lt2 \
                     WHERE lt2.track_id = t.id LIMIT 1), '') AS library_id, \
     t.content_hash, t.title, COALESCE(ar.name, '') AS artist, aa.name AS album_artist, \
     al.title AS album, al.year AS year, al.genre AS genre, t.track_no, t.disc_no, t.duration_ms, \
     t.acoustid, t.recording_mbid, COALESCE(ar.name_normalized, '') AS artist_norm, t.title_norm, \
     al.title_normalized AS album_norm, \
     f.codec, f.sample_rate_hz, f.bit_depth, f.channels, f.lossless, f.spatial, \
     f.rg_gain_db, f.rg_peak";

#[derive(sqlx::FromRow)]
pub struct LibraryRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub track_count: i64,
}

pub async fn list_libraries(db: &SqlitePool) -> AppResult<Vec<LibraryRow>> {
    Ok(sqlx::query_as::<_, LibraryRow>(
        "SELECT l.id, l.name, l.path, COUNT(lt.track_id) AS track_count \
         FROM libraries l LEFT JOIN library_tracks lt ON lt.library_id = l.id \
         GROUP BY l.id ORDER BY l.name",
    )
    .fetch_all(db)
    .await?)
}

pub async fn list_tracks(
    db: &SqlitePool,
    library_id: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<Track>> {
    let sql = format!(
        "SELECT {TRACK_COLS_WITH_LIB} \
         FROM library_tracks lt \
         JOIN tracks t ON t.id = lt.track_id \
         {TRACK_JOINS} \
         WHERE lt.library_id = ? \
         ORDER BY ar.name_normalized, al.title_normalized, t.disc_no, t.track_no, t.title_norm \
         LIMIT ? OFFSET ?"
    );
    let rows = sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(library_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.into_contract()).collect())
}

pub async fn get_track(db: &SqlitePool, track_id: &str) -> AppResult<Option<Track>> {
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} \
         FROM tracks t \
         {TRACK_JOINS} \
         WHERE t.id = ?"
    );
    let row = sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(track_id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.into_contract()))
}

/// Returns any known filesystem path for a track - used by the streaming handler.
pub async fn get_track_path(db: &SqlitePool, track_id: &str) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT fp.path FROM tracks t \
         JOIN file_paths fp ON fp.content_hash = t.content_hash \
         WHERE t.id = ? LIMIT 1",
    )
    .bind(track_id)
    .fetch_optional(db)
    .await?)
}

/// Source-file facts the streaming handler needs to serve a track.
pub struct StreamMeta {
    pub path: String,
    pub content_hash: String,
    pub codec: String,
    /// Spatial/Atmos passthrough-only: never transcode, always serve the original bitstream.
    pub spatial: bool,
}

/// Returns the source path, content hash, codec, and spatial flag for a track - used by the
/// streaming handler to decide between bit-perfect passthrough and a transcoded tier.
pub async fn get_track_meta(db: &SqlitePool, track_id: &str) -> AppResult<Option<StreamMeta>> {
    let row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT fp.path, t.content_hash, f.codec, f.spatial \
         FROM tracks t \
         JOIN files f ON f.content_hash = t.content_hash \
         JOIN file_paths fp ON fp.content_hash = t.content_hash \
         WHERE t.id = ? LIMIT 1",
    )
    .bind(track_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(path, content_hash, codec, spatial)| StreamMeta {
        path,
        content_hash,
        codec,
        spatial: spatial != 0,
    }))
}

/// Own-copy match helpers - used by playback.rs.
pub async fn find_track_by_hash(db: &SqlitePool, hash: &str) -> AppResult<Option<TrackRow>> {
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} FROM tracks t \
         {TRACK_JOINS} \
         WHERE t.content_hash = ? LIMIT 1"
    );
    Ok(sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(hash)
        .fetch_optional(db)
        .await?)
}

pub async fn find_track_by_acoustid(
    db: &SqlitePool,
    acoustid: &str,
) -> AppResult<Option<TrackRow>> {
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} FROM tracks t \
         {TRACK_JOINS} \
         WHERE t.acoustid = ? LIMIT 1"
    );
    Ok(sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(acoustid)
        .fetch_optional(db)
        .await?)
}

pub async fn find_track_by_mbid(db: &SqlitePool, mbid: &str) -> AppResult<Option<TrackRow>> {
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} FROM tracks t \
         {TRACK_JOINS} \
         WHERE t.recording_mbid = ? LIMIT 1"
    );
    Ok(sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(mbid)
        .fetch_optional(db)
        .await?)
}

pub async fn find_track_fuzzy(
    db: &SqlitePool,
    artist_norm: &str,
    title_norm: &str,
    duration_ms: u32,
) -> AppResult<Option<TrackRow>> {
    const FUZZ: i64 = 2000;
    let dur = duration_ms as i64;
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} FROM tracks t \
         {TRACK_JOINS} \
         WHERE ar.name_normalized = ? AND t.title_norm = ? \
         AND t.duration_ms BETWEEN ? AND ? LIMIT 1"
    );
    Ok(sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .bind(artist_norm)
        .bind(title_norm)
        .bind(dur - FUZZ)
        .bind(dur + FUZZ)
        .fetch_optional(db)
        .await?)
}
