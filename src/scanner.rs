//! Filesystem scanning + watching.
//!
//! `initial_scan` walks every configured library folder and upserts tracks into SQLite.
//! `start_watcher` registers a `notify` watcher for incremental re-index on fs events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, LazyLock, Mutex};
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use sqlx::SqlitePool;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{error, info, warn};

use crate::index;

/// Live progress of the most recent scan per library, for the management UI. In-memory only (a
/// library restart aborts any scan anyway), keyed by `library_id`. Lets the UI show a progress bar
/// that survives navigation. The state lives here, not in a component, which matters because a
/// forced full re-index re-hashes every file and can run for many minutes.
#[derive(Clone, Default)]
pub struct ScanProgress {
    /// Whether a scan is currently in flight.
    pub running: bool,
    /// Total audio files found under the root this pass.
    pub total: u32,
    /// Files examined so far (indexed or skipped).
    pub done: u32,
    /// Whether this pass is a forced full re-index (re-processes unchanged files).
    pub force: bool,
}

static SCAN_PROGRESS: LazyLock<Mutex<HashMap<String, ScanProgress>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Snapshot the current scan progress for a library, or `None` if it has never been scanned.
pub fn scan_progress(library_id: &str) -> Option<ScanProgress> {
    SCAN_PROGRESS.lock().ok()?.get(library_id).cloned()
}

fn progress_begin(library_id: &str, total: u32, force: bool) {
    if let Ok(mut m) = SCAN_PROGRESS.lock() {
        m.insert(
            library_id.to_string(),
            ScanProgress {
                running: true,
                total,
                done: 0,
                force,
            },
        );
    }
}

fn progress_set_done(library_id: &str, done: u32) {
    if let Ok(mut m) = SCAN_PROGRESS.lock() {
        if let Some(p) = m.get_mut(library_id) {
            p.done = done;
        }
    }
}

fn progress_finish(library_id: &str) {
    if let Ok(mut m) = SCAN_PROGRESS.lock() {
        if let Some(p) = m.get_mut(library_id) {
            p.running = false;
            p.done = p.total;
        }
    }
}

/// Extensions we consider audio files.
const AUDIO_EXTS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "aiff", "aif", "alac", "wv", "ape", "wma",
];

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Walk `root` and upsert every audio file into the SQLite index under `library_id`,
/// skipping any file under an excluded sub-directory. When `force` is set, every file is re-probed
/// and re-indexed even if unchanged. This is needed to re-derive index-time decisions (e.g. edition folding)
/// on a library indexed before that logic existed; the normal path skips unchanged files for speed.
pub async fn initial_scan(db: &SqlitePool, library_id: &str, root: &Path, force: bool) {
    info!(library_id, path = ?root, force, "starting initial scan");
    let mut count = 0u32;
    let mut errors = 0u32;
    let mut skipped = 0u32;

    let mut unchanged = 0u32;
    let excluded = load_exclusions(db, library_id).await;
    let paths = collect_audio_files(root);
    progress_begin(library_id, paths.len() as u32, force);
    let mut examined = 0u32;
    for path in paths {
        examined += 1;
        progress_set_done(library_id, examined);
        if is_excluded(&path, &excluded) {
            skipped += 1;
            continue;
        }
        // Skip the full re-probe and SHA-256 when mtime and size match what we already indexed. The
        // fs watcher handles live edits between scans, so this only skips genuinely unchanged files.
        // A forced rescan bypasses the skip to re-run indexing for every file.
        if !force && path_unchanged(db, library_id, &path).await {
            unchanged += 1;
            continue;
        }
        match index_file(db, library_id, &path).await {
            Ok(_) => count += 1,
            Err(e) => {
                errors += 1;
                warn!(path = ?path, error = %e, "scan: failed to index file");
            }
        }
    }
    progress_finish(library_id);
    info!(
        library_id,
        count, errors, skipped, unchanged, "initial scan complete"
    );
}

