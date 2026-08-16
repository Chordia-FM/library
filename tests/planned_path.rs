//! Where a track that has not arrived yet will be filed.
//!
//! The download path and the organiser have to agree. If they do not, the first copy of every album
//! lands somewhere the organiser then moves it from — which to a user looks exactly like the app
//! misfiling their music and then shuffling it around behind their back.
//!
//! Driven against the real schema and the real template renderer, because agreement with *that* is
//! the whole property under test; a hand-rolled expectation would only test itself.

use std::path::PathBuf;

use chordia_library::organize::{planned_path, PlannedTrack};
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

async fn library(pool: &SqlitePool, organize: bool, template: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO libraries (id, name, path, organize, organize_template) VALUES (?,?,?,?,?)",
    )
    .bind(&id)
    .bind("Music")
    .bind("/music")
    .bind(if organize { 1 } else { 0 })
    .bind(template)
    .execute(pool)
    .await
    .expect("insert library");
    id
}

fn track() -> PlannedTrack {
    PlannedTrack {
        title: "Bad Things".into(),
        artist: "mgk".into(),
        album_artist: Some("mgk".into()),
        album: Some("Bloom".into()),
        track_no: Some(3),
        disc_no: Some(1),
        disc_count: Some(1),
        ..PlannedTrack::default()
    }
}

#[tokio::test]
async fn a_download_lands_where_the_template_says() {
    let (pool, _dir) = db().await;
    let id = library(&pool, true, "{albumartist}/{album}/{track} - {title}").await;

    let path = planned_path(&pool, &id, track(), "flac")
        .await
        .expect("a path");
    assert_eq!(
        path,
        PathBuf::from("/music").join("mgk/Bloom/03 - Bad Things.flac")
    );
}

#[tokio::test]
async fn a_folder_that_does_not_organise_gets_no_plan() {
    // Not an error: someone who turned organising off keeps their own layout, and the caller drops
    // the file flat in the root instead. Refusing the download would be the wrong answer.
    let (pool, _dir) = db().await;
    let id = library(&pool, false, "{albumartist}/{album}/{track} - {title}").await;
    assert!(planned_path(&pool, &id, track(), "flac").await.is_none());
}

#[tokio::test]
async fn metadata_the_template_needs_and_does_not_have_gets_no_plan() {
    // The same refusal `organize_file` makes. A template asking for a track number cannot be
    // rendered for a track that has none, and quietly dropping the segment produces a path that
    // collides with every other untracked file on the album.
    let (pool, _dir) = db().await;
    let id = library(&pool, true, "{albumartist}/{album}/{track} - {title}").await;

    let untracked = PlannedTrack {
        track_no: None,
        ..track()
    };
    assert!(planned_path(&pool, &id, untracked, "flac").await.is_none());
}
