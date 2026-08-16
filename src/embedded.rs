//! Running this library inside another process, with no Hub.
//!
//! The desktop app needs to play files sitting on the machine it is running on, including with no
//! network at all. It could have grown its own scanner, its own tag reader, its own Range server —
//! a second implementation of this crate, in the shell, diverging from it immediately.
//!
//! Instead it starts this one. The client already speaks to library servers over HTTP; a library
//! server bound to `127.0.0.1` on a port the OS picked is one more of those, and every path the
//! app already has — browsing, streaming, seeking, the whole Web Audio graph — works against it
//! unchanged. Nothing in the player knows the difference.
//!
//! That the same code also runs standalone, as a server with no Hub configured, is not a side
//! effect. It is the point: a Chordia library is supposed to be useful without asking anyone's
//! permission, and until now it could not start without a Hub to point at.
//!
//! ## What is different from a normal server
//!
//! - **No Hub**, so no pairing, no heartbeat, no catalog sync, no scrobble forwarding, no acoustic
//!   identification, no acquisition. None of those workers are started.
//! - **No capability tokens.** Authorisation is [`LocalSession`] — see there for why that is the
//!   honest answer rather than a weaker one.
//! - **Loopback only.** The listener binds `127.0.0.1`, so the server is unreachable from the
//!   network even before authorisation is considered.
//! - **Nothing shells out.** See [`Config::embedded`].
//!
//! Deliberately not behind a cargo feature. The obvious place for one would be `local_session` on
//! `AppState` and the branches that read it — which are the branches that decide whether a request
//! is authorised. Conditional compilation in that code would mean the security-relevant paths differ
//! between builds, and the build a reviewer reads is not necessarily the build a user runs. The
//! whole module is a few hundred lines and the dependencies were already here.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::Context;

use crate::auth::LocalSession;
use crate::config::Config;
use crate::http::AppState;
use crate::{scanner, tls};

/// A running embedded library, and the two facts a client needs to talk to it.
pub struct Embedded {
    /// `http://127.0.0.1:{port}` is the endpoint. The port is OS-assigned and changes every run,
    /// which is why it is returned rather than configured.
    pub port: u16,
    /// The bearer token for every request. Never persisted — see [`LocalSession`].
    pub token: String,
    /// Shared with the handlers; kept so the caller can add folders through the same API.
    pub state: AppState,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Embedded {
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Stop serving. Idempotent, and also what `Drop` does — this exists for a caller that wants to
    /// wait until the socket is actually released.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a library server on loopback and return how to reach it.
///
/// Resolves once the socket is bound and the port is known, so a caller can hand the endpoint
/// straight to a client without polling for readiness.
pub async fn run(data_dir: PathBuf) -> anyhow::Result<Embedded> {
    // Needed before any rustls config is built. Idempotent, and the embedded server does not
    // terminate TLS itself — but `reqwest` inside `AppState` does, for relay pulls from peers.
    tls::install_crypto_provider();

    let config = Config::embedded(data_dir);
    let session = LocalSession::generate();
    let token = session.token().to_string();

    let state = AppState {
        local_session: Some(session),
        ..AppState::new(&config)
            .await
            .context("initialising the embedded library")?
    };

    // Port 0: the OS picks a free one. Fixing a port would mean a second Chordia — or anything else
    // on the machine — could take it first, and there is no reason to have that failure mode when
    // the port is handed to the only client that needs it.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .context("binding a loopback port for the embedded library")?;
    let port = listener.local_addr()?.port();

    resume_libraries(&state).await?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let app = crate::http::router(state.clone());
    tokio::spawn(async move {
        // `into_make_service_with_connect_info` is not decoration: it is what puts the peer address
        // in the request extensions, and `auth::from_loopback` refuses the local session without it.
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = rx.await;
        });
        if let Err(error) = served.await {
            tracing::error!(?error, "embedded library server stopped");
        }
    });

    tracing::info!(port, "embedded library listening on loopback");
    Ok(Embedded {
        port,
        token,
        state,
        shutdown: Some(tx),
    })
}

