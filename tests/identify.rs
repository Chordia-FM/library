//! How this library reads the Hub's answers to `POST /v1/catalog/identify`.
//!
//! The AcoustID key moved to the Hub, so a library now has to cope with a Hub that has no key. That
//! is a supported deployment, not a fault, and the failure mode this suite exists to prevent is
//! subtle: if "the Hub does not do identification" were folded into either "no match" or "the
//! request failed", the library would keep fingerprinting and re-asking forever for an answer that
//! can never come — or, worse, mark every track in the library as unidentifiable.
//!
//! These drive a real HTTP server rather than a mock, because the thing under test *is* the
//! status-code mapping. A mock would just return whatever the test asked for.

use std::net::SocketAddr;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use chordia_contracts::identify::{IdentifyRequest, IdentifyResponse};
use chordia_library::pairing::{HubClient, IdentifyOutcome};

/// Serve one fixed reply on `/v1/catalog/identify` and return the base URL of the fake Hub.
async fn fake_hub(reply: Response) -> String {
    let reply = std::sync::Arc::new(std::sync::Mutex::new(Some(reply)));
    let app = Router::new().route(
        "/v1/catalog/identify",
        post(move || {
            let reply = reply.clone();
            async move {
                reply
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| StatusCode::TOO_MANY_REQUESTS.into_response())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind fake hub");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn request() -> IdentifyRequest {
    IdentifyRequest {
        fingerprint: "AQABz0mUSEkSRRcOLA".into(),
        duration_ms: 215_000,
        title: Some("Diablo".into()),
        artist: None,
        album: None,
    }
}

async fn ask(reply: Response) -> anyhow::Result<IdentifyOutcome> {
    let base = fake_hub(reply).await;
    HubClient::new(base, reqwest::Client::new())
        .identify("test-server-key", &request())
        .await
}

/// A Hub with no AcoustID key answers `501`. That must degrade **quietly**: not an `Err` (which
/// would be logged as a failure and retried forever) and not `NoMatch` (which would look like a
/// successful lookup and keep the worker fingerprinting the rest of the library for nothing).
#[tokio::test]
async fn a_hub_without_an_acoustid_key_degrades_quietly() {
    let outcome = ask(StatusCode::NOT_IMPLEMENTED.into_response())
        .await
        .expect("a keyless Hub must not surface as an error - it is a supported deployment");

    assert!(
        matches!(outcome, IdentifyOutcome::NotConfigured),
        "a 501 must be recognized as 'this Hub does not do identification', got {outcome:?}"
    );
}

/// `204` is a real answer: AcoustID was asked and has never heard this fingerprint. It must stay
/// distinct from `NotConfigured`, because the library reacts to them differently — one track is
/// skipped, versus the whole worker shutting down.
#[tokio::test]
async fn no_match_is_distinct_from_not_configured() {
    let outcome = ask(StatusCode::NO_CONTENT.into_response()).await.unwrap();
    assert!(
        matches!(outcome, IdentifyOutcome::NoMatch),
        "a 204 is 'no match', got {outcome:?}"
    );
}

/// A successful identification carries the fields an untagged import is missing — the album is the
/// whole reason this pass exists.
#[tokio::test]
async fn an_identified_fingerprint_carries_the_album() {
    let body = IdentifyResponse {
        acoustid: "acid-1".into(),
        recording_mbid: Some("rec-1".into()),
        album: Some("Faces".into()),
        release_mbid: Some("rel-1".into()),
        track_no: Some(13),
        ..Default::default()
    };
    let outcome = ask(Json(body).into_response()).await.unwrap();

    let IdentifyOutcome::Identified(id) = outcome else {
        panic!("a 200 must be an identification, got {outcome:?}");
    };
    assert_eq!(id.acoustid, "acid-1");
    assert_eq!(id.album.as_deref(), Some("Faces"));
    assert_eq!(id.track_no, Some(13));
}

/// A provider or transport failure must NOT become a quiet `NoMatch`. The library has to be able to
/// tell "retry this later" from "there is nothing to find" — collapsing them is how a dead provider
/// once stayed invisible for four days.
#[tokio::test]
async fn a_provider_failure_is_an_error_not_a_no_match() {
    let err = ask(StatusCode::BAD_GATEWAY.into_response())
        .await
        .expect_err("a 502 must surface as a retryable error");
    assert!(
        err.to_string().contains("502"),
        "the error should name the status so the log is actionable: {err}"
    );

    // An unreachable Hub is the same class of answer.
    let unreachable = HubClient::new("http://127.0.0.1:9".to_string(), reqwest::Client::new())
        .identify("test-server-key", &request())
        .await;
    assert!(
        unreachable.is_err(),
        "an unreachable Hub must be an error, not 'no match'"
    );
}