/// Drop index entries for files that have been deleted from disk while we weren't watching.
///
/// Safety: per-file `try_exists(path) == Ok(false)` does not distinguish "file deleted" from
/// "whole drive unmounted". On Windows an offline drive letter reports `Ok(false)` for every path
/// under it, so pruning naively would wipe a library living on an external or network drive that's
/// simply offline. We therefore first confirm the library root is reachable. If it isn't, we
/// skip pruning entirely (the files are presumed still there, just unreachable right now).
pub async fn prune_missing(db: &SqlitePool, library_id: &str) {
    let root: Option<String> = sqlx::query_scalar("SELECT path FROM libraries WHERE id = ?")
        .bind(library_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    match &root {
        Some(r) if matches!(tokio::fs::try_exists(r).await, Ok(true)) => {}
        _ => {
            warn!(
                library_id,
                root = ?root,
                "prune skipped: library root not reachable (drive offline?), not removing any files"
            );
            return;
        }
    }

    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM file_paths WHERE library_id = ?")
        .bind(library_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();

    let mut removed = 0u32;
    for path in paths {
        if matches!(tokio::fs::try_exists(&path).await, Ok(false)) {
            match index::remove_track(db, library_id, std::path::Path::new(&path)).await {
                Ok(_) => removed += 1,
                Err(e) => warn!(path = %path, error = %e, "prune: remove failed"),
            }
        }
    }
    let superseded = prune_superseded(db, library_id).await;
    info!(
        library_id,
        removed, superseded, "prune complete: removed deleted files"
    );
}

/// Has this path already been indexed with the same mtime and size? If so a rescan can skip the
/// expensive re-probe and content re-hash. Conservative: any stat error, missing row, or NULL
/// freshness column returns `false` (re-index), so we never skip a file we're unsure about.
/// Remove tracks whose content is no longer at ANY path — the ones the sweep above cannot see.
///
/// [`prune_missing`] walks `file_paths` and removes tracks whose FILE disappeared. That misses a
/// track whose content was superseded **in place**. Rewriting a file's tags changes its
/// `content_hash`, so `upsert_track` mints a fresh `files`/`tracks`/`library_tracks` set, and
/// `file_paths` — `UNIQUE(path)` — is REPOINTED to the new hash rather than gaining a row. The old
/// track keeps its library membership and is reachable from no path at all, so every prune skipped
/// it while `catalog_sync` went on pushing it forever. Retagging one album that way put two and
/// three copies of every track on the Hub.
///
/// Deletion order mirrors [`crate::index::remove_track`], including keeping the `files` row: it
/// carries codec and ReplayGain loudness keyed by content hash, is invisible to track queries once
/// orphaned, and lets the same bytes be re-indexed without re-analysis.
///
/// Callers must have already checked the library root is reachable — an unmounted drive makes every
/// path look absent, and this would then delete the whole library.
async fn prune_superseded(db: &SqlitePool, library_id: &str) -> u64 {
    // Membership first: this library no longer reaches that content through any path of its own.
    let dropped = sqlx::query(
        "DELETE FROM library_tracks \
          WHERE library_id = ?1 \
            AND track_id IN ( \
                SELECT t.id FROM tracks t \
                 WHERE NOT EXISTS ( \
                     SELECT 1 FROM file_paths fp \
                      WHERE fp.content_hash = t.content_hash AND fp.library_id = ?1))",
    )
    .bind(library_id)
    .execute(db)
    .await;
    let dropped = match dropped {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            warn!(library_id, error = %e, "prune: dropping superseded memberships failed");
            return 0;
        }
    };

    // Then the row itself, but only once NO library reaches it and NO path anywhere points at it.
    // Both clauses are load-bearing: another library may still hold the same content, and a path in
    // another library must keep the metadata alive even if this one dropped its membership.
    if let Err(e) = sqlx::query(
        "DELETE FROM tracks \
          WHERE NOT EXISTS (SELECT 1 FROM library_tracks lt WHERE lt.track_id = tracks.id) \
            AND NOT EXISTS ( \
                SELECT 1 FROM file_paths fp WHERE fp.content_hash = tracks.content_hash)",
    )
    .execute(db)
    .await
    {
        warn!(library_id, error = %e, "prune: deleting superseded tracks failed");
    }
    dropped
}

async fn path_unchanged(db: &SqlitePool, library_id: &str, path: &Path) -> bool {
    let (mtime_ns, size_bytes) = index::file_freshness(path).await;
    let (Some(mtime_ns), Some(size_bytes)) = (mtime_ns, size_bytes) else {
        return false;
    };
    let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT mtime_ns, size_bytes FROM file_paths WHERE path = ? AND library_id = ?",
    )
    .bind(path.to_string_lossy().as_ref())
    .bind(library_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    matches!(row, Some((Some(m), Some(s))) if m == mtime_ns && s == size_bytes)
}

/// Load a library's excluded directory paths.
pub async fn load_exclusions(db: &SqlitePool, library_id: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT path FROM library_excluded_dirs WHERE library_id = ?")
        .bind(library_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
}

fn norm_path(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// Is `path` inside (or equal to) any excluded directory? Case-insensitive, separator-agnostic,
/// and boundary-aware so `/music/rock` doesn't match `/music/rockabilly`.
pub fn is_excluded(path: &Path, excluded: &[String]) -> bool {
    if excluded.is_empty() {
        return false;
    }
    let p = norm_path(&path.to_string_lossy());
    excluded.iter().any(|ex| {
        let e = norm_path(ex);
        !e.is_empty() && (p == e || p.starts_with(&format!("{e}/")))
    })
}

/// Index a single file.  Returns the track UUID or an error.
///
/// If the library organises on disk, the freshly-indexed file is then moved into its template
/// location (and `file_paths` updated). The move is best-effort, so a failure is logged, not fatal.
pub async fn index_file(db: &SqlitePool, library_id: &str, path: &Path) -> anyhow::Result<String> {
    let probed = tokio::task::spawn_blocking({
        let p = path.to_owned();
        move || crate::metadata::probe(&p)
    })
    .await??;
    let track_id = index::upsert_track(db, library_id, path, &probed).await?;

    if let Some((root, settings)) = crate::organize::library_settings(db, library_id).await {
        if let Err(e) = crate::organize::organize_file(db, library_id, &root, &settings, path).await
        {
            warn!(path = ?path, error = %e, "organize: failed to place file");
        }
    }

    Ok(track_id)
}

/// Periodically prune deleted files and re-scan each library, catching changes missed while the
/// server was down or if a watcher event was dropped. The `notify` watcher handles live changes
/// between runs; this is the backstop. A full rescan re-hashes files, so the interval should stay
/// coarse (see [`crate::config::ScanConfig`]).
pub fn start_scheduler(
    db: SqlitePool,
    libraries: Vec<(String, PathBuf)>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            for (id, root) in &libraries {
                prune_missing(&db, id).await;
                initial_scan(&db, id, root, false).await;
            }
        }
    })
}