/// Re-scan the folders this library already knows about, and watch them for changes.
///
/// Mirrors what `main.rs` does at boot, and matters more here: a desktop app is closed and reopened
/// constantly, and everything added or deleted in between happened while nothing was watching.
async fn resume_libraries(state: &AppState) -> anyhow::Result<()> {
    let libraries: Vec<(String, String)> = sqlx::query_as("SELECT id, path FROM libraries")
        .fetch_all(&state.db)
        .await
        .context("loading libraries")?;

    let watched: Vec<(String, PathBuf)> = libraries
        .into_iter()
        .map(|(id, path)| (id, PathBuf::from(path)))
        .collect();

    for (id, path) in &watched {
        let db = state.db.clone();
        let (id, path) = (id.clone(), path.clone());
        // Spawned rather than awaited: a large library takes minutes to walk, and the app must be
        // able to play the tracks already indexed while that happens.
        tokio::spawn(async move {
            scanner::initial_scan(&db, &id, &path, false).await;
            scanner::prune_missing(&db, &id).await;
        });
    }

    if !watched.is_empty() {
        scanner::start_watcher(state.db.clone(), watched.clone());
        if state.config.scan.interval_minutes > 0 {
            scanner::start_scheduler(
                state.db.clone(),
                watched,
                std::time::Duration::from_secs(state.config.scan.interval_minutes * 60),
            );
        }
    }
    Ok(())
}

/// Add a folder, index it, and start watching it.
///
/// The same three calls `POST /v1/mgmt/libraries` makes, minus one: that handler refuses any path
/// outside the server's own `data/music`, and it is right to — it is reachable over the network by
/// anyone holding the management token, and without the sandbox that token would be a request to
/// index `/etc` and read it back over HTTP.
///
/// None of that describes this. The caller is the application itself, in-process, acting on a
/// folder the user just chose in their operating system's own file picker. Requiring them to move
/// their music collection into the app's data directory first would not be security; it would be an
/// app that cannot do the thing it exists to do.
pub async fn add_folder(
    state: &AppState,
    name: &str,
    path: &std::path::Path,
) -> anyhow::Result<String> {
    anyhow::ensure!(path.is_dir(), "not a folder: {}", path.display());
    let id = crate::index::upsert_library(&state.db, name, path)
        .await
        .context("registering the folder")?;

    let db = state.db.clone();
    let (scan_id, scan_path) = (id.clone(), path.to_path_buf());
    tokio::spawn(async move {
        scanner::initial_scan(&db, &scan_id, &scan_path, false).await;
    });
    scanner::start_watcher(state.db.clone(), vec![(id.clone(), path.to_path_buf())]);
    Ok(id)
}

/// Download a track from somewhere else into one of this library's folders.
///
/// The counterpart to browser downloads, and a different thing rather than a better one: a browser
/// download is a blob in IndexedDB that only that browser profile can play, while this is a file on
/// the disk, in the layout the library maintains, which the scanner indexes and any other program
/// can open. It is what "own a copy" means when there is a filesystem.
///
/// **The bytes never enter the webview.** The caller hands over a URL — a stream URL with its
/// capability token — and this fetches it here and streams it to disk. Passing a 50 MB FLAC through
/// the IPC boundary as base64 would cost several times its size in memory, twice.
///
/// Written to a temporary name in the destination folder and renamed into place, so the watcher
/// never sees a half-written file and index it as a truncated track. Same filesystem by
/// construction, so the rename is atomic.
pub async fn download_into(
    state: &AppState,
    library_id: &str,
    url: &str,
    meta: crate::organize::PlannedTrack,
) -> anyhow::Result<PathBuf> {
    let root: String = sqlx::query_scalar("SELECT path FROM libraries WHERE id = ?")
        .bind(library_id)
        .fetch_optional(&state.db)
        .await?
        .context("unknown folder")?;
    let root = PathBuf::from(root);

    let response = state
        .http
        .get(url)
        .send()
        .await
        .context("requesting the track")?;
    anyhow::ensure!(
        response.status().is_success(),
        "download failed: {}",
        response.status()
    );

    // Named after what actually arrived, not what was asked for. The caller knows which quality tier
    // it requested but not what the server chose to send - a transcoded tier is a different container
    // from the source, and a spatial track is served as the original whatever was requested. Getting
    // this wrong writes a file the scanner does not recognise as audio, which fails as a download
    // that appeared to succeed.
    let ext = extension_from(&response).context(
        "the server did not say what kind of audio this is, so there is no name for the file",
    )?;

    // The template when the folder organises itself, and a flat drop when it does not. Falling back
    // rather than refusing: someone who turned organising off wants their own layout, and the answer
    // to that is to leave the file where the scanner will find it, not to decline the download.
    let target =
        match crate::organize::planned_path(&state.db, library_id, meta.clone(), &ext).await {
            Some(path) => path,
            None => root.join(fallback_name(&meta, &ext)),
        };
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    anyhow::ensure!(
        !tokio::fs::try_exists(&target).await.unwrap_or(false),
        "already downloaded: {}",
        target.display()
    );

    let partial = target.with_extension(format!("{ext}.part"));
    let bytes = response.bytes().await.context("reading the track")?;
    tokio::fs::write(&partial, &bytes)
        .await
        .with_context(|| format!("writing {}", partial.display()))?;
    tokio::fs::rename(&partial, &target)
        .await
        .with_context(|| format!("moving {} into place", partial.display()))?;
    Ok(target)
}

