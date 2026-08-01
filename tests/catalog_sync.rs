//! Integration coverage for the catalog-sync payload — what this library tells the Hub.
//!
//! The crate was binary-only until now, so nothing under `tests/` could reach any module and this
//! surface had no coverage at all. It is the one that matters most: the Hub builds its entire
//! catalog from this payload, and a field missing here is a field the Hub can never have. That is
//! not hypothetical — artist identity crossed as name strings alone, with the artist's MusicBrainz
//! id sitting unused in the local database, and the Hub minted duplicate artists as a result.
//!
//! These drive the real schema (migrations applied to a temp SQLite file) rather than mocking it,
//! because the bug was in a SELECT: a mock would have happily returned whatever the test asked for.

use chordia_library::catalog_sync;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

/// A migrated, empty database on a throwaway file. `sqlx::migrate!` needs a real path, and an
/// in-memory pool would give each connection its own blank database.
async fn db() -> (SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("library.sqlite");
    let url = format!(
        "sqlite://{}?mode=rwc",
        path.to_string_lossy().replace('\\', "/")
    );
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    (pool, dir)
}

async fn seed_library(pool: &SqlitePool) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO libraries (id, name, path) VALUES (?,?,?)")
        .bind(&id)
        .bind("Test")
        .bind("/music")
        .execute(pool)
        .await
        .expect("seed library");
    id
}

/// Insert one track with its artist, optionally carrying a MusicBrainz artist id.
async fn seed_track(pool: &SqlitePool, lib: &str, artist: &str, artist_mbid: Option<&str>) {
    let artist_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO artists (id, name, name_normalized, mbid) VALUES (?,?,?,?)")
        .bind(&artist_id)
        .bind(artist)
        .bind(artist.to_lowercase())
        .bind(artist_mbid)
        .execute(pool)
        .await
        .expect("seed artist");
    // Tracks are content-addressed: `content_hash` is a FK into `files`, so the file row has to
    // exist first. That indirection is the schema's whole shape — one file, one track, many
    // library memberships.
    let hash = format!("hash-{artist}");
    sqlx::query(
        "INSERT INTO files (content_hash, codec, sample_rate_hz, bit_depth, channels, lossless) \
         VALUES (?,'flac',44100,16,2,1)",
    )
    .bind(&hash)
    .execute(pool)
    .await
    .expect("seed file");

    let track_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tracks (id, content_hash, title, title_norm, artist_id, duration_ms) \
         VALUES (?,?,?,?,?,?)",
    )
    .bind(&track_id)
    .bind(&hash)
    .bind("A Song")
    .bind("a song")
    .bind(&artist_id)
    .bind(200_000i64)
    .execute(pool)
    .await
    .expect("seed track");
    // Membership is its own table and the sync query joins through it, so a track without a row
    // here is invisible to the Hub no matter what else is right.
    sqlx::query("INSERT INTO library_tracks (library_id, track_id) VALUES (?,?)")
        .bind(lib)
        .bind(&track_id)
        .execute(pool)
        .await
        .expect("seed membership");
}

/// The regression this file was written for: the artist's MusicBrainz id must reach the Hub.
///
/// It was stored locally and shipped over the upgrade-proposal wire, but the catalog-sync query
/// joined `artists` and selected only the name — so the Hub had nothing but a string to identify an
/// artist by, and a file tagged with a former name minted a duplicate instead of matching.
#[tokio::test]
async fn the_payload_carries_the_artist_musicbrainz_id() {
    let (pool, _dir) = db().await;
    let lib = seed_library(&pool).await;
    seed_track(
        &pool,
        &lib,
        "mgk",
        Some("f6af669a-56ea-448a-a044-de76181ada33"),
    )
    .await;

    let tracks = catalog_sync::collect_tracks(&pool, &lib)
        .await
        .expect("collect");
    assert_eq!(tracks.len(), 1);
    assert_eq!(
        tracks[0].artist_mbid.as_deref(),
        Some("f6af669a-56ea-448a-a044-de76181ada33"),
        "the artist MBID must cross the wire; without it the Hub can only match on a name"
    );
    assert_eq!(tracks[0].artist, "mgk");
}

/// Most files carry no MusicBrainz tags, and that has to stay a clean absence rather than an empty
/// string — the Hub treats `Some("")` and `None` differently when deciding whether to match by id.
#[tokio::test]
async fn a_track_without_a_musicbrainz_artist_id_sends_none() {
    let (pool, _dir) = db().await;
    let lib = seed_library(&pool).await;
    seed_track(&pool, &lib, "Untagged Artist", None).await;

    let tracks = catalog_sync::collect_tracks(&pool, &lib)
        .await
        .expect("collect");
    assert_eq!(tracks.len(), 1);
    assert!(
        tracks[0].artist_mbid.is_none(),
        "absent must be None, not Some(\"\")"
    );
    assert_eq!(tracks[0].artist, "Untagged Artist");
}

/// Sanity on the join itself: a library's payload contains only its own tracks. The sync request is
/// per-library and the Hub reconciles memberships from it, so a leak here would attach one library's
/// tracks to another.
#[tokio::test]
async fn the_payload_is_scoped_to_one_library() {
    let (pool, _dir) = db().await;
    let a = seed_library(&pool).await;
    let b = seed_library(&pool).await;
    seed_track(&pool, &a, "In A", None).await;
    seed_track(&pool, &b, "In B", None).await;

    let from_a = catalog_sync::collect_tracks(&pool, &a).await.expect("a");
    assert_eq!(from_a.len(), 1);
    assert_eq!(from_a[0].artist, "In A");
}