/// Spawn a `notify` watcher task for each `(library_id, root)` pair.
/// Events are debounced and processed in the background.
pub fn start_watcher(
    db: SqlitePool,
    libraries: Vec<(String, PathBuf)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if libraries.is_empty() {
            return;
        }

        let (std_tx, std_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = match notify::recommended_watcher(std_tx) {
            Ok(w) => w,
            Err(e) => {
                error!(error = %e, "failed to create fs watcher");
                return;
            }
        };

        for (_, root) in &libraries {
            if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                warn!(path = ?root, error = %e, "could not watch path");
            }
        }

        // Bridge notify's std::sync::mpsc to a tokio channel so we can .await inside the loop.
        let (tok_tx, mut tok_rx) = tokio_mpsc::unbounded_channel::<notify::Result<Event>>();
        std::thread::spawn(move || {
            for ev in std_rx {
                if tok_tx.send(ev).is_err() {
                    break;
                }
            }
            // Keep watcher alive until the thread exits.
            drop(watcher);
        });

        // Build a quick library_id lookup by path prefix.
        // Swap to (PathBuf, String) for the prefix-match lookup below.
        let lib_map: Vec<(PathBuf, String)> =
            libraries.into_iter().map(|(id, path)| (path, id)).collect();

        while let Some(result) = tok_rx.recv().await {
            match result {
                Ok(event) => handle_event(&db, &lib_map, event).await,
                Err(e) => warn!(error = %e, "fs watcher error"),
            }
        }
    })
}

async fn handle_event(db: &SqlitePool, lib_map: &[(PathBuf, String)], event: Event) {
    for path in event.paths {
        if !is_audio(&path) {
            continue;
        }
        let library_id = match find_library(lib_map, &path) {
            Some(id) => id,
            None => continue,
        };
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                // Small delay so the write is flushed before we read.
                tokio::time::sleep(Duration::from_millis(200)).await;
                if is_excluded(&path, &load_exclusions(db, library_id).await) {
                    continue;
                }
                if let Err(e) = index_file(db, library_id, &path).await {
                    warn!(path = ?path, error = %e, "watcher: index failed");
                } else {
                    info!(path = ?path, "watcher: indexed");
                }
            }
            EventKind::Remove(_) => {
                if let Err(e) = index::remove_track(db, library_id, &path).await {
                    warn!(path = ?path, error = %e, "watcher: remove failed");
                } else {
                    info!(path = ?path, "watcher: removed");
                }
            }
            _ => {}
        }
    }
}

