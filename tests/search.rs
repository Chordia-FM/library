//! `search::search` against the real migrated schema: ranking across tracks, albums and artists,
//! escaping, and the album/artist expansions `/play` queues.

use chordia_library::search::{self, HitKind};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

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

async fn artist(db: &SqlitePool, id: &str, name: &str) {
    sqlx::query("INSERT INTO artists (id, name, name_normalized) VALUES (?, ?, ?)")
        .bind(id)
        .bind(name)
        .bind(chordia_library::metadata::normalize(name))
        .execute(db)
        .await
        .unwrap();
}

async fn album(db: &SqlitePool, id: &str, title: &str, artist_id: &str, year: i64) {
    sqlx::query(
        "INSERT INTO albums (id, title, title_normalized, artist_id, year) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(title)
    .bind(chordia_library::metadata::normalize(title))
    .bind(artist_id)
    .bind(year)
    .execute(db)
    .await
    .unwrap();
}

async fn track(
    db: &SqlitePool,
    id: &str,
    title: &str,
    artist_id: &str,
    album_id: Option<&str>,
    track_no: i64,
    duration_ms: i64,
) {
    let hash = format!("hash-{id}");
    sqlx::query(
        "INSERT INTO files (content_hash, codec, sample_rate_hz, bit_depth, channels, lossless, \
             duration_ms) VALUES (?, 'flac', 44100, 16, 2, 1, ?)",
    )
    .bind(&hash)
    .bind(duration_ms)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tracks (id, content_hash, title, title_norm, duration_ms, artist_id, \
             album_id, track_no, disc_no) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(id)
    .bind(&hash)
    .bind(title)
    .bind(chordia_library::metadata::normalize(title))
    .bind(duration_ms)
    .bind(artist_id)
    .bind(album_id)
    .bind(track_no)
    .execute(db)
    .await
    .unwrap();
}

/// Daft Punk with two albums; a second artist whose track title contains "one".
async fn seed(db: &SqlitePool) {
    artist(db, "ar-dp", "Daft Punk").await;
    artist(db, "ar-other", "Someone Else").await;
    artist(db, "ar-empty", "Orphan Artist").await;
    album(db, "al-disc", "Discovery", "ar-dp", 2001).await;
    album(db, "al-hw", "Homework", "ar-dp", 1997).await;
    album(db, "al-x", "Nothing to Do With It", "ar-other", 2010).await;
    track(
        db,
        "t-omt",
        "One More Time",
        "ar-dp",
        Some("al-disc"),
        1,
        320_000,
    )
    .await;
    track(
        db,
        "t-aero",
        "Aerodynamic",
        "ar-dp",
        Some("al-disc"),
        2,
        212_000,
    )
    .await;
    track(
        db,
        "t-dl",
        "Digital Love",
        "ar-dp",
        Some("al-disc"),
        3,
        301_000,
    )
    .await;
    track(
        db,
        "t-atw",
        "Around the World",
        "ar-dp",
        Some("al-hw"),
        7,
        429_000,
    )
    .await;
    track(db, "t-one", "One", "ar-other", Some("al-x"), 1, 100_000).await;
    track(
        db,
        "t-onemore",
        "One More Time (Live)",
        "ar-other",
        None,
        1,
        330_000,
    )
    .await;
}

#[tokio::test]
async fn exact_title_beats_longer_and_foreign_matches() {
    let (db, _dir) = db().await;
    seed(&db).await;
    let hits = search::search(&db, "one more time", &[HitKind::Track], 10)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, vec!["t-omt", "t-onemore"], "{hits:#?}");
    assert_eq!(hits[0].subtitle, "Daft Punk");
    assert_eq!(hits[0].duration_ms, Some(320_000));
}

#[tokio::test]
async fn artist_query_ranks_the_artist_above_their_tracks() {
    let (db, _dir) = db().await;
    seed(&db).await;
    let hits = search::search(
        &db,
        "daft punk",
        &[HitKind::Track, HitKind::Album, HitKind::Artist],
        10,
    )
    .await
    .unwrap();
    assert_eq!(hits[0].kind, HitKind::Artist, "{hits:#?}");
    assert_eq!(hits[0].id, "ar-dp");
    assert_eq!(hits[0].track_count, 4);
    // Every Daft Punk track and album is in the list, matched through the artist column.
    assert!(hits.iter().any(|h| h.id == "t-atw"));
    assert!(hits.iter().any(|h| h.id == "al-hw"));
    // An artist with no tracks is never offered.
    assert!(!hits.iter().any(|h| h.id == "ar-empty"));
}

