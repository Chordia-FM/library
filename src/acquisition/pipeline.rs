//! Per-job acquisition pipeline: search → score → grab → monitor → import.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use chordia_contracts::acquisition::{CandidateInput, ClaimedJob, JobCandidates};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::{failed, local_library, quality, status, AcquisitionClient, Release};
use crate::http::AppState;
use crate::pairing::HubClient;
use crate::scanner;

/// Abandon a download that made SOME progress then flatlined for this long (slow/dying swarm).
const STALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Abandon a download that never connected to a seed and never moved off 0% for this long: a dead
/// swarm (stale tracker seed counts, nothing reachable). Short, so we fall through to the next-best
/// candidate quickly instead of waiting out the full stall timeout.
const NO_PEERS_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// At most this many ranked candidates are tried automatically before a job is reported failed.
const MAX_CANDIDATE_ATTEMPTS: usize = 5;

/// How a monitored download ended. Distinguishes "move on to the next candidate" from "stop".
enum MonitorOutcome {
    /// Files imported into the library; the job is done.
    Imported,
    /// Cancelled / terminated on the Hub. The torrent was torn down; stop without failing.
    Cancelled,
    /// Dead swarm, stall, or mismatched content: the torrent was removed, and the caller should try
    /// the next-best candidate (and fail the job only once none are left). Carries the specific reason
    /// when the rejection was about CONTENT (e.g. a single-track job whose release doesn't contain the
    /// requested track), so the final failure can tell the truth instead of blaming seeders.
    Abandoned(Option<String>),
}

/// Run one claimed job end to end, reporting status back to the Hub. Failures are reported, not
/// propagated.
pub async fn run_job(state: &AppState, job: ClaimedJob) {
    let Some(creds) = state.credentials.read().await.clone() else {
        return;
    };
    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());
    if let Err(e) = run_inner(state, &hub, &creds.server_api_key, &job).await {
        tracing::warn!(job = %job.job_id, error = %e, "download job failed");
        // Tear down any torrent grabbed before the failure so a failed job never orphans a download.
        // Keep the resume bookkeeping if removal fails (transient qBittorrent error) so a later resume
        // re-attempts it; only forget the job once there's nothing left to remove.
        let mut removed = true;
        if let Ok(Some(hash)) = super::prior_hash(&state.db, job.job_id).await {
            removed = match AcquisitionClient::from_config(&state.config.acquisition) {
                Some(client) => client.remove_on_teardown(&hash).await.is_ok(),
                None => false,
            };
        }
        let _ = hub
            .report_job_status(&creds.server_api_key, job.job_id, &failed(&e.to_string()))
            .await;
        if removed {
            let _ = super::clear_job(&state.db, job.job_id).await;
        }
    }
}

/// Re-attach the monitor to a download that was in flight before a library restart.
pub async fn resume_job(state: &AppState, job_id: Uuid, hash: String, hub_library_id: Uuid) {
    let Some(creds) = state.credentials.read().await.clone() else {
        return;
    };
    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());
    let outcome = async {
        let client = AcquisitionClient::from_config(&state.config.acquisition)
            .ok_or_else(|| anyhow::anyhow!("acquisition not configured"))?;
        // If the job was cancelled/failed (or retried, then re-grabbed) on the Hub while the library was
        // down, don't re-attach a monitor to the stale torrent. Remove it + its files and forget it.
        if !hub
            .job_active(&creds.server_api_key, job_id)
            .await
            .unwrap_or(true)
        {
            if client.remove_on_teardown(&hash).await.is_ok() {
                let _ = super::clear_job(&state.db, job_id).await;
            }
            return Ok(MonitorOutcome::Cancelled);
        }
        let (local_id, music_root) = local_library(&state.db, hub_library_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("target library no longer hosted here"))?;
        monitor_to_completion(
            state,
            &hub,
            &creds.server_api_key,
            &client,
            job_id,
            &local_id,
            &music_root,
            &hash,
            &[], // resumed downloads carry no expected tracklist; verification ran on the first pass
            &[], // and no track filter — a resume imports whatever the first pass grabbed
        )
        .await
    }
    .await;
    match outcome {
        // Imported, or cancelled/torn-down on the Hub: nothing left to do.
        Ok(MonitorOutcome::Imported | MonitorOutcome::Cancelled) => {}
        // The resumed swarm went dead/stalled. The monitor already removed the torrent + cleared
        // bookkeeping; there's no candidate list to fall back on across a restart, so report it failed
        // and let the user retry (which re-searches fresh).
        Ok(MonitorOutcome::Abandoned(_)) => {
            let _ = hub
                .report_job_status(
                    &creds.server_api_key,
                    job_id,
                    &failed("download stalled: no live seeders"),
                )
                .await;
        }
        Err(e) => {
            tracing::warn!(job = %job_id, error = %e, "download resume failed");
            // Symmetric with run_job: tear down the torrent, keep the bookkeeping if removal fails.
            let removed = match AcquisitionClient::from_config(&state.config.acquisition) {
                Some(client) => client.remove_on_teardown(&hash).await.is_ok(),
                None => false,
            };
            let _ = hub
                .report_job_status(&creds.server_api_key, job_id, &failed(&e.to_string()))
                .await;
            if removed {
                let _ = super::clear_job(&state.db, job_id).await;
            }
        }
    }
}