fn find_library<'a>(lib_map: &'a [(PathBuf, String)], path: &Path) -> Option<&'a str> {
    lib_map
        .iter()
        .find(|(root, _)| path.starts_with(root))
        .map(|(_, id)| id.as_str())
}

fn collect_audio_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(collect_audio_files(&p));
            } else if is_audio(&p) {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    /// The three tables the sweep reasons over, plus the columns it touches.
    async fn mem_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for ddl in [
            "CREATE TABLE file_paths (id TEXT PRIMARY KEY, content_hash TEXT NOT NULL, \
             library_id TEXT NOT NULL, path TEXT NOT NULL UNIQUE)",
            "CREATE TABLE tracks (id TEXT PRIMARY KEY, content_hash TEXT NOT NULL UNIQUE, \
             title TEXT NOT NULL)",
            "CREATE TABLE library_tracks (library_id TEXT NOT NULL, track_id TEXT NOT NULL, \
             PRIMARY KEY (library_id, track_id))",
        ] {
            sqlx::query(ddl).execute(&db).await.unwrap();
        }
        db
    }

    async fn add(db: &SqlitePool, lib: &str, hash: &str, title: &str, path: Option<&str>) {
        sqlx::query("INSERT INTO tracks (id, content_hash, title) VALUES (?, ?, ?)")
            .bind(format!("trk-{hash}"))
            .bind(hash)
            .bind(title)
            .execute(db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO library_tracks (library_id, track_id) VALUES (?, ?)")
            .bind(lib)
            .bind(format!("trk-{hash}"))
            .execute(db)
            .await
            .unwrap();
        if let Some(p) = path {
            sqlx::query(
                "INSERT INTO file_paths (id, content_hash, library_id, path) VALUES (?, ?, ?, ?)",
            )
            .bind(format!("fp-{hash}"))
            .bind(hash)
            .bind(lib)
            .bind(p)
            .execute(db)
            .await
            .unwrap();
        }
    }

    async fn titles(db: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar::<_, String>("SELECT title FROM tracks ORDER BY title")
            .fetch_all(db)
            .await
            .unwrap()
    }

    /// The reported bug: retagging a file in place left the pre-tag track reachable from nothing,
    /// and it kept being pushed to the Hub as a duplicate of the track that replaced it.
    #[tokio::test]
    async fn a_track_superseded_in_place_is_removed() {
        let db = mem_db().await;
        // Same path, two content hashes: the tag rewrite repointed `file_paths` to the new one.
        add(&db, "lib", "old", "Attention (pre-tag)", None).await;
        add(
            &db,
            "lib",
            "new",
            "Attention",
            Some("/music/Attention.flac"),
        )
        .await;

        let dropped = prune_superseded(&db, "lib").await;

        assert_eq!(dropped, 1, "the superseded membership was not dropped");
        assert_eq!(
            titles(&db).await,
            vec!["Attention".to_string()],
            "the pre-tag track survived and will keep syncing as a duplicate"
        );
    }

    /// The guard that stops this deleting a healthy library: a track still at a path must survive.
    #[tokio::test]
    async fn a_track_still_at_a_path_is_kept() {
        let db = mem_db().await;
        add(&db, "lib", "live", "Kept", Some("/music/Kept.flac")).await;

        assert_eq!(prune_superseded(&db, "lib").await, 0);
        assert_eq!(titles(&db).await, vec!["Kept".to_string()]);
    }

    /// Another library still reaching the same bytes keeps the row alive, even though THIS library
    /// no longer has a path to it. Both `NOT EXISTS` clauses in the delete exist for this case.
    #[tokio::test]
    async fn content_another_library_still_holds_is_kept() {
        let db = mem_db().await;
        add(&db, "a", "shared", "Shared", None).await;
        sqlx::query("INSERT INTO library_tracks (library_id, track_id) VALUES ('b', 'trk-shared')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO file_paths (id, content_hash, library_id, path) \
             VALUES ('fp-b', 'shared', 'b', '/other/Shared.flac')",
        )
        .execute(&db)
        .await
        .unwrap();

        prune_superseded(&db, "a").await;

        assert_eq!(
            titles(&db).await,
            vec!["Shared".to_string()],
            "a track another library still reaches through its own path was deleted"
        );
    }
}
