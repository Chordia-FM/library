//! Library management API for adding and listing library folders and browsing the server filesystem.
//!
//! Authenticated with `Authorization: Library {management_token}` (issued during the claim flow).

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::http::AppState;
use crate::index;
use crate::scanner;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mgmt/libraries", get(list_libraries).post(add_library))
        .route(
            "/mgmt/libraries/{id}",
            axum::routing::patch(link_hub_library),
        )
        .route("/mgmt/libraries/{id}/tree", get(library_tree))
        .route(
            "/mgmt/libraries/{id}/dirs",
            axum::routing::put(set_excluded_dirs),
        )
        .route(
            "/mgmt/libraries/{id}/organize",
            axum::routing::put(set_organize),
        )
        .route(
            "/mgmt/libraries/{id}/rescan",
            axum::routing::post(rescan_library),
        )
        .route("/mgmt/browse", get(browse_dirs))
}

pub(crate) async fn require_mgmt_auth(headers: &HeaderMap, state: &AppState) -> AppResult<()> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Library "))
        .ok_or(AppError::Unauthorized)?;

    let lock = state.credentials.read().await;
    let expected = lock
        .as_ref()
        .map(|c| c.management_token.as_str())
        .ok_or(AppError::Unauthorized)?;

    if provided != expected {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

// Library list and add.

#[derive(Deserialize)]
struct AddLibraryRequest {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct LibraryInfo {
    id: String,
    name: String,
    path: String,
    /// The Hub-side library UUID this local library is linked to (if any), which lets the frontend
    /// map a Hub library back to its local id for management.
    hub_library_id: Option<String>,
    /// Whether files are laid out on disk from the templates below.
    organize: bool,
    /// Album or default template (e.g. `{albumartist}/{album}/{track} - {title}`).
    organize_template: Option<String>,
    /// Template for tracks with no album (singles). Falls back to the album template when empty.
    organize_template_single: Option<String>,
    /// Template for unidentified tracks (no artist tag). Falls back to the album template.
    organize_template_unknown: Option<String>,
    /// Keep only the highest-quality copy when several files map to the same destination.
    dedupe: bool,
}

async fn list_libraries(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<LibraryInfo>>> {
    require_mgmt_auth(&headers, &state).await?;

    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<String>,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        ),
    >(
        "SELECT id, name, path, hub_library_id, organize, organize_template, \
                organize_template_single, organize_template_unknown, dedupe \
         FROM libraries ORDER BY name",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(
                    id,
                    name,
                    path,
                    hub_library_id,
                    organize,
                    organize_template,
                    organize_template_single,
                    organize_template_unknown,
                    dedupe,
                )| LibraryInfo {
                    id,
                    name,
                    path,
                    hub_library_id,
                    organize: organize != 0,
                    organize_template,
                    organize_template_single,
                    organize_template_unknown,
                    dedupe: dedupe != 0,
                },
            )
            .collect(),
    ))
}

async fn add_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AddLibraryRequest>,
) -> AppResult<(StatusCode, Json<LibraryInfo>)> {
    require_mgmt_auth(&headers, &state).await?;

    let name = body.name.trim().to_string();
    let path_str = body.path.trim().to_string();

    if name.is_empty() || path_str.is_empty() {
        return Err(AppError::BadRequest("name and path are required".into()));
    }

    let path = std::path::PathBuf::from(&path_str);
    if !path.exists() {
        return Err(AppError::BadRequest(format!(
            "path does not exist: {path_str}"
        )));
    }

    // Ensure the folder is inside the sandboxed music root.
    let music_root = state.config.data_dir.join("music");
    let root_canonical = std::fs::canonicalize(&music_root)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("music root unavailable: {e}")))?;
    let path_canonical = std::fs::canonicalize(&path)
        .map_err(|_| AppError::BadRequest(format!("cannot resolve path: {path_str}")))?;
    if !path_canonical.starts_with(&root_canonical) {
        return Err(AppError::BadRequest(
            "library path must be inside the server's data/music directory".into(),
        ));
    }

    let library_id = index::upsert_library(&state.db, &name, &path)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    let db = state.db.clone();
    let lib_id = library_id.clone();
    tokio::spawn(async move {
        scanner::initial_scan(&db, &lib_id, &path).await;
    });

    scanner::start_watcher(
        state.db.clone(),
        vec![(library_id.clone(), std::path::PathBuf::from(&path_str))],
    );

    Ok((
        StatusCode::CREATED,
        Json(LibraryInfo {
            id: library_id,
            name,
            path: path_str,
            hub_library_id: None,
            organize: false,
            organize_template: None,
            organize_template_single: None,
            organize_template_unknown: None,
            dedupe: false,
        }),
    ))
}

#[derive(Deserialize)]
struct LinkHubRequest {
    hub_library_id: String,
}