#[tokio::test]
async fn album_hit_carries_count_duration_and_year() {
    let (db, _dir) = db().await;
    seed(&db).await;
    let hits = search::search(&db, "discovery", &[HitKind::Album], 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let h = &hits[0];
    assert_eq!(h.id, "al-disc");
    assert_eq!(h.track_count, 3);
    assert_eq!(h.duration_ms, Some(320_000 + 212_000 + 301_000));
    assert_eq!(h.subtitle, "Daft Punk · 2001");
}

#[tokio::test]
async fn multi_term_queries_need_every_term_and_punctuation_is_ignored() {
    let (db, _dir) = db().await;
    seed(&db).await;
    let hits = search::search(&db, "punk aero-dynamic!", &[HitKind::Track], 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "t-aero");

    let none = search::search(&db, "daft nonsense", &[HitKind::Track], 10)
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn wildcards_in_the_query_are_literal_and_blank_queries_return_nothing() {
    let (db, _dir) = db().await;
    seed(&db).await;
    // `normalize` strips `%`, so this is just "one"; the point is that it does not blow up or match
    // everything.
    let hits = search::search(&db, "%", &[HitKind::Track], 10)
        .await
        .unwrap();
    assert!(hits.is_empty());
    let hits = search::search(&db, "   ", &[HitKind::Track], 10)
        .await
        .unwrap();
    assert!(hits.is_empty());
    let hits = search::search(&db, "one", &[HitKind::Track], 0)
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn limit_and_ordering_across_kinds() {
    let (db, _dir) = db().await;
    seed(&db).await;
    let hits = search::search(
        &db,
        "one",
        &[HitKind::Track, HitKind::Album, HitKind::Artist],
        2,
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 2);
    // "One" is an exact title match and wins outright.
    assert_eq!(hits[0].id, "t-one");
    // Scores are non-increasing.
    assert!(hits[0].score >= hits[1].score);
}

#[tokio::test]
async fn album_and_artist_expansions_are_ordered() {
    let (db, _dir) = db().await;
    seed(&db).await;
    let rows = search::album_tracks(&db, "al-disc").await.unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["t-omt", "t-aero", "t-dl"]);
    assert_eq!(rows[0].artist, "Daft Punk");
    assert_eq!(rows[0].album.as_deref(), Some("Discovery"));

    let rows = search::artist_tracks(&db, "ar-dp", 100).await.unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    // Homework (1997) before Discovery (2001), then track order within each.
    assert_eq!(ids, vec!["t-atw", "t-omt", "t-aero", "t-dl"]);

    let rows = search::artist_tracks(&db, "ar-dp", 2).await.unwrap();
    assert_eq!(rows.len(), 2);
}

/// The suggestions autocomplete opens with: the asker's requests, the server's favourites, the
/// newest tracks, in that order and each once.
#[cfg(feature = "discord")]
#[tokio::test]
async fn suggestions_lead_with_the_askers_requests_then_the_popular_then_the_new() {
    use chordia_library::discord::suggest::suggest;

    let (db, _dir) = db().await;
    artist(&db, "ar1", "Daft Punk").await;
    album(&db, "al1", "Discovery", "ar1", 2001).await;
    for i in 1..=6 {
        track(
            &db,
            &format!("t{i}"),
            &format!("Track {i}"),
            "ar1",
            Some("al1"),
            i,
            200_000,
        )
        .await;
    }
    // 42 asked for t2 twice and t3 once; the room hammered t5; someone played t1.
    for (track, by, at) in [
        ("t2", "42", 1),
        ("t3", "42", 2),
        ("t2", "42", 3),
        ("t5", "7", 4),
        ("t5", "8", 5),
        ("t5", "9", 6),
        ("t1", "7", 7),
    ] {
        sqlx::query(
            "INSERT INTO discord_plays (app_id, guild_id, track_id, requested_by, started_at) \
             VALUES ('1', 'g', ?, ?, ?)",
        )
        .bind(track)
        .bind(by)
        .bind(at)
        .execute(&db)
        .await
        .unwrap();
    }
    let hits = suggest(&db, "1", "g", 42, &[HitKind::Track], 25)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    // Mine newest first, then the most played not already there, then the newest indexed.
    assert_eq!(ids, ["t2", "t3", "t5", "t1", "t6", "t4"]);
    assert_eq!(hits[0].title, "Track 2");
    // Several kinds share the room, tracks first, the album and the artist behind them.
    let mixed = suggest(
        &db,
        "1",
        "g",
        42,
        &[HitKind::Track, HitKind::Album, HitKind::Artist],
        25,
    )
    .await
    .unwrap();
    assert_eq!(mixed[0].kind, HitKind::Track);
    assert!(mixed
        .iter()
        .any(|h| h.kind == HitKind::Album && h.id == "al1"));
    assert!(mixed
        .iter()
        .any(|h| h.kind == HitKind::Artist && h.id == "ar1"));
    // A stranger with no requests still gets the favourites and the new.
    let theirs = suggest(&db, "1", "g", 99, &[HitKind::Track], 3)
        .await
        .unwrap();
    let ids: Vec<&str> = theirs.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["t5", "t2", "t1"]);
}