async fn run_inner(
    state: &AppState,
    hub: &HubClient,
    api_key: &str,
    job: &ClaimedJob,
) -> anyhow::Result<()> {
    let client = AcquisitionClient::from_config(&state.config.acquisition)
        .ok_or_else(|| anyhow::anyhow!("acquisition not configured"))?;
    let (local_id, music_root) = local_library(&state.db, job.library_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("target library is not hosted on this server"))?;

    // Resolve the ORDERED list of releases to try. A dead/stalled swarm falls through to the next-best
    // automatically (the user's "try the next one"), so we keep the whole ranked list, not just the top.
    //
    // Which `chosen_guid`s are real USER PICKS? An auto-grab also records the guid it grabbed (for
    // display), so pinning on the guid alone left auto jobs permanently stuck re-searching a guid
    // Prowlarr no longer returns. But NON-interactive jobs can also legitimately end up in
    // `awaiting_selection` (an unsure auto search hands over candidates) — and gating on
    // `interactive` alone DISCARDED the user's pick on those, re-searching and re-asking forever.
    // The stored source is the reliable tell: only the Hub's select-candidate path copies
    // `chosen_download_url`/`chosen_magnet_url` onto the job; status-report recordings never do.
    let pick_has_source = job.chosen_download_url.is_some() || job.chosen_magnet_url.is_some();
    let picked = job
        .chosen_guid
        .as_deref()
        .filter(|_| job.interactive || pick_has_source);
    let candidates: Vec<Release> = if let Some(guid) = picked {
        hub.report_job_status(api_key, job.job_id, &status("searching"))
            .await?;
        // Prefer the SOURCE stored at selection time: it grabs directly with no freshness window.
        // Only a legacy pick (made before candidate sources were persisted) falls back to
        // re-finding the guid via a live search — which only works while Prowlarr still returns
        // that release. Their explicit pick is the only one we try either way: if it's dead, fail
        // rather than silently grab something they didn't choose.
        if pick_has_source {
            vec![Release {
                guid: guid.to_string(),
                title: job
                    .chosen_title
                    .clone()
                    .or_else(|| job.display_title.clone())
                    .unwrap_or_default(),
                download_url: job.chosen_download_url.clone(),
                magnet_url: job.chosen_magnet_url.clone(),
                info_hash: job.chosen_info_hash.clone(),
                size: job.chosen_size_bytes.unwrap_or(0),
                seeders: job.chosen_seeders.unwrap_or(0),
                leechers: 0,
                indexer: job.chosen_indexer.clone(),
            }]
        } else {
            vec![find_chosen(&client, job, guid).await?]
        }
    } else {
        hub.report_job_status(api_key, job.job_id, &status("searching"))
            .await?;
        // Prowlarr returns fuzzy results; `search_relevant` keeps everything PLAUSIBLY about the
        // requested artist+album (e.g. drops a "Hot Rize" for a Mac Miller request) and retries on the
        // core album title when the full one finds nothing.
        let releases = search_relevant(&client, job).await?;
        if releases.is_empty() {
            hub.report_job_status(api_key, job.job_id, &status("no_results"))
                .await?;
            return Ok(());
        }
        let artist = job.artist_name.as_deref();
        let album = job.album_title.as_deref();
        // Auto-grab ONLY releases we're CONFIDENT are the exact album (right artist + distinctive words +
        // exact volume, not an instrumental/variant), best quality first — no questions asked.
        let mut confident: Vec<Release> = releases
            .iter()
            .filter(|r| is_confident_match(&r.title, artist, album))
            .cloned()
            .collect();
        quality::rank(&mut confident, job.quality_profile.as_ref());
        if !job.interactive && !confident.is_empty() {
            confident.truncate(MAX_CANDIDATE_ATTEMPTS);
            confident
        } else {
            // Not sure (ambiguous title, only a different volume, odd naming) or the user opted to
            // choose: hand the candidates over to pick from, closest match first. Never risk the wrong
            // album by guessing. The Hub flips the job to `awaiting_selection` and re-queues it (with
            // `chosen_guid`) once they pick.
            let mut ranked = releases;
            quality::rank(&mut ranked, job.quality_profile.as_ref());
            if ranked.is_empty() {
                // Nothing downloadable (no seeders / disallowed format): report no_results rather than
                // an empty pick-list the user can't act on.
                hub.report_job_status(api_key, job.job_id, &status("no_results"))
                    .await?;
                return Ok(());
            }
            ranked.sort_by(|a, b| {
                let (ca, cb) = (
                    match_closeness(&a.title, artist, album),
                    match_closeness(&b.title, artist, album),
                );
                cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
            });
            let candidates: Vec<CandidateInput> =
                ranked.iter().take(20).map(to_candidate).collect();
            hub.report_candidates(api_key, job.job_id, &JobCandidates { candidates })
                .await?;
            return Ok(());
        }
    };

    // Try each candidate in turn: a dead swarm / stall / mislabelled grab drops to the next-best; a hard
    // error (qBittorrent down, etc.) fails the job; a cancellation stops cleanly.
    let acq = &state.config.acquisition;
    // In remote/seedbox mode qBittorrent saves on the seedbox (`remote_path`), so don't mkdir it
    // locally; otherwise it's a local staging dir we create per-grab.
    let (savepath, local_staging) = if acq.is_remote() {
        (acq.remote_path.clone().unwrap_or_default(), false)
    } else {
        (
            state
                .config
                .acquisition_staging_dir()
                .to_string_lossy()
                .into_owned(),
            true,
        )
    };
    let category = acq.category().to_string();
    // A per-job tag so we can pick out our torrent on a SHARED qBittorrent (the seedbox) without racing
    // other libraries' concurrent adds.
    let tag = job.job_id.to_string();
    let total = candidates.len();
    // Reasons candidates were rejected on CONTENT (not seeders), so a final failure can name the real
    // cause (e.g. "the release doesn't contain the requested track") instead of a phantom swarm problem.
    let mut content_reasons: Vec<String> = Vec::new();
    for (i, chosen) in candidates.into_iter().enumerate() {
        // Grab.
        let mut grab = status("grabbing");
        grab.chosen_guid = Some(chosen.guid.clone());
        grab.chosen_title = Some(chosen.title.clone());
        grab.quality_label = Some(quality::label_for(&chosen.title));
        grab.size_bytes = Some(chosen.size);
        grab.seeders = Some(chosen.seeders);
        hub.report_job_status(api_key, job.job_id, &grab).await?;

        // A retry (or a cancel that couldn't be cleaned up while the library was down), or the previous
        // candidate in this loop, can leave an old torrent for this job. Remove it + its files before
        // grabbing fresh, so attempts don't pile up orphaned downloads.
        if let Ok(Some(old_hash)) = super::prior_hash(&state.db, job.job_id).await {
            let _ = client.remove_on_teardown(&old_hash).await;
        }

        if local_staging {
            std::fs::create_dir_all(&savepath)?;
        }
        let hash = client.grab(&chosen, &savepath, &category, &tag).await?;
        // Persist the grab so a library restart can re-attach the monitor (see `resume_job`).
        let _ = super::record_job(&state.db, job.job_id, &hash, job.library_id).await;
        let mut dl = status("downloading");
        dl.qbit_hash = Some(hash.clone());
        dl.progress = Some(0.0);
        // Best-effort: if the user cancelled during the grab this 404s, but the monitor loop below
        // detects the cancellation and tears the torrent down, so don't fail (and orphan it) on that report.
        let _ = hub.report_job_status(api_key, job.job_id, &dl).await;

        match monitor_to_completion(
            state,
            hub,
            api_key,
            &client,
            job.job_id,
            &local_id,
            &music_root,
            &hash,
            &job.expected_titles,
            &job.wanted_titles,
        )
        .await?
        {
            MonitorOutcome::Imported | MonitorOutcome::Cancelled => return Ok(()),
            MonitorOutcome::Abandoned(reason) => {
                // The monitor already removed the torrent + cleared bookkeeping; fall through to the
                // next-best candidate (if any). Keep any CONTENT reason to explain a final failure.
                if let Some(r) = reason {
                    content_reasons.push(r);
                }
                tracing::info!(
                    job = %job.job_id,
                    "candidate {}/{} abandoned, trying next",
                    i + 1,
                    total
                );
            }
        }
    }

    // Every candidate was rejected. If the rejections were about CONTENT (e.g. a single-track job whose
    // releases don't actually contain the requested track), say so plainly instead of blaming seeders —
    // otherwise the user chases a phantom swarm problem for a track that simply isn't in any rip found.
    let msg = match content_reasons.last() {
        Some(reason) if !job.wanted_titles.is_empty() => format!(
            "none of the {total} release(s) found contain the requested track — {reason}. It may not be in any rip your indexers can reach."
        ),
        Some(reason) => format!("no usable source: {reason}"),
        None => "no usable source: every candidate stalled or had no live seeders".to_string(),
    };
    hub.report_job_status(api_key, job.job_id, &failed(&msg))
        .await?;
    Ok(())
}

