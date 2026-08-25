//! Audio streaming endpoint - the data plane.
//!
//! The `CapToken` extractor accepts `Authorization: Bearer <token>` OR `?token=<token>` (for
//! HTML `<audio>` elements that cannot set custom headers).

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use chordia_contracts::auth::{CapabilityAction, CapabilityClaims, ResourceRef};
use chordia_contracts::streaming::{QualityProfile, StreamQuery};

use crate::auth::{require_action, CapToken};
use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::http::AppState;
use crate::streaming;

/// Verify the capability token's `library_id` claim matches the library that owns `track_id`.
///
/// This used to exempt libraries with no `hub_library_id` (a pre-M4 backwards-compatibility carve
/// out), which inverted the check: an unlinked local library matched *every* token, so a capability
/// token legitimately minted for library A streamed its tracks too. Linking is a separate `PATCH`
/// from creation (`mgmt::create_library` writes `hub_library_id: None`), so that window is real and
/// permanent for any library whose link call never lands.
///
/// An unlinked library is now simply unstreamable, which is the correct reading: the Hub cannot have
/// issued a token scoped to a library it does not know about.
///
/// Returns the LOCAL library id the track belongs to, which the caller needs for the per-grantee
/// folder check below.
async fn check_library_scope(
    db: &sqlx::SqlitePool,
    track_id: &str,
    hub_library_id: &str,
) -> AppResult<String> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT lt.library_id FROM library_tracks lt \
         JOIN libraries l ON l.id = lt.library_id \
         WHERE lt.track_id = ? AND l.hub_library_id = ? LIMIT 1",
    )
    .bind(track_id)
    .bind(hub_library_id)
    .fetch_optional(db)
    .await
    .map_err(|e| AppError::Internal(e.into()))?;

    row.ok_or(AppError::Forbidden)
}

/// Verify the token's `resource` claim actually covers the track being requested.
///
/// **This check did not exist.** `CapabilityClaims.resource` was minted, signed, and then never
/// read — so a token the Hub had deliberately narrowed to one track was enforced exactly like one
/// scoped to the whole library. Nothing mints a narrowed token today, which is the only reason this
/// was not already an incident; it also means turning the check on breaks nothing.
///
/// The two narrow variants are handled differently, and the difference is the honest part:
///
/// - **Track.** Enforced, against the LIBRARY's own track id — the same value that appears in the
///   stream URL, and the only id the library can resolve. That is now the documented meaning of the
///   claim (see `ResourceRef` in contracts); it was previously undefined because nothing read it.
/// - **Album.** REFUSED. The Hub's album ids and the library's are different id spaces, and the
///   library stores no mapping between them, so it cannot tell whether a track is in the album the
///   token names. A check it cannot perform must fail closed rather than wave the request through
///   as the old code did — a narrower token silently granting MORE than a wide one is the worst
///   possible reading.
fn check_resource_scope(claims: &CapabilityClaims, track_id: &str) -> AppResult<()> {
    match &claims.resource {
        // The wide grant. `check_library_scope` has already proved the track is in this library, and
        // the claim naming a different library than the token's own `library_id` is a malformed
        // token rather than a narrower one.
        ResourceRef::Library { library_id } => {
            if *library_id != claims.library_id {
                return Err(AppError::Forbidden);
            }
            Ok(())
        }
        ResourceRef::Track {
            track_id: allowed, ..
        } => {
            // The claim is a `Uuid` and the path segment is whatever the client sent. A value that
            // is not a UUID cannot be the track this token names, so it is refused rather than
            // compared as text — string equality against a malformed id is the kind of comparison
            // that looks right and is not.
            match uuid::Uuid::parse_str(track_id) {
                Ok(requested) if requested == *allowed => Ok(()),
                _ => Err(AppError::Forbidden),
            }
        }
        ResourceRef::Album { .. } => Err(AppError::Forbidden),
    }
}