/// `PATCH /v1/mgmt/libraries/:id` stores the Hub-side library UUID so the streaming
/// handler can cross-check capability tokens (M4).
async fn link_hub_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(local_id): Path<String>,
    Json(body): Json<LinkHubRequest>,
) -> AppResult<StatusCode> {
    require_mgmt_auth(&headers, &state).await?;

    let hub_id = body.hub_library_id.trim().to_string();
    if hub_id.is_empty() {
        return Err(AppError::BadRequest("hub_library_id required".into()));
    }

    let updated = sqlx::query("UPDATE libraries SET hub_library_id = ? WHERE id = ?")
        .bind(&hub_id)
        .bind(&local_id)
        .execute(&state.db)
        .await?;

    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Strip the Windows verbatim prefix `\\?\` added by `canonicalize` so callers
/// receive normal-looking paths (e.g. `D:\music` instead of `\\?\D:\music`).
fn display_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

// Directory inclusion tree.

#[derive(Serialize)]
struct DirNode {
    name: String,
    path: String,
    /// Whether this directory is currently scanned (not excluded by itself or an ancestor).
    included: bool,
    children: Vec<DirNode>,
}

#[derive(Serialize)]
struct TreeResponse {
    root: String,
    dirs: Vec<DirNode>,
}

fn build_tree(dir: &std::path::Path, excluded: &[String]) -> Vec<DirNode> {
    let mut nodes = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return nodes;
    };
    let mut entries: Vec<_> = rd
        .flatten()
        .filter(|e| matches!(e.file_type(), Ok(t) if t.is_dir()))
        .collect();
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let p = e.path();
        nodes.push(DirNode {
            name,
            path: display_path(&p),
            included: !scanner::is_excluded(&p, excluded),
            children: build_tree(&p, excluded),
        });
    }
    nodes
}

/// `GET /v1/mgmt/libraries/{id}/tree` returns the directory tree under a library's root, with each
/// node flagged included or excluded so the UI can render checkboxes.
async fn library_tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<TreeResponse>> {
    require_mgmt_auth(&headers, &state).await?;
    let root: String = sqlx::query_scalar("SELECT path FROM libraries WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let excluded = scanner::load_exclusions(&state.db, &id).await;
    let root_pb = std::path::PathBuf::from(&root);
    let dirs = tokio::task::spawn_blocking(move || build_tree(&root_pb, &excluded))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("tree walk failed: {e}")))?;
    Ok(Json(TreeResponse {
        root: display_path(std::path::Path::new(&root)),
        dirs,
    }))
}

#[derive(Deserialize)]
struct SetDirsRequest {
    /// Absolute directory paths (under the library root) to exclude from the library.
    excluded: Vec<String>,
}

/// `PUT /v1/mgmt/libraries/{id}/dirs` replaces the excluded-directory set and re-scans so
/// memberships reflect the new selection.
async fn set_excluded_dirs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SetDirsRequest>,
) -> AppResult<StatusCode> {
    require_mgmt_auth(&headers, &state).await?;

    let root: String = sqlx::query_scalar("SELECT path FROM libraries WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;

    // Every excluded path must live under the library root.
    let root_set = [root.clone()];
    for p in &body.excluded {
        if !scanner::is_excluded(std::path::Path::new(p), &root_set) {
            return Err(AppError::BadRequest(format!(
                "path outside library root: {p}"
            )));
        }
    }

    sqlx::query("DELETE FROM library_excluded_dirs WHERE library_id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;
    for p in &body.excluded {
        sqlx::query("INSERT OR IGNORE INTO library_excluded_dirs (library_id, path) VALUES (?, ?)")
            .bind(&id)
            .bind(p)
            .execute(&state.db)
            .await?;
    }

    // Drop memberships for files that are now excluded.
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM file_paths WHERE library_id = ?")
        .bind(&id)
        .fetch_all(&state.db)
        .await?;
    for path in paths {
        if scanner::is_excluded(std::path::Path::new(&path), &body.excluded) {
            let _ = index::remove_track(&state.db, &id, std::path::Path::new(&path)).await;
        }
    }

    // Re-scan to (re-)add anything that's now included.
    let db = state.db.clone();
    let lib = id.clone();
    let root_pb = std::path::PathBuf::from(&root);
    tokio::spawn(async move {
        scanner::initial_scan(&db, &lib, &root_pb).await;
    });

    Ok(StatusCode::NO_CONTENT)
}

// Organise on disk.

#[derive(Deserialize)]
struct SetOrganizeRequest {
    /// Whether to lay files out from the templates.
    organize: bool,
    /// Album or default template (e.g. `{albumartist}/{album}/{track} - {title}`). Required to enable.
    #[serde(default)]
    template: Option<String>,
    /// Optional template for tracks with no album.
    #[serde(default)]
    template_single: Option<String>,
    /// Optional template for unidentified tracks.
    #[serde(default)]
    template_unknown: Option<String>,
    /// Collapse files that map to the same destination to the highest-quality copy.
    #[serde(default)]
    dedupe: bool,
}

/// Trim a template to `Some(non-empty)` and reject one missing a filename segment (trailing slash).
fn clean_template(raw: Option<String>) -> AppResult<Option<String>> {
    let t = raw.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if let Some(t) = &t {
        if t.ends_with('/') || t.ends_with('\\') {
            return Err(AppError::BadRequest(
                "template must end with a file name segment".into(),
            ));
        }
    }
    Ok(t)
}

/// `PUT /v1/mgmt/libraries/{id}/organize` sets the organise toggle, templates, and dedupe flag.
/// Enabling kicks off a background pass that lays out every existing file. Disabling just leaves
/// files where they are.
async fn set_organize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SetOrganizeRequest>,
) -> AppResult<StatusCode> {
    require_mgmt_auth(&headers, &state).await?;

    let root: String = sqlx::query_scalar("SELECT path FROM libraries WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let template = clean_template(body.template)?;
    let template_single = clean_template(body.template_single)?;
    let template_unknown = clean_template(body.template_unknown)?;

    if body.organize && template.is_none() {
        return Err(AppError::BadRequest(
            "an album template is required to enable organize".into(),
        ));
    }

    sqlx::query(
        "UPDATE libraries SET organize = ?, organize_template = ?, organize_template_single = ?, \
                organize_template_unknown = ?, dedupe = ? WHERE id = ?",
    )
    .bind(body.organize as i64)
    .bind(template.as_deref())
    .bind(template_single.as_deref())
    .bind(template_unknown.as_deref())
    .bind(body.dedupe as i64)
    .bind(&id)
    .execute(&state.db)
    .await?;

    // On enable, lay out everything already in the library (in the background).
    if let (true, Some(album)) = (body.organize, template) {
        let db = state.db.clone();
        let lib = id.clone();
        let root_pb = std::path::PathBuf::from(&root);
        let settings = crate::organize::OrgSettings {
            album,
            single: template_single,
            unknown: template_unknown,
            dedupe: body.dedupe,
        };
        tokio::spawn(async move {
            crate::organize::reorganize_library(&db, &lib, &root_pb, &settings).await;
        });
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/mgmt/libraries/{id}/rescan` re-reads every file under the library root in the
/// background, picking up metadata edits and added or removed files. If organise is on, files are
/// placed by the templates as they're re-indexed.
async fn rescan_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    require_mgmt_auth(&headers, &state).await?;

    let root: String = sqlx::query_scalar("SELECT path FROM libraries WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let st = state.clone();
    let lib = id.clone();
    let root_pb = std::path::PathBuf::from(&root);
    tokio::spawn(async move {
        // Pick up new/changed files, then drop entries for files deleted from disk.
        scanner::initial_scan(&st.db, &lib, &root_pb).await;
        scanner::prune_missing(&st.db, &lib).await;
        // Push the refreshed catalog (including the deletions) to the Hub right away, rather than
        // waiting for the periodic sync, so removed tracks leave browsing promptly.
        if let Err(e) = crate::catalog_sync::sync_all(&st).await {
            tracing::warn!(error = %e, "rescan: catalog sync failed");
        }
    });

    Ok(StatusCode::ACCEPTED)
}

// Directory browser.

#[derive(Deserialize)]
struct BrowseQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct DirEntry {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct BrowseResponse {
    /// Absolute path of the directory being listed.
    path: String,
    /// Parent directory within the sandbox, or null when already at the root.
    parent: Option<String>,
    /// Immediate subdirectories, sorted case-insensitively.
    dirs: Vec<DirEntry>,
}

async fn browse_dirs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BrowseQuery>,
) -> AppResult<Json<BrowseResponse>> {
    require_mgmt_auth(&headers, &state).await?;

    // All browsing is sandboxed to data/music, so users cannot escape this subtree.
    let music_root = state.config.data_dir.join("music");
    let root_canonical = std::fs::canonicalize(&music_root)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("music root unavailable: {e}")))?;

    let target = query
        .path
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| music_root.clone());

    // Canonicalize to resolve any `..` traversal attempts before checking the prefix.
    let target_canonical = std::fs::canonicalize(&target).map_err(|_| AppError::NotFound)?;

    if !target_canonical.starts_with(&root_canonical) {
        return Err(AppError::Forbidden);
    }

    if !target_canonical.is_dir() {
        return Err(AppError::BadRequest(format!(
            "not a directory: {}",
            target.display()
        )));
    }

    let mut dirs: Vec<DirEntry> = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&target_canonical).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if matches!(entry.file_type().await, Ok(ft) if ft.is_dir()) {
                dirs.push(DirEntry {
                    name,
                    path: display_path(&entry.path()),
                });
            }
        }
    }
    dirs.sort_by_key(|d| d.name.to_lowercase());

    // Parent is null when already at the sandbox root, where the back button is disabled.
    let parent = if target_canonical == root_canonical {
        None
    } else {
        target_canonical.parent().map(display_path)
    };

    Ok(Json(BrowseResponse {
        path: display_path(&target_canonical),
        parent,
        dirs,
    }))
}