/// Poll qBittorrent to completion (reporting progress), then import into the library and report
/// `completed`. Shared by the initial run and the restart-resume path. Clears resume bookkeeping
/// once the files are imported.
#[allow(clippy::too_many_arguments)]
async fn monitor_to_completion(
    state: &AppState,
    hub: &HubClient,
    api_key: &str,
    client: &AcquisitionClient,
    job_id: Uuid,
    local_id: &str,
    music_root: &Path,
    hash: &str,
    expected_titles: &[String],
    // For a single-track download: the exact title(s) to import; the release must contain them and only
    // the matching file(s) are imported. Empty = import the whole download (album/discography).
    wanted_titles: &[String],
) -> anyhow::Result<MonitorOutcome> {
    // Two independent decisions: (1) `keep_source` — copy vs move at import. Remote/shared-seedbox mode
    // ALWAYS copies (the files live behind a mount and must never be moved out from under the swarm).
    // (2) `keep_seeding`: whether to drop the torrent after import; driven by the user's flag ALONE, so
    // `keep_seeding=false` actually stops seeding even in remote mode.
    let keep_source = state.config.acquisition.keep_seeding || state.config.acquisition.is_remote();
    let keep_seeding = state.config.acquisition.keep_seeding;
    let mut last_reported = 0.0f32;
    // Stall watchdogs. `last_advance` resets on download progress; `last_alive` also resets whenever a
    // seed is connected, so a swarm that never connects a seed AND never moves off 0% is abandoned fast
    // (NO_PEERS_TIMEOUT), while one that progressed then flatlined gets the full STALL_TIMEOUT.
    let mut best_progress = -1.0f32;
    let mut last_advance = Instant::now();
    let mut last_alive = Instant::now();
    // Set once the torrent's metadata (file list) has been checked against the album's tracklist.
    let mut manifest_checked = false;
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        // Early wrong-release check: the moment the torrent's metadata resolves, its FILE LIST gets
        // the same content check the post-download import does. A mislabelled torrent (right-looking
        // name, wrong audio inside) is abandoned within seconds of the grab — before it spends the
        // whole download — and the run loop falls through to the next-best candidate.
        if !manifest_checked {
            if let Ok(names) = client.torrent_files(hash).await {
                if !names.is_empty() {
                    manifest_checked = true;
                    if let Some(reason) = verify_manifest(&names, expected_titles, wanted_titles) {
                        let _ = client.remove_on_teardown(hash).await;
                        let _ = super::clear_job(&state.db, job_id).await;
                        tracing::warn!(
                            job = %job_id,
                            "{reason}; abandoning at metadata check, will try next candidate"
                        );
                        return Ok(MonitorOutcome::Abandoned(Some(reason)));
                    }
                }
            }
        }
        // Stop + clean up if the user cancelled (or the Hub otherwise terminated the job): remove the
        // torrent AND its on-disk data. A transient Hub error keeps us monitoring (treats the job as
        // still active) rather than killing a healthy download. Only drop the resume bookkeeping once
        // removal succeeds; otherwise keep the row so a later resume re-attempts the teardown.
        if !hub.job_active(api_key, job_id).await.unwrap_or(true) {
            if client.remove_on_teardown(hash).await.is_ok() {
                let _ = super::clear_job(&state.db, job_id).await;
                tracing::info!(job = %job_id, "download cancelled: removed torrent + files");
            } else {
                tracing::warn!(job = %job_id, "cancelled but torrent removal failed; will retry on resume");
            }
            return Ok(MonitorOutcome::Cancelled);
        }
        let info = match client.info(hash).await {
            Ok(Some(info)) => info,
            // Torrent genuinely gone from qBittorrent (removed out from under us) — give up.
            Ok(None) => anyhow::bail!("torrent vanished from qBittorrent"),
            // A transient qBittorrent error (e.g. a restart) must NOT kill an otherwise-healthy
            // download, since the failure path would delete its files. Retry next tick, but still honour the
            // stall deadline so a persistent outage can't spin forever.
            Err(e) => {
                if last_advance.elapsed() > STALL_TIMEOUT {
                    anyhow::bail!("torrent info unavailable past the stall deadline: {e}");
                }
                tracing::debug!(job = %job_id, error = %e, "torrent info poll failed; retrying");
                continue;
            }
        };
        if info.is_complete() {
            // Best-effort: a status-report blip must NOT send an already-downloaded torrent through the
            // failure path (which deletes its files).
            let _ = hub
                .report_job_status(api_key, job_id, &status("importing"))
                .await;
            // Content check: a mislabelled torrent (right-looking title, wrong audio) must not be
            // imported. If the Hub gave us the album's tracklist and the downloaded files clearly don't
            // match it, fail the job (remove the torrent + files) instead of polluting the library.
            // Where THIS library reads the finished files: identity for a local qBittorrent, or the
            // remote→local mount remap for a shared seedbox.
            let content = state
                .config
                .acquisition
                .local_content_path(&info.content_path);
            // A just-completed torrent is renamed into place on the seedbox, but a network mount
            // (sshfs/rclone) has no push notification and caches directory listings, so the files can
            // be briefly invisible here. Wait for the import source to appear before reading it, so
            // mount-propagation lag doesn't fail (and tear down) a perfectly good download.
            for _ in 0..15u32 {
                if Path::new(&content).exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            if let Some(reason) = verify_content(&content, expected_titles, wanted_titles) {
                // Mislabelled album, OR (track job) the release lacks the wanted track: drop it and let
                // the caller try the next-best candidate. Only if none remain does the job fail — so
                // downloading one bonus track never dumps a whole, possibly-wrong album into the library.
                let _ = client.remove_on_teardown(hash).await;
                let _ = super::clear_job(&state.db, job_id).await;
                tracing::warn!(job = %job_id, "{reason}; abandoning, will try next candidate");
                return Ok(MonitorOutcome::Abandoned(Some(reason)));
            }
            import(
                &state.db,
                local_id,
                music_root,
                &state.config.acquisition.import_subdir,
                &content,
                keep_source,
                wanted_titles,
            )
            .await?;
            // Push the new tracks to the Hub BEFORE reporting completed, so the catalog reflects them
            // the moment the UI shows the job done (otherwise the periodic sync lags and "completed"
            // appears before the files are browsable).
            if let Err(e) = crate::catalog_sync::sync_all(state).await {
                tracing::warn!(error = %e, "post-import catalog sync failed");
            }
            // Stop seeding when the user opted out (keep_seeding=false). deleteFiles=false: in local mode
            // the files were MOVED in (nothing to delete); in remote mode they were COPIED off the mount,
            // so our copy is self-sufficient and the shared seedbox's data + any other libraries deduped to
            // the same infohash must be left intact.
            if !keep_seeding {
                let _ = client.remove_torrent(hash, false).await;
            }
            // Forget the job BEFORE the completed report: once imported, a failed completed-report must
            // not let the failure path find a hash and delete the (now imported / still-seeding) files.
            let _ = super::clear_job(&state.db, job_id).await;
            let _ = hub
                .report_job_status(api_key, job_id, &status("completed"))
                .await;
            return Ok(MonitorOutcome::Imported);
        }
        // Stall watchdogs: progress resets `last_advance`; any connected seed resets `last_alive`.
        if info.progress > best_progress + 0.0001 {
            best_progress = info.progress;
            last_advance = Instant::now();
        }
        if info.num_seeds > 0 {
            last_alive = Instant::now();
        }
        // Dead swarm: never connected a seed and never moved off 0%. Abandon fast and try the next
        // candidate. Otherwise, the slower flatline watchdog for a download that started then stalled.
        let dead_swarm = best_progress <= 0.0001 && last_alive.elapsed() > NO_PEERS_TIMEOUT;
        if dead_swarm || last_advance.elapsed() > STALL_TIMEOUT {
            let _ = client.remove_on_teardown(hash).await;
            let _ = super::clear_job(&state.db, job_id).await;
            tracing::info!(
                job = %job_id,
                seeds = info.num_seeds,
                progress = best_progress.max(0.0),
                "download {}; abandoning, will try next candidate",
                if dead_swarm { "found no live seeders" } else { "stalled" }
            );
            return Ok(MonitorOutcome::Abandoned(None));
        }
        if info.progress - last_reported >= 0.02 {
            last_reported = info.progress;
            let mut p = status("downloading");
            p.progress = Some(info.progress);
            let _ = hub.report_job_status(api_key, job_id, &p).await;
        }
    }
}

/// Build a Prowlarr query from artist + album, falling back to the display title when neither is set.
fn query_str(artist: Option<&str>, album: Option<&str>, display: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(a) = artist.filter(|s| !s.is_empty()) {
        parts.push(a);
    }
    if let Some(t) = album.filter(|s| !s.is_empty()) {
        parts.push(t);
    }
    if parts.is_empty() {
        if let Some(d) = display.filter(|s| !s.is_empty()) {
            parts.push(d);
        }
    }
    parts.join(" ")
}