/// The file extension for what the server sent, from its `Content-Type`.
///
/// A small explicit table rather than `mime_guess`'s reverse lookup, which returns whichever
/// extension happens to come first for a type and will answer `audio/mpeg` with `mpga`. These are
/// the containers this project streams, under the names it already uses on disk.
fn extension_from(response: &reqwest::Response) -> Option<String> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)?
        .to_str()
        .ok()?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    let ext = match content_type.as_str() {
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/mp4" | "audio/aac" | "audio/x-m4a" => "m4a",
        "audio/ogg" | "application/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/aiff" | "audio/x-aiff" => "aiff",
        "audio/x-ms-wma" => "wma",
        _ => return None,
    };
    Some(ext.to_string())
}

/// A safe name for a folder that does not organise itself: `Artist - Title.ext`, flat in the root.
fn fallback_name(meta: &crate::organize::PlannedTrack, ext: &str) -> String {
    let unsafe_chars = |c: char| r#"/\:*?"<>|"#.contains(c) || c.is_control();
    let clean = |s: &str| s.replace(unsafe_chars, "_").trim().to_string();
    let stem = match clean(&meta.artist).as_str() {
        "" => clean(&meta.title),
        artist => format!("{artist} - {}", clean(&meta.title)),
    };
    let stem = if stem.is_empty() {
        "Unknown".to_string()
    } else {
        stem
    };
    if ext.is_empty() {
        stem
    } else {
        format!("{stem}.{ext}")
    }
}

/// Forget a folder. The files on disk are never touched.
///
/// `library_tracks` cascades from `libraries`, so the membership rows go with it; the `tracks` rows
/// themselves are left, because a track can belong to more than one folder and the scanner's prune
/// pass already collects the ones nothing points at any more. Deleting them here would be a second,
/// worse implementation of that — one that could take a track another folder still holds.
pub async fn remove_folder(state: &AppState, id: &str) -> anyhow::Result<()> {
    // SQLite enforces foreign keys per connection and only when asked, so the cascade above is not
    // guaranteed to have fired on whichever connection this lands on. Explicit is cheaper than
    // finding out later that removing a folder left its tracks listed.
    sqlx::query("DELETE FROM library_tracks WHERE library_id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .context("removing the folder's tracks")?;
    sqlx::query("DELETE FROM libraries WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .context("removing the folder")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three things a Hub-less configuration must not do, asserted where they are decided
    /// rather than discovered later as "why is it trying to run ffmpeg".
    #[test]
    fn the_embedded_config_needs_no_hub_and_no_binaries() {
        let config = Config::embedded(PathBuf::from("./data"));
        assert!(config.backend_url.is_none());
        assert!(!config.loudness.enabled);
        assert!(!config.acquisition.enabled);
        // Local, because there is nowhere to push a catalog to.
        assert_eq!(
            config.metadata_storage,
            crate::config::MetadataStorage::Local
        );
    }

    /// The session is the whole authorisation story for an embedded library, so its comparison is
    /// worth pinning: a prefix must not pass, and neither must a longer string that starts with it.
    #[test]
    fn a_local_session_only_matches_itself() {
        let session = LocalSession::generate();
        let token = session.token().to_string();
        assert!(session.matches(&token));
        assert!(!session.matches(&token[..token.len() - 1]));
        assert!(!session.matches(&format!("{token}x")));
        assert!(!session.matches(""));
        // Two runs never share one.
        assert!(!session.matches(LocalSession::generate().token()));
    }
}
