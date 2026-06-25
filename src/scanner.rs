//! Filesystem scanning + watching.
//!
//! `initial_scan` walks every configured library folder and upserts tracks into SQLite.
//! `start_watcher` registers a `notify` watcher for incremental re-index on fs events.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use sqlx::SqlitePool;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{error, info, warn};

use crate::index;

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
/// skipping any file under an excluded sub-directory.
pub async fn initial_scan(db: &SqlitePool, library_id: &str, root: &Path) {
    info!(library_id, path = ?root, "starting initial scan");
    let mut count = 0u32;
    let mut errors = 0u32;
    let mut skipped = 0u32;

    let mut unchanged = 0u32;
    let excluded = load_exclusions(db, library_id).await;
    let paths = collect_audio_files(root);
    for path in paths {
        if is_excluded(&path, &excluded) {
            skipped += 1;
            continue;
        }
        // Skip the full re-probe and SHA-256 when mtime and size match what we already indexed. The
        // fs watcher handles live edits between scans, so this only skips genuinely unchanged files.
        if path_unchanged(db, library_id, &path).await {
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
    info!(library_id, removed, "prune complete: removed deleted files");
}

/// Has this path already been indexed with the same mtime and size? If so a rescan can skip the
/// expensive re-probe and content re-hash. Conservative: any stat error, missing row, or NULL
/// freshness column returns `false` (re-index), so we never skip a file we're unsure about.
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
                initial_scan(&db, id, root).await;
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