/// Reduce an album title to its core for a backup search: drop a trailing parenthetical (`(Deluxe)`,
/// `[2009 Remaster]`) and any subtitle after a `": "`/`" - "` separator. Catalog/MusicBrainz titles are
/// often more verbose than trackers list them (e.g. "Clouds: Disney Channel Voices" vs just "Clouds"),
/// so the fuller query finds nothing while the core does. The colon must be followed by a space so
/// stylized names like "GO:OD AM" aren't truncated.
fn core_album(title: &str) -> String {
    let title = title.trim();
    let cut = [": ", " - ", " – ", " (", " ["]
        .iter()
        .filter_map(|sep| title.find(sep))
        .min();
    match cut {
        Some(idx) => title[..idx].trim().to_string(),
        None => title.to_string(),
    }
}

/// The core album title to use as a backup query, or `None` when stripping changes nothing (so a
/// second search wouldn't differ from the first).
fn core_fallback(album: Option<&str>) -> Option<String> {
    let album = album.filter(|s| !s.is_empty())?;
    let core = core_album(album);
    (!core.is_empty() && core != album).then_some(core)
}

/// The artist-name queries to try, in order: the era-correct name first (trackers indexed an old
/// release under the name in use THEN, e.g. "Machine Gun Kelly" for a 2019 album by the now-"mgk"
/// artist), then the current canonical name (trackers are inconsistent about rebrands). Deduped
/// case-insensitively; `[None]` when the job carries no artist hint (query from album/display alone).
fn artist_name_candidates(job: &ClaimedJob) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = Vec::new();
    for n in [job.era_artist_name.as_deref(), job.artist_name.as_deref()] {
        let Some(n) = n.filter(|s| !s.is_empty()) else {
            continue;
        };
        if !out
            .iter()
            .any(|e| e.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(n)))
        {
            out.push(Some(n.to_string()));
        }
    }
    if out.is_empty() {
        out.push(None);
    }
    out
}

/// Search Prowlarr for a job and return the hits that plausibly match it. For each artist-name
/// candidate (era name, then current name) it tries the full artist+album query, then retries once on
/// the core album title (subtitle/edition stripped), returning the first candidate that yields any
/// relevant hits.
async fn search_relevant(
    client: &AcquisitionClient,
    job: &ClaimedJob,
) -> anyhow::Result<Vec<Release>> {
    let album = job.album_title.as_deref();
    // When a specific edition was requested, bias the primary query toward it (e.g. append "Deluxe")
    // so Prowlarr surfaces that edition first. "Standard"/empty carries no signal, so skip it.
    let edition = job
        .edition_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("standard"));
    // A specific edition was requested → every candidate must actually BE that edition. This is what
    // stops a fallback query from grabbing the (higher-seeded) standard release instead of the Deluxe:
    // if no true edition match is found, we return nothing and the job reports no_results.
    let keep_edition = |rels: &mut Vec<Release>| {
        if let Some(ed) = edition {
            rels.retain(|r| edition_match(&r.title, ed));
        }
    };
    for artist_opt in artist_name_candidates(job) {
        let artist = artist_opt.as_deref();
        let base = query_str(artist, album, job.display_title.as_deref());
        let query = match edition {
            Some(ed) => format!("{base} {ed}"),
            None => base.clone(),
        };
        let mut releases = client.search(&query).await?;
        releases.retain(|r| is_relevant(&r.title, artist, album));
        keep_edition(&mut releases);
        // Edition query found nothing → retry the plain artist+album query (the edition may be labelled
        // differently on the tracker), but still require the edition token so we never fall back to the
        // standard release.
        if releases.is_empty() && edition.is_some() {
            let mut more = client.search(&base).await?;
            more.retain(|r| is_relevant(&r.title, artist, album));
            keep_edition(&mut more);
            releases = more;
        }
        if releases.is_empty() {
            if let Some(core) = core_fallback(album) {
                let backup = query_str(artist, Some(&core), job.display_title.as_deref());
                if backup != base {
                    let mut more = client.search(&backup).await?;
                    more.retain(|r| is_relevant(&r.title, artist, Some(&core)));
                    keep_edition(&mut more);
                    releases = more;
                }
            }
        }
        if !releases.is_empty() {
            return Ok(releases);
        }
    }
    Ok(Vec::new())
}

/// Re-find a user-chosen candidate by guid, trying both the full and core-title queries (the candidate
/// list may have been built from either).
async fn find_chosen(
    client: &AcquisitionClient,
    job: &ClaimedJob,
    guid: &str,
) -> anyhow::Result<Release> {
    let album = job.album_title.as_deref();
    for artist_opt in artist_name_candidates(job) {
        let artist = artist_opt.as_deref();
        let query = query_str(artist, album, job.display_title.as_deref());
        if let Some(r) = client
            .search(&query)
            .await?
            .into_iter()
            .find(|r| r.guid == guid)
        {
            return Ok(r);
        }
        if let Some(core) = core_fallback(album) {
            let backup = query_str(artist, Some(&core), job.display_title.as_deref());
            if backup != query {
                if let Some(r) = client
                    .search(&backup)
                    .await?
                    .into_iter()
                    .find(|r| r.guid == guid)
                {
                    return Ok(r);
                }
            }
        }
    }
    anyhow::bail!("chosen release is no longer available")
}

fn to_candidate(r: &Release) -> CandidateInput {
    CandidateInput {
        guid: r.guid.clone(),
        title: r.title.clone(),
        indexer: r.indexer.clone(),
        quality_label: Some(quality::label_for(&r.title)),
        score: None,
        size_bytes: Some(r.size),
        seeders: Some(r.seeders),
        leechers: Some(r.leechers),
        // Persist the actual source with the candidate: a later user pick then grabs directly
        // instead of re-resolving the guid via a live Prowlarr search (whose results age out —
        // that's what made picks "expire" and re-ask).
        download_url: r.download_url.clone(),
        magnet_url: r.magnet_url.clone(),
        info_hash: r.info_hash.clone(),
    }
}

/// Place the finished torrent's audio files into the library's import folder, then index them so they
/// appear (and `catalog_sync` pushes them to the Hub). The `notify` watcher would also catch them.
/// `keep_source` hardlinks/copies (so the torrent keeps its files to seed); otherwise it moves them.
async fn import(
    db: &SqlitePool,
    local_lib_id: &str,
    music_root: &Path,
    import_subdir: &str,
    content_path: &str,
    keep_source: bool,
    // When non-empty (a track job), import ONLY the files matching a wanted title; otherwise import all.
    wanted_titles: &[String],
) -> anyhow::Result<()> {
    let dest = music_root.join(import_subdir);
    std::fs::create_dir_all(&dest)?;
    let mut placed = 0usize;
    place_audio(
        Path::new(content_path),
        &dest,
        keep_source,
        &mut placed,
        wanted_titles,
    )?;
    if placed == 0 {
        anyhow::bail!("no matching audio files found in the completed download");
    }
    scanner::initial_scan(db, local_lib_id, &dest, false).await;
    Ok(())
}

fn place_audio(
    src: &Path,
    dest_dir: &Path,
    keep_source: bool,
    placed: &mut usize,
    wanted_titles: &[String],
) -> anyhow::Result<()> {
    if src.is_file() {
        // For a track job, keep only the files whose name matches a wanted title.
        let keep =
            is_audio(src) && (wanted_titles.is_empty() || file_is_wanted(src, wanted_titles));
        if keep {
            place_one(src, dest_dir, keep_source)?;
            *placed += 1;
        }
    } else if src.is_dir() {
        for entry in std::fs::read_dir(src)? {
            place_audio(&entry?.path(), dest_dir, keep_source, placed, wanted_titles)?;
        }
    }
    Ok(())
}

/// Whether an audio file's name matches one of the wanted track titles.
fn file_is_wanted(src: &Path, wanted_titles: &[String]) -> bool {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    wanted_titles.iter().any(|w| title_matches(stem, w))
}

