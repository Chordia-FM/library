//! End-to-end coverage for the Hub-less mode the desktop app runs on.
//!
//! Everything here is reachable only through a real socket, which is the point: the local session
//! is the *entire* authorisation story for an embedded library, and three of its four conditions
//! (a session exists, the request is loopback, the bytes match) live in an axum extractor that a
//! unit test cannot reach. A mistake in any of them is either an app that cannot play its own music
//! or a music server on someone's laptop that answers to anybody who can reach the port.
//!
//! Real files, real migrations, real HTTP. The one thing not exercised is a non-loopback peer,
//! because the listener binds `127.0.0.1` and there is no way to produce one.

use std::path::Path;

use chordia_library::embedded;

/// Start an embedded library over a throwaway data directory containing one real audio file.
///
/// The file is the crate's own `tiny.flac` fixture, copied into a folder that is then added the way
/// the desktop app adds one — outside the data directory, which is the case the management API
/// refuses and this path deliberately allows.
async fn library() -> (embedded::Embedded, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let music = dir.path().join("Music");
    std::fs::create_dir_all(&music).expect("music dir");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/testdata/tiny.flac"),
        music.join("tiny.flac"),
    )
    .expect("fixture");

    let server = embedded::run(dir.path().join("data"))
        .await
        .expect("embedded library starts");
    embedded::add_folder(&server.state, "Music", &music)
        .await
        .expect("folder is added");
    (server, dir)
}

/// Wait for the initial scan to put the file in the catalog. The scan is spawned, so a fresh server
/// legitimately has an empty catalog for a moment.
async fn wait_for_a_track(server: &embedded::Embedded) -> String {
    for _ in 0..100 {
        let found: Option<String> = sqlx::query_scalar("SELECT id FROM tracks LIMIT 1")
            .fetch_optional(&server.state.db)
            .await
            .expect("query");
        if let Some(id) = found {
            return id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the initial scan never indexed the fixture");
}

#[tokio::test]
async fn a_folder_outside_the_data_directory_is_indexed_and_streams() {
    let (server, _dir) = library().await;
    let track = wait_for_a_track(&server).await;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/stream/{track}", server.endpoint()))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("request");

    assert!(response.status().is_success(), "{:?}", response.status());
    // Range support is what the player seeks with, and it comes from the same handler a paired
    // library uses — worth asserting that the local path did not bypass it.
    assert_eq!(
        response
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok()),
        Some("bytes")
    );
    assert!(!response.bytes().await.expect("body").is_empty());
}

#[tokio::test]
async fn the_stream_is_useless_without_the_session_token() {
    // The port is on the machine, so anything else running on it can reach the socket. What it
    // cannot do is guess a 43-character secret that was never written down.
    let (server, _dir) = library().await;
    let track = wait_for_a_track(&server).await;
    let url = format!("{}/v1/stream/{track}", server.endpoint());
    let http = reqwest::Client::new();

    for attempt in [
        None,
        Some("".to_string()),
        Some("not-the-token".to_string()),
    ] {
        let request = http.get(&url);
        let request = match &attempt {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        let status = request.send().await.expect("request").status();
        assert_eq!(status, 401, "accepted {attempt:?}");
    }
}

#[tokio::test]
async fn the_query_token_works_for_an_audio_element() {
    // `<audio src>` cannot set a header, so the stream endpoint also takes `?token=`. Every desktop
    // playback goes through this path rather than the header one.
    let (server, _dir) = library().await;
    let track = wait_for_a_track(&server).await;

    let status = reqwest::Client::new()
        .get(format!(
            "{}/v1/stream/{track}?token={}",
            server.endpoint(),
            server.token
        ))
        .send()
        .await
        .expect("request")
        .status();
    assert!(status.is_success(), "{status:?}");
}

#[tokio::test]
async fn management_answers_to_the_session_and_nothing_else() {
    // An embedded library was never paired, so it has no management token — the credential this
    // endpoint normally wants does not exist. Without the session standing in, the folder list the
    // settings screen renders would be permanently empty.
    let (server, _dir) = library().await;
    let url = format!("{}/v1/mgmt/libraries", server.endpoint());
    let http = reqwest::Client::new();

    let response = http
        .get(&url)
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("request");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body.as_array().map(Vec::len), Some(1));

    assert_eq!(
        http.get(&url).send().await.expect("request").status(),
        401,
        "management is open to unauthenticated callers"
    );
}

#[tokio::test]
async fn the_matcher_answers_the_session_and_nothing_else() {
    // `/v1/tracks/match` took no credential at all, which on a loopback server with `Allow-Origin:
    // *` meant any page the user visited could ask what music is on their disk. It is the only
    // endpoint the desktop app uses to answer "do I already own this", so it has to keep working
    // *with* the session and stop working without it.
    let (server, _dir) = library().await;
    let track = wait_for_a_track(&server).await;
    let hash: String = sqlx::query_scalar("SELECT content_hash FROM tracks WHERE id = ?")
        .bind(&track)
        .fetch_one(&server.state.db)
        .await
        .expect("hash");
    let url = format!("{}/v1/tracks/match?content_hash={hash}", server.endpoint());
    let http = reqwest::Client::new();

    let body: serde_json::Value = http
        .get(&url)
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(body["track"]["id"].as_str(), Some(track.as_str()));

    assert_eq!(
        http.get(&url).send().await.expect("request").status(),
        401,
        "the matcher is open to unauthenticated callers"
    );
}

#[tokio::test]
async fn the_readiness_probe_is_not_a_port_oracle() {
    // `/v1/ping` reports paired status and folder count, which is how a page scanning the ephemeral
    // range recognises this server. A real server must answer it unauthenticated (the pairing wizard
    // probes it before any credential exists); an embedded one has no wizard and must not.
    let (server, _dir) = library().await;
    let url = format!("{}/v1/ping", server.endpoint());
    let http = reqwest::Client::new();

    assert!(http
        .get(&url)
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("request")
        .status()
        .is_success());

    assert_eq!(
        http.get(&url).send().await.expect("request").status(),
        401,
        "ping identifies the embedded library to any caller"
    );
}

#[tokio::test]
async fn a_second_run_gets_a_different_token() {
    // The token lives and dies with the process. A stable one would be a credential worth stealing
    // from a memory dump or a log line months later; this one is worthless the moment the app exits.
    let (first, _a) = library().await;
    let (second, _b) = library().await;
    assert_ne!(first.token, second.token);
    assert_ne!(first.port, second.port);
}