/// Is this file in a folder withheld from the person the token authorizes?
///
/// A library can be shared with someone while keeping part of it back — see
/// `0021_share_dir_exclusions.sql` for why the paths live here rather than on the Hub, and why this
/// is a different mechanism from the library's own scan-time exclusions.
///
/// Keyed on the token's `sub`, so it applies to whoever the Hub authorized and to nobody else. The
/// owner streaming their own library has no rows and pays one indexed lookup that returns nothing.
///
/// Reuses `scanner::is_excluded` rather than comparing strings here: it is case-insensitive,
/// separator-agnostic and boundary-aware, so `/music/Live` does not withhold `/music/Livewire`. A
/// second implementation of that comparison is exactly how one of them ends up subtly wrong.
async fn check_folder_exclusions(
    db: &sqlx::SqlitePool,
    local_library_id: &str,
    subject: &str,
    path: &str,
) -> AppResult<()> {
    let excluded: Vec<String> = sqlx::query_scalar(
        "SELECT path FROM library_share_excluded_dirs \
         WHERE library_id = ? AND grantee_user_id = ?",
    )
    .bind(local_library_id)
    .bind(subject)
    .fetch_all(db)
    .await
    .map_err(|e| AppError::Internal(e.into()))?;

    if crate::scanner::is_excluded(std::path::Path::new(path), &excluded) {
        // `Forbidden`, not `NotFound`. The grantee can see the track in the catalog — the Hub's copy
        // is not filtered — so pretending it does not exist would be a lie they can check.
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Everything a Hub-signed capability token has to satisfy before a byte is served.
///
/// ONE function rather than three calls in the handler, deliberately: this is the whole of the
/// data-plane authorisation decision, and a reviewer should be able to read it in one place and a
/// test should be able to exercise it without a socket. The handler's use of it is a single line.
///
/// Three questions, and until now only the first was asked:
///
/// 1. Is this track in the library the token names? (was checked)
/// 2. Does the token's `resource` claim actually cover it? (was signed and ignored)
/// 3. Is it in a folder withheld from this particular person? (did not exist)
async fn authorize(
    db: &sqlx::SqlitePool,
    claims: &CapabilityClaims,
    track_id: &str,
    path: &str,
) -> AppResult<()> {
    let local_library_id =
        check_library_scope(db, track_id, &claims.library_id.to_string()).await?;
    check_resource_scope(claims, track_id)?;
    check_folder_exclusions(db, &local_library_id, &claims.sub.to_string(), path).await
}

pub fn router() -> Router<AppState> {
    Router::new().route("/stream/{track_id}", get(stream))
}

/// `GET /v1/stream/{track_id}?profile=original|high|normal|data_saver[&token=<cap_token>]`
///
/// Requires a `StreamRead` capability token. `Original` (the default) is byte-for-byte lossless
/// passthrough; lower tiers are transcoded on the fly (ffmpeg) and cached. Spatial/Atmos tracks
/// are always served as `Original` - they are passthrough-only and never transcoded.
async fn stream(
    State(state): State<AppState>,
    token: CapToken,
    Path(track_id): Path<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> AppResult<axum::response::Response> {
    require_action(&token, CapabilityAction::StreamRead)?;

    let meta = catalog::get_track_meta(&state.db, &track_id)
        .await?
        .ok_or(AppError::NotFound)?;

    // An embedded library has no Hub, so it has no `hub_library_id` to scope against and no Hub
    // that could have minted a token narrower than "this machine's own music". The checks are not
    // relaxed here — there is simply no statement to check. See `auth::LocalSession`.
    if !token.local {
        authorize(&state.db, &token.claims, &track_id, &meta.path).await?;
    }

    let source = std::path::Path::new(&meta.path);

    // Spatial/Atmos is passthrough-only; force the original bitstream regardless of the request.
    let want_transcode = q.profile != QualityProfile::Original && !meta.spatial;
    if want_transcode {
        if let Some(t) = state
            .transcoder
            .ensure(source, &meta.content_hash, q.profile)
            .await?
        {
            return streaming::serve_range(
                &t.path,
                &t.etag,
                t.content_codec,
                &headers,
                state.config.max_stream_kbps,
            )
            .await
            .map_err(AppError::Internal);
        }
    }

    // Original tier (or spatial passthrough).
    streaming::serve_range(
        source,
        &meta.content_hash,
        &meta.codec,
        &headers,
        state.config.max_stream_kbps,
    )
    .await
    .map_err(AppError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chordia_contracts::auth::CapabilityAction;
    use chordia_contracts::library::PermissionLevel;
    use sqlx::SqlitePool;
    use uuid::Uuid;

    const OWNER: &str = "11111111-1111-4111-8111-111111111111";
    const FRIEND: &str = "22222222-2222-4222-8222-222222222222";

    /// The tables `authorize` reasons over. Only the columns it touches, like `scanner`'s helper.
    async fn mem_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for ddl in [
            "CREATE TABLE libraries (id TEXT PRIMARY KEY, hub_library_id TEXT)",
            "CREATE TABLE library_tracks (library_id TEXT NOT NULL, track_id TEXT NOT NULL, \
             PRIMARY KEY (library_id, track_id))",
            "CREATE TABLE library_share_excluded_dirs (library_id TEXT NOT NULL, \
             grantee_user_id TEXT NOT NULL, path TEXT NOT NULL, \
             PRIMARY KEY (library_id, grantee_user_id, path))",
        ] {
            sqlx::query(ddl).execute(&db).await.unwrap();
        }
        db
    }

    /// A library linked to `hub_id`, holding one track.
    async fn seed(db: &SqlitePool, hub_id: Uuid, track: &str) {
        sqlx::query("INSERT INTO libraries (id, hub_library_id) VALUES ('lib-1', ?)")
            .bind(hub_id.to_string())
            .execute(db)
            .await
            .unwrap();
        add_track(db, track).await;
    }

    async fn add_track(db: &SqlitePool, track: &str) {
        sqlx::query("INSERT INTO library_tracks (library_id, track_id) VALUES ('lib-1', ?)")
            .bind(track)
            .execute(db)
            .await
            .unwrap();
    }

    async fn withhold(db: &SqlitePool, user: &str, path: &str) {
        sqlx::query(
            "INSERT INTO library_share_excluded_dirs (library_id, grantee_user_id, path) \
             VALUES ('lib-1', ?, ?)",
        )
        .bind(user)
        .bind(path)
        .execute(db)
        .await
        .unwrap();
    }

    fn claims(hub_library: Uuid, subject: &str, resource: ResourceRef) -> CapabilityClaims {
        CapabilityClaims {
            sub: subject.parse().unwrap(),
            aud: Uuid::nil(),
            library_id: hub_library,
            resource,
            action: CapabilityAction::StreamRead,
            permission_level: PermissionLevel::Read,
            room_id: None,
            jti: Uuid::now_v7(),
            iat: 0,
            exp: 0,
            kid: String::new(),
        }
    }

    #[tokio::test]
    async fn the_ordinary_case_is_allowed() {
        let db = mem_db().await;
        let hub = Uuid::now_v7();
        let track = Uuid::now_v7().to_string();
        seed(&db, hub, &track).await;

        let c = claims(hub, FRIEND, ResourceRef::Library { library_id: hub });
        assert!(authorize(&db, &c, &track, "/music/rock/a.flac")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_token_for_another_library_is_refused() {
        // The check that already existed. Kept because the rewrite changed its return type, and a
        // scope check that quietly stops working is the worst regression this file could carry.
        let db = mem_db().await;
        let hub = Uuid::now_v7();
        let track = Uuid::now_v7().to_string();
        seed(&db, hub, &track).await;

        let other = Uuid::now_v7();
        let c = claims(other, FRIEND, ResourceRef::Library { library_id: other });
        assert!(authorize(&db, &c, &track, "/music/a.flac").await.is_err());
    }

    #[tokio::test]
    async fn a_track_scoped_token_reaches_only_that_track() {
        // The claim that was signed and never read. Before this, a token narrowed to one track
        // streamed the whole library.
        let db = mem_db().await;
        let hub = Uuid::now_v7();
        let allowed = Uuid::now_v7();
        let other = Uuid::now_v7().to_string();
        seed(&db, hub, &allowed.to_string()).await;
        add_track(&db, &other).await;

        let c = claims(hub, FRIEND, ResourceRef::Track { track_id: allowed });
        assert!(authorize(&db, &c, &allowed.to_string(), "/music/a.flac")
            .await
            .is_ok());
        assert!(
            authorize(&db, &c, &other, "/music/b.flac").await.is_err(),
            "a track-scoped token must not reach a second track in the same library"
        );
    }

    #[tokio::test]
    async fn a_malformed_track_id_is_refused_rather_than_compared_as_text() {
        let db = mem_db().await;
        let hub = Uuid::now_v7();
        let allowed = Uuid::now_v7();
        seed(&db, hub, "not-a-uuid").await;

        let c = claims(hub, FRIEND, ResourceRef::Track { track_id: allowed });
        assert!(authorize(&db, &c, "not-a-uuid", "/music/a.flac")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_album_scoped_token_fails_closed() {
        // The library cannot resolve a HUB album id against its own, so it cannot answer the
        // question the claim asks. The old code answered "yes" to every question it could not ask,
        // which made a narrower token grant MORE than a wide one.
        let db = mem_db().await;
        let hub = Uuid::now_v7();
        let track = Uuid::now_v7().to_string();
        seed(&db, hub, &track).await;

        let c = claims(
            hub,
            FRIEND,
            ResourceRef::Album {
                album_id: Uuid::now_v7(),
            },
        );
        assert!(authorize(&db, &c, &track, "/music/a.flac").await.is_err());
    }

    #[tokio::test]
    async fn a_withheld_folder_is_refused_for_that_person_only() {
        let db = mem_db().await;
        let hub = Uuid::now_v7();
        let track = Uuid::now_v7().to_string();
        seed(&db, hub, &track).await;
        withhold(&db, FRIEND, "/music/Bootlegs").await;

        let friend = claims(hub, FRIEND, ResourceRef::Library { library_id: hub });
        assert!(
            authorize(&db, &friend, &track, "/music/Bootlegs/x.flac")
                .await
                .is_err(),
            "a withheld folder must not stream"
        );
        assert!(
            authorize(&db, &friend, &track, "/music/Albums/x.flac")
                .await
                .is_ok(),
            "the rest of the library still streams"
        );

        // Somebody else's exclusions are not this person's. The feature is per-grantee, and a rule
        // that leaked between people would be worse than no rule at all.
        let owner = claims(hub, OWNER, ResourceRef::Library { library_id: hub });
        assert!(authorize(&db, &owner, &track, "/music/Bootlegs/x.flac")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_withheld_folder_matches_on_a_path_boundary() {
        // `/music/Live` must not withhold `/music/Livewire`. `scanner::is_excluded` is reused
        // precisely so that reasoning exists once; this pins that it is still being reused.
        let db = mem_db().await;
        let hub = Uuid::now_v7();
        let track = Uuid::now_v7().to_string();
        seed(&db, hub, &track).await;
        withhold(&db, FRIEND, "/music/Live").await;

        let c = claims(hub, FRIEND, ResourceRef::Library { library_id: hub });
        assert!(authorize(&db, &c, &track, "/music/Live/a.flac")
            .await
            .is_err());
        assert!(authorize(&db, &c, &track, "/music/Livewire/a.flac")
            .await
            .is_ok());
        // Case and separator, which is how the same folder is spelled on Windows.
        assert!(authorize(&db, &c, &track, "\\music\\LIVE\\a.flac")
            .await
            .is_err());
    }
}