/// The file-name stems (extension stripped) of every audio file under `root`, for title matching.
fn audio_file_stems(root: &Path) -> Vec<String> {
    fn walk(p: &Path, out: &mut Vec<String>) {
        if p.is_file() {
            if is_audio(p) {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push(stem.to_string());
                }
            }
        } else if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                walk(&entry.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// Whether an audio file's name plausibly IS the given track title: most of the title's word tokens
/// appear in the file name (robust to a leading track number, a "feat." tail, and the extension).
fn title_matches(file_stem: &str, title: &str) -> bool {
    let hay: HashSet<String> = tokenize(file_stem).into_iter().collect();
    let toks = tokenize(title);
    if toks.is_empty() {
        return false;
    }
    let hit = toks.iter().filter(|t| hay.contains(*t)).count();
    hit as f32 / toks.len() as f32 >= 0.75
}

fn place_one(src: &Path, dest_dir: &Path, keep_source: bool) -> anyhow::Result<()> {
    let name = src
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid file name"))?;
    // The fs watcher indexes each file the moment it lands here and `organize` moves it into place,
    // then prunes the emptied import dir (organize.rs `prune_empty_dirs`) — so the destination can
    // vanish BETWEEN files. Re-assert it per file rather than once per import, or every track after
    // the first fails with a bare ENOENT and takes the whole album's job down with it.
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating import dir {}", dest_dir.display()))?;
    let dest = dest_dir.join(name);
    if keep_source {
        // Hardlink so the torrent keeps its copy (free, same volume) and can seed; fall back to a copy
        // across filesystems. Never move, or seeding breaks.
        if std::fs::hard_link(src, &dest).is_err() {
            copy_into_place(src, dest_dir, &dest)?;
        }
    } else {
        // Atomic rename when on the same volume; copy+remove across filesystems (e.g. a seedbox mount).
        if std::fs::rename(src, &dest).is_err() {
            copy_into_place(src, dest_dir, &dest)?;
            let _ = std::fs::remove_file(src);
        }
    }
    Ok(())
}

/// Copy `src` into the library via a `.part` sidecar, then rename it into place.
///
/// Copying STRAIGHT to `dest` is not atomic, and `dest` sits inside the music root the fs watcher
/// watches recursively. Off a remote mount (seedbox) a track takes minutes to pull, so the file grows
/// on disk the whole time: the watcher fires on it repeatedly, the scanner hashes a PARTIAL file, and
/// since tracks are keyed by `content_hash` every partial read minted a brand-new track row — one
/// album arrived as dozens of duplicates. (Local imports hardlink or rename, which are atomic, so this
/// only ever bit the remote path.)
///
/// `.part` is not in `AUDIO_EXTS`, so the scanner ignores the sidecar completely; the rename is atomic
/// within the same directory, so the watcher only ever observes the finished file.
fn copy_into_place(src: &Path, dest_dir: &Path, dest: &Path) -> anyhow::Result<()> {
    let mut part_name = dest
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid file name"))?
        .to_os_string();
    part_name.push(".part");
    let part = dest_dir.join(part_name);
    if let Err(e) = std::fs::copy(src, &part) {
        let _ = std::fs::remove_file(&part); // don't leave a half-copied sidecar behind
        return Err(anyhow::Error::new(e).context(format!(
            "copying {} -> {}",
            src.display(),
            part.display()
        )));
    }
    std::fs::rename(&part, dest)
        .with_context(|| format!("finalising {} -> {}", part.display(), dest.display()))?;
    Ok(())
}

/// Post-download content check: before importing, sanity-check the finished download IS the requested
/// album by TRACK COUNT — robust to tags/script/edition, unlike fuzzy title matching (which false-fails on
/// untagged, non-Latin, single-char-title, or deluxe rips). Fails only on a gross undershoot: far fewer
/// audio files than the album's track count, i.e. a single / EP / partial / mislabelled grab. Generous (a
/// third of expected) so a standard edition of a multi-edition release, or a deluxe, never trips it.
/// Returns `Some(reason)` to fail, else `None`; skips when the cached tracklist is too small to judge.
fn verify_content(
    content_path: &str,
    expected_titles: &[String],
    wanted_titles: &[String],
) -> Option<String> {
    // Track job: require every wanted title to be present as a file (reject a release lacking it, so a
    // single-track download can't import a whole wrong album). Import then keeps only those files.
    if !wanted_titles.is_empty() {
        let stems = audio_file_stems(Path::new(content_path));
        for want in wanted_titles {
            if !stems.iter().any(|s| title_matches(s, want)) {
                return Some(format!("the wanted track “{want}” isn't in this release"));
            }
        }
        return None;
    }
    let expected = expected_titles.len();
    if expected < 5 {
        return None; // single / EP / uncached: nothing reliable to check against
    }
    let got = count_audio_files(Path::new(content_path));
    // `expected` is the UNION of titles across every edition, so it can over-count a single edition.
    // Keep the floor low (capped) so a legit standard/EP edition never trips it; this still catches the
    // gross case (a single / a couple of tracks grabbed for a whole album).
    let floor = (expected / 4).clamp(2, 5);
    (got < floor).then(|| {
        format!(
            "downloaded only {got} tracks but the album has ~{expected}; likely the wrong release or an incomplete download"
        )
    })
}

/// The same wrong-release check as `verify_content`, but against a torrent's FILE LIST (the
/// relative paths from its metadata) — so a mislabelled grab is caught seconds after the grab,
/// before the download spends bandwidth. Same tolerances: count-based for albums (robust to
/// tags/script/edition), title-based for single-track jobs.
fn verify_manifest(
    names: &[String],
    expected_titles: &[String],
    wanted_titles: &[String],
) -> Option<String> {
    let audio: Vec<&Path> = names
        .iter()
        .map(Path::new)
        .filter(|p| is_audio(p))
        .collect();
    if !wanted_titles.is_empty() {
        let stems: Vec<String> = audio
            .iter()
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        for want in wanted_titles {
            if !stems.iter().any(|s| title_matches(s, want)) {
                return Some(format!("the wanted track “{want}” isn't in this release"));
            }
        }
        return None;
    }
    let expected = expected_titles.len();
    if expected < 5 {
        return None; // single / EP / uncached: nothing reliable to check against
    }
    let got = audio.len();
    let floor = (expected / 4).clamp(2, 5);
    (got < floor).then(|| {
        format!(
            "the release contains only {got} audio files but the album has ~{expected} tracks; likely the wrong release"
        )
    })
}

/// Count the audio files under `root` (recursively).
fn count_audio_files(root: &Path) -> usize {
    fn walk(p: &Path, n: &mut usize) {
        if p.is_file() {
            if is_audio(p) {
                *n += 1;
            }
        } else if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                walk(&entry.path(), n);
            }
        }
    }
    let mut n = 0;
    walk(root, &mut n);
    n
}

/// Whether a Prowlarr hit PLAUSIBLY matches the requested artist (and album) — the loose pool gate.
/// Token-based so format noise (`(2009) [FLAC]`) doesn't matter; lenient thresholds keep oddly-titled
/// but possibly-correct releases in the pool. Picking the *right* one (or asking the user) is the job of
/// `is_confident_match` / the run loop; this only weeds out the clearly-unrelated.
fn is_relevant(title: &str, artist: Option<&str>, album: Option<&str>) -> bool {
    let hay: HashSet<String> = tokenize(title).into_iter().collect();
    let coverage = |needle: &str| -> f32 {
        let toks = tokenize(needle);
        if toks.is_empty() {
            return 1.0;
        }
        let hit = toks.iter().filter(|t| hay.contains(*t)).count();
        hit as f32 / toks.len() as f32
    };
    if let Some(artist) = artist.filter(|s| !s.is_empty()) {
        if coverage(artist) < 0.6 {
            return false;
        }
    }
    if let Some(album) = album.filter(|s| !s.is_empty()) {
        return coverage(album) >= 0.5;
    }
    true
}

/// Tokens that carry a release's *content variant* rather than the original album: an instrumental,
/// karaoke, tribute, etc. A confident auto-grab never lands on one of these (unless the request itself is
/// that variant).
const VARIANT_MARKERS: [&str; 8] = [
    "instrumental",
    "karaoke",
    "tribute",
    "made famous",
    "in the style of",
    "originally performed",
    "lullaby",
    "8-bit",
];
/// Words that introduce a volume number ("Vol. 3"); dropped from the distinctive-word check since the
/// numeral itself is matched separately. Kept to the unambiguous abbreviations, NOT "part"/"no", which
/// are commonly genuine title words ("Part of Me", "No Ceilings").
const VOLUME_KEYWORDS: [&str; 2] = ["vol", "volume"];

fn drop_leading_articles(mut toks: Vec<String>) -> Vec<String> {
    while matches!(
        toks.first().map(String::as_str),
        Some("the" | "tha" | "a" | "an")
    ) {
        toks.remove(0);
    }
    toks
}

/// Format / source / edition noise words that AREN'T part of an album name — allowed as "extra" tokens
/// without breaking a confident match. NOT exhaustive on purpose: a missing word just sends a release to
/// ASK (fail-safe), never a wrong grab.
#[rustfmt::skip]
const NOISE: &[&str] = &[
    // codecs / formats / quality (bare bitrate/sample-rate NUMBERS are handled by the numeric rule)
    "flac", "mp3", "m4a", "m4b", "alac", "aac", "wav", "aiff", "aif", "ogg", "opus", "ape", "wv",
    "wavpack", "mqa", "dsd", "dts", "atmos", "wma", "dxd", "kbps", "cbr", "vbr", "v0", "v2", "web",
    "webrip", "webflac", "rip", "scene", "remux", "log", "cue", "eac", "lossless", "hires", "res", "hd",
    "16bit", "24bit", "khz", "hz", "bit",
    // media / source / store. NOTE: standalone "cd"/"disc" are deliberately EXCLUDED — a bare number
    // after them ("Culture Disc 2") would masquerade as the volume; only the combined single-token forms
    // (2cd / cd1 / disc1), which can't be confused with a volume, are treated as noise.
    "vinyl", "vinylrip", "cassette", "cd1", "cd2", "cd3", "cd4", "2cd", "3cd", "4cd", "disc1", "disc2",
    "disc3", "dvd", "dvda", "sacd", "qobuz", "tidal", "deezer", "itunes", "bandcamp", "spotify",
    "amazon", "applemusic", "hdtracks", "beatport", "7digital",
    // release type (descriptors, not identity)
    "ost", "soundtrack", "score", "lp", "ep", "single", "comp", "compilation", "album",
    // edition qualifiers (same album identity)
    "promo", "advance", "deluxe", "expanded", "extended", "remaster", "remastered", "anniversary",
    "edition", "version", "bonus", "special", "limited", "reissue", "explicit", "clean", "mono",
    "stereo", "digipak", "collectors", "collector", "japanese", "japan", "international", "intl",
    // verbose release-tag filler ("FLAC Quality Album with Lyrics", "incl. Scans"): advertising, not
    // identity. All protected by the `req.contains` short-circuit, so a real album word ("Full Moon
    // Fever", "Quality Control") is never treated as noise.
    "quality", "lyrics", "tracklist", "scans", "booklet", "complete", "full", "incl", "including",
    "with", "feat", "ft", "featuring",
];

/// Lowercase alphanumeric tokens KEEPING single characters, so a volume "V" survives (unlike `tokenize`).
fn tokens_keep_singles(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Lowercase tokens that sit INSIDE `(…)`/`[…]`/`{…}`: release metadata (year, format, scene group,
/// store, credits). They're excused as noise in a confident match, and crucially a bracketed numeral is
/// NOT allowed to satisfy a volume (so "Culture (2 Chainz Remix)" isn't read as "Culture, Vol. 2").
fn bracketed_tokens(s: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    let mut depth: i32 = 0;
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
            continue;
        }
        // token boundary: a run inside brackets (depth > 0) is metadata
        if !cur.is_empty() {
            if depth > 0 {
                out.insert(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = (depth - 1).max(0),
            _ => {}
        }
    }
    if !cur.is_empty() && depth > 0 {
        out.insert(cur);
    }
    out
}

/// The album's distinctive tokens (single chars kept; leading articles + volume keywords dropped) — a
/// confident release must carry exactly these, no more, no less. "Tha Carter V" → [carter, v]; "Greatest
/// Hits Vol. 2" → [greatest, hits, 2]; "Culture II" → [culture, ii].
fn album_tokens(album: &str) -> Vec<String> {
    let toks = tokens_keep_singles(album)
        .into_iter()
        .filter(|t| !VOLUME_KEYWORDS.contains(&t.as_str()))
        .collect();
    drop_leading_articles(toks)
}

/// A numeral token mapped to a canonical key by VALUE ("ii"/"2"/"two" → "#2", "v" → "#5") so a volume's
/// rendering doesn't matter; non-numerals pass through unchanged.
fn numeral_key(tok: &str) -> String {
    match parse_numeral(tok) {
        Some(v) => format!("#{v}"),
        None => tok.to_string(),
    }
}

/// A `<digits><unit>` audio-spec token: sample rate / bit depth / bitrate (96khz, 24bit, 320kbps), so
/// hi-res tags don't have to be enumerated. The numeric prefix avoids matching real words ("rabbit").
fn is_audio_spec(tok: &str) -> bool {
    ["khz", "kbps", "bit", "hz"].iter().any(|u| {
        tok.strip_suffix(u)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Whether `tok` is non-distinctive noise in a release title: a leading-article word, a year/rate/bitrate
/// number, an audio spec, a volume keyword, or a known format/edition word, and NOT itself part of the
/// requested album (`req`).
fn is_noise(tok: &str, req: &HashSet<String>) -> bool {
    if req.contains(tok) {
        return false;
    }
    matches!(tok, "the" | "tha" | "a" | "an")
        // a pure multi-digit number that isn't a request token: a year, sample rate, or bitrate.
        || (tok.len() >= 2 && tok.bytes().all(|b| b.is_ascii_digit()))
        || is_audio_spec(tok)
        || VOLUME_KEYWORDS.contains(&tok)
        || NOISE.contains(&tok)
}

/// Whether we're CONFIDENT a release IS exactly the requested album: strict enough to auto-grab without
/// asking. Requires: strong artist presence (≥0.8); not a content variant; EVERY distinctive album token
/// present (numeral form ignored, "II" == "2"); and NO EXTRA distinctive token in the title. Any word
/// that isn't the artist, a request token, or known noise signals a DIFFERENT album / volume / disc /
/// variant, so it's NOT confident. Anything short of this falls back to asking, never a wrong grab.
fn is_confident_match(title: &str, artist: Option<&str>, album: Option<&str>) -> bool {
    let hay = tokens_keep_singles(title);
    let hay_norm: HashSet<String> = hay.iter().map(|t| numeral_key(t)).collect();
    // Tokens inside (…)/[…] are release metadata (year, format, scene group, store), excused as noise,
    // and a bracketed numeral can't stand in for the volume.
    let bracketed = bracketed_tokens(title);

    let mut artist_set: HashSet<String> = HashSet::new();
    if let Some(artist) = artist.filter(|s| !s.is_empty()) {
        let at = drop_leading_articles(tokens_keep_singles(artist));
        if !at.is_empty() {
            let present = at
                .iter()
                .filter(|t| hay_norm.contains(&numeral_key(t)))
                .count();
            if (present as f32 / at.len() as f32) < 0.8 {
                return false;
            }
        }
        artist_set = at.into_iter().collect();
    }

    if let Some(album) = album.filter(|s| !s.is_empty()) {
        if has_variant_marker(title, album) {
            return false;
        }
        let req = album_tokens(album);
        let req_raw: HashSet<String> = req.iter().cloned().collect();
        let req_norm: HashSet<String> = req.iter().map(|t| numeral_key(t)).collect();
        // Every distinctive request token must be present OUTSIDE the artist name — so the "5" in artist
        // "Maroon 5" can't stand in for album "V", etc.
        let album_hay_norm: HashSet<String> = hay
            .iter()
            .filter(|t| !artist_set.contains(*t) && !bracketed.contains(*t))
            .map(|t| numeral_key(t))
            .collect();
        if !req_norm.iter().all(|t| album_hay_norm.contains(t)) {
            return false;
        }
        // ...and no EXTRA distinctive token (a different album / volume / disc). Artist words, bracketed
        // metadata, and known noise are all fine.
        for t in &hay {
            if req_norm.contains(&numeral_key(t))
                || artist_set.contains(t)
                || bracketed.contains(t)
                || is_noise(t, &req_raw)
            {
                continue;
            }
            return false;
        }
    }
    true
}

/// A graded closeness of a release to the request, for ordering the human pick-list (closest first) when
/// we fall back to asking. A confident match scores highest; otherwise the fraction of distinctive request
/// tokens present plus artist coverage, minus a content-variant penalty.
fn match_closeness(title: &str, artist: Option<&str>, album: Option<&str>) -> f32 {
    let hay = tokens_keep_singles(title);
    let hay_norm: HashSet<String> = hay.iter().map(|t| numeral_key(t)).collect();
    let cover = |needle: &[String]| -> f32 {
        if needle.is_empty() {
            return 1.0;
        }
        needle
            .iter()
            .filter(|t| hay_norm.contains(&numeral_key(t)))
            .count() as f32
            / needle.len() as f32
    };
    let mut score = 0.0;
    if let Some(artist) = artist.filter(|s| !s.is_empty()) {
        score += cover(&drop_leading_articles(tokens_keep_singles(artist)));
    }
    if let Some(album) = album.filter(|s| !s.is_empty()) {
        score += cover(&album_tokens(album));
        if is_confident_match(title, artist, Some(album)) {
            score += 1.0;
        }
        if has_variant_marker(title, album) {
            score -= 1.0;
        }
    }
    score
}

/// Whether `title` is a content variant (instrumental/karaoke/…) the request didn't ask for.
fn has_variant_marker(title: &str, album: &str) -> bool {
    let (t, a) = (title.to_lowercase(), album.to_lowercase());
    VARIANT_MARKERS
        .iter()
        .any(|m| t.contains(m) && !a.contains(m))
}

/// Lowercase alphanumeric word tokens (length > 1), for loose title matching.
fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(String::from)
        .collect()
}

/// Whether a candidate title plausibly IS the requested edition. Matches on the edition's distinctive
/// word (the longest alphabetic token, ignoring generic "edition"/"version"), as a substring so
/// "Remaster" matches "Remastered". Used to reject the standard release when a specific edition (e.g.
/// "Deluxe") was requested.
fn edition_match(title: &str, edition: &str) -> bool {
    let key = tokenize(edition)
        .into_iter()
        .filter(|t| !matches!(t.as_str(), "edition" | "version" | "the"))
        // Prefer a real word (has letters) over a bare year number, then the longest such token.
        .max_by_key(|t| (t.chars().any(|c| c.is_alphabetic()), t.len()));
    match key {
        Some(k) => title.to_lowercase().contains(&k),
        None => true,
    }
}

/// Volume numerals recognised for sequel disambiguation: Roman I–XX, the spelled-out cardinals
/// one–twenty, and a 1–2 digit number. Longer numbers, like years (2018) or name-numbers (182, 1989),
/// are NOT volumes; coverage handles those.
const ROMAN: [&str; 20] = [
    "i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x", "xi", "xii", "xiii", "xiv", "xv",
    "xvi", "xvii", "xviii", "xix", "xx",
];
const WORDS: [&str; 20] = [
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
];

/// Parse a lowercase token as a small volume numeral: Arabic (1–2 digit), Roman I–XX, or spelled-out,
/// so "v", "5" and "five" all read as 5.
fn parse_numeral(tok: &str) -> Option<u32> {
    if (1..=2).contains(&tok.len()) && tok.bytes().all(|b| b.is_ascii_digit()) {
        return tok.parse().ok().filter(|n| *n > 0);
    }
    if let Some(i) = ROMAN.iter().position(|r| *r == tok) {
        return Some(i as u32 + 1);
    }
    WORDS.iter().position(|w| *w == tok).map(|i| i as u32 + 1)
}

fn is_audio(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some(
            "flac"
                | "mp3"
                | "m4a"
                | "alac"
                | "wav"
                | "aiff"
                | "aif"
                | "ogg"
                | "opus"
                | "aac"
                | "wv"
                | "ape"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The POOL (is_relevant) is loose: keep anything plausibly about the artist+album ──────────
    #[test]
    fn pool_keeps_plausible_drops_unrelated() {
        let artist = Some("Lil Wayne");
        let album = Some("Tha Carter V");
        // Both the right volume AND a different one stay in the POOL; the confidence gate picks.
        assert!(is_relevant(
            "Lil Wayne - Tha Carter V (2018) [FLAC]",
            artist,
            album
        ));
        assert!(is_relevant(
            "Lil Wayne - Tha Carter VI (2025) [FLAC]",
            artist,
            album
        ));
        // A clearly-unrelated release is dropped.
        assert!(!is_relevant("Weird Al - Polka Party", artist, album));
    }

    // ── Auto-grab (is_confident_match) is strict: the exact album + exact volume only ────────────
    #[test]
    fn confident_only_on_the_exact_album_and_volume() {
        let artist = Some("Lil Wayne");
        let album = Some("Tha Carter V");
        assert!(is_confident_match(
            "Lil Wayne - Tha Carter V (2018) [FLAC]",
            artist,
            album
        ));
        // A different volume, the base album, and a content variant are NOT confident → ask the user.
        assert!(!is_confident_match(
            "Lil Wayne - Tha Carter VI (2025) [FLAC]",
            artist,
            album
        ));
        assert!(!is_confident_match(
            "Lil Wayne - Tha Carter (2004)",
            artist,
            album
        ));
        assert!(!is_confident_match(
            "Lil Wayne - Tha Carter V (Instrumentals) [2018]",
            artist,
            album
        ));
        // Arabic / dropped-article forms of the SAME volume are still confident.
        assert!(is_confident_match(
            "Lil Wayne - Tha Carter 5 (2018) WEB FLAC",
            artist,
            album
        ));
        assert!(is_confident_match(
            "Lil Wayne - Carter V [24bit]",
            artist,
            album
        ));
    }

    #[test]
    fn confident_distinguishes_volumes() {
        let a = Some("Migos");
        assert!(is_confident_match(
            "Migos - Culture II (2018) FLAC",
            a,
            Some("Culture II")
        ));
        assert!(!is_confident_match(
            "Migos - Culture III (2021) FLAC",
            a,
            Some("Culture II")
        ));
        assert!(!is_confident_match(
            "Migos - Culture (2017) FLAC",
            a,
            Some("Culture II")
        ));
    }

    #[test]
    fn confident_through_spelled_and_abbreviated_volumes() {
        // Same album + same volume, just rendered differently; all confident auto-grabs.
        assert!(is_confident_match(
            "Migos - Culture Two (2018) FLAC",
            Some("Migos"),
            Some("Culture II")
        ));
        assert!(is_confident_match(
            "Lil Wayne - Tha Carter Five (2018) [FLAC]",
            Some("Lil Wayne"),
            Some("Tha Carter V")
        ));
        // "Vol." catalog title vs "Volume" release title.
        assert!(is_confident_match(
            "DJ - Greatest Hits Volume 2 (2010) FLAC",
            Some("DJ"),
            Some("Greatest Hits Vol. 2")
        ));
    }

    #[test]
    fn confident_for_non_sequel_albums() {
        assert!(is_confident_match(
            "Frank Ocean - Channel Orange (2012) [FLAC]",
            Some("Frank Ocean"),
            Some("Channel Orange")
        ));
        assert!(is_confident_match(
            "Taylor Swift - 1989 (2014) FLAC",
            Some("Taylor Swift"),
            Some("1989")
        ));
        // A leading number is part of the name — the canonical "4 Your Eyez Only" release is confident.
        assert!(is_confident_match(
            "J. Cole - 4 Your Eyez Only (2016) [FLAC]",
            Some("J. Cole"),
            Some("4 Your Eyez Only")
        ));
        // Wrong artist is never confident.
        assert!(!is_confident_match(
            "Someone Else - Channel Orange",
            Some("Frank Ocean"),
            Some("Channel Orange")
        ));
    }

    #[test]
    fn not_confident_on_wrong_album_incidental_number_or_extra_words() {
        // An incidental number, like "(2 Chainz Remix)", "Disc 2", or "2 CD", must NOT pass as Culture II.
        assert!(!is_confident_match(
            "Migos - Culture (2 Chainz Remix EP)",
            Some("Migos"),
            Some("Culture II")
        ));
        assert!(!is_confident_match(
            "Migos - Culture Disc 2 (2017) FLAC",
            Some("Migos"),
            Some("Culture II")
        ));
        assert!(!is_confident_match(
            "Migos - Culture 2 CD (2017)",
            Some("Migos"),
            Some("Culture II")
        ));
        // Requesting the BASE album must NOT auto-grab a sequel...
        assert!(!is_confident_match(
            "Migos - Culture II (2018) FLAC",
            Some("Migos"),
            Some("Culture")
        ));
        assert!(!is_confident_match(
            "Travis Scott - Rodeo II (Deluxe) 2020",
            Some("Travis Scott"),
            Some("Rodeo")
        ));
        // ...and a longer album that merely CONTAINS the requested name is a different record.
        assert!(!is_confident_match(
            "Some Artist - Blonde Ambition (2019) FLAC",
            Some("Some Artist"),
            Some("Blonde")
        ));
        // A subtitle between the name and numeral, or an unknown numeral form ("3rd"), → ask (not grab).
        assert!(!is_confident_match(
            "Brockhampton - Saturation: The Final III (2017)",
            Some("Brockhampton"),
            Some("Saturation III")
        ));
        assert!(!is_confident_match(
            "Brockhampton - Saturation 3rd (2017) FLAC",
            Some("Brockhampton"),
            Some("Saturation III")
        ));
        // ...but the real base albums DO auto-grab, and the unsure ones stay in the pool to pick from.
        assert!(is_confident_match(
            "Migos - Culture (2017) FLAC",
            Some("Migos"),
            Some("Culture")
        ));
        assert!(is_relevant(
            "Brockhampton - Saturation 3rd (2017) FLAC",
            Some("Brockhampton"),
            Some("Saturation III")
        ));
    }

    #[test]
    fn closeness_ranks_the_requested_volume_first() {
        let a = Some("Lil Wayne");
        let al = Some("Tha Carter V");
        let v = match_closeness("Lil Wayne - Tha Carter V (2018) FLAC", a, al);
        let vi = match_closeness("Lil Wayne - Tha Carter VI (2025) FLAC", a, al);
        assert!(
            v > vi,
            "requested volume must rank above a different one ({v} vs {vi})"
        );
    }

    #[test]
    fn confident_through_common_source_and_disc_tags() {
        // Common tracker tags (multi-disc, streaming store, OST, hi-res spec) must NOT force a pick.
        assert!(is_confident_match(
            "Pink Floyd - The Wall (1979) [2CD] [FLAC]",
            Some("Pink Floyd"),
            Some("The Wall")
        ));
        assert!(is_confident_match(
            "Beyonce - Lemonade (2016) Qobuz FLAC",
            Some("Beyonce"),
            Some("Lemonade")
        ));
        assert!(is_confident_match(
            "Hans Zimmer - Interstellar (2014) OST FLAC",
            Some("Hans Zimmer"),
            Some("Interstellar")
        ));
        assert!(is_confident_match(
            "Adele - 25 (2015) [FLAC] [24bit-96khz]",
            Some("Adele"),
            Some("25")
        ));
        // ...but a standalone "Disc 2" still keeps the BASE album out of a "Culture II" auto-grab.
        assert!(!is_confident_match(
            "Migos - Culture Disc 2 (2017) FLAC",
            Some("Migos"),
            Some("Culture II")
        ));
    }

    #[test]
    fn confident_through_scene_tags_and_verbose_filler() {
        // Verbose uploader titles ("FLAC Quality Album with Lyrics", "incl. Scans") are the SAME album.
        // The filler words are advertising, not identity, so these must auto-grab.
        assert!(is_confident_match(
            "Lil Wayne - Tha Carter V (2018) FLAC Quality Album with Lyrics",
            Some("Lil Wayne"),
            Some("Tha Carter V")
        ));
        assert!(is_confident_match(
            "Adele - 25 (2015) FLAC Complete incl. Scans + Booklet",
            Some("Adele"),
            Some("25")
        ));
        // Bracketed scene-group / store / quality tags are metadata, excused no matter the group name.
        assert!(is_confident_match(
            "Lil Wayne - Tha Carter V (2018) [FLAC] [DeepGuy]",
            Some("Lil Wayne"),
            Some("Tha Carter V")
        ));
        assert!(is_confident_match(
            "Kanye West - Graduation (2007) [pradyut] [Mp3-V0]",
            Some("Kanye West"),
            Some("Graduation")
        ));
        // ...but a BRACKETED numeral must NOT stand in for the requested volume: "(2 Chainz Remix)" is
        // not "Culture, Vol. 2", so the base album stays out of a "Culture II" auto-grab.
        assert!(!is_confident_match(
            "Migos - Culture [2 Chainz Remix] (2017) FLAC",
            Some("Migos"),
            Some("Culture II")
        ));
        // ...and a real album word that happens to be in the filler list is protected by `req.contains`:
        // "Full Moon Fever" is still its own album, not "Moon Fever" + noise.
        assert!(is_confident_match(
            "Tom Petty - Full Moon Fever (1989) FLAC",
            Some("Tom Petty"),
            Some("Full Moon Fever")
        ));
    }

    #[test]
    fn artist_digits_and_volume_keyword_words() {
        // The "5" in artist "Maroon 5" must NOT satisfy album "V"; a different Maroon 5 album asks.
        assert!(!is_confident_match(
            "Maroon 5 - Songs About Jane (2002) FLAC",
            Some("Maroon 5"),
            Some("V")
        ));
        assert!(is_confident_match(
            "Maroon 5 - V (2014) FLAC",
            Some("Maroon 5"),
            Some("V")
        ));
        // "No Ceilings" keeps its "No" (not treated as a volume keyword), so the sequel isn't confident.
        assert!(is_confident_match(
            "Lil Wayne - No Ceilings (2009) [FLAC]",
            Some("Lil Wayne"),
            Some("No Ceilings")
        ));
        assert!(!is_confident_match(
            "Lil Wayne - No Ceilings 3 (2020) FLAC",
            Some("Lil Wayne"),
            Some("No Ceilings")
        ));
    }

    #[test]
    fn numeral_parsing() {
        assert_eq!(parse_numeral("v"), Some(5));
        assert_eq!(parse_numeral("vi"), Some(6));
        assert_eq!(parse_numeral("5"), Some(5));
        assert_eq!(parse_numeral("five"), Some(5)); // spelled-out
        assert_eq!(parse_numeral("two"), Some(2));
        assert_eq!(parse_numeral("2018"), None); // year, not a volume
        assert_eq!(parse_numeral("182"), None); // name-number, not a volume
        assert_eq!(parse_numeral("orange"), None);
    }
}
