//! Storing the credits a `Tracklist.txt` describes.
//!
//! The parsing lives in [`crate::tracklist`] and knows nothing about files or SQL; this is the half
//! that finds the sidecar beside an album, checks it actually describes that album, and writes the
//! result. Split that way because the format is the risky part and it is far easier to test as a
//! pure function than through a scan.
//!
//! Worker-owned data, with the rules that implies here: a re-index must COALESCE-preserve
//! `credits_source_hash` / `credits_at` rather than clearing them, and an edited sidecar must clear
//! its own stamp so the next pass rewrites rather than trusting stale rows.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::tracklist::{self, Tracklist};

/// How far a printed duration may sit from the probed one and still be the same track.
///
/// The file prints whole seconds and encoders disagree about the final frame, so exact equality
/// rejects correct files. Two seconds is comfortably inside the gap between any two real tracks.
const TOLERANCE: Duration = Duration::from_secs(2);

/// Find the tracklist sidecar in `dir`, if there is one.
///
/// Matched on the suffix rather than the whole name because the album title leads it
/// ("Triple X Years In The Game - Tracklist.txt") and that title is the one part we cannot predict.
pub fn find_sidecar(dir: &Path) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.to_ascii_lowercase().ends_with("tracklist.txt") {
            // More than one is ambiguous, and guessing between them is how the wrong credits get
            // written. Take none rather than a coin flip.
            if found.is_some() {
                warn!(dir = ?dir, "several tracklist files; skipping credits for this folder");
                return None;
            }
            found = Some(path);
        }
    }
    found
}

/// Read and parse the sidecar for the album in `dir`.
pub fn read_sidecar(dir: &Path) -> Option<(Tracklist, String, String)> {
    let path = find_sidecar(dir)?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(raw.as_bytes()));
    let parsed = tracklist::parse(&raw)?;
    Some((parsed, hash, path.to_string_lossy().into_owned()))
}

/// One indexed track, as the caller must describe it for matching.
pub struct IndexedTrack {
    pub id: String,
    pub track_no: Option<i64>,
    pub duration_ms: i64,
}

/// Apply a folder's sidecar to the tracks indexed from it.
///
/// Returns how many tracks gained credits. Does nothing and returns 0 when the file does not
/// describe this album — see [`Tracklist::matches`]; a sidecar left in the wrong folder parses
/// perfectly and would otherwise write confident, wrong credits that nobody would ever catch.
pub async fn apply(
    db: &SqlitePool,
    dir: &Path,
    tracks: &[IndexedTrack],
    force: bool,
) -> anyhow::Result<usize> {
    let Some((list, hash, source)) = read_sidecar(dir) else {
        return Ok(0);
    };

    let probed: Vec<(u32, Duration)> = tracks
        .iter()
        .filter_map(|t| {
            let no = u32::try_from(t.track_no?).ok()?;
            Some((no, Duration::from_millis(t.duration_ms.max(0) as u64)))
        })
        .collect();

    if !list.matches(&probed, TOLERANCE) {
        warn!(
            source = %source,
            "tracklist durations do not match this album; ignoring it"
        );
        return Ok(0);
    }

    let mut applied = 0usize;
    for track in tracks {
        let Some(no) = track.track_no.and_then(|n| u32::try_from(n).ok()) else {
            continue;
        };
        let Some(entry) = list.tracks.iter().find(|t| t.number == no) else {
            continue;
        };

        // Already current: same file, same track. Skipped rather than rewritten so a rescan of a
        // large library does not churn every credit row it already has.
        if !force {
            let seen: Option<String> =
                sqlx::query_scalar("SELECT credits_source_hash FROM tracks WHERE id = ?1")
                    .bind(&track.id)
                    .fetch_optional(db)
                    .await?
                    .flatten();
            if seen.as_deref() == Some(hash.as_str()) {
                continue;
            }
        }

        let mut tx = db.begin().await?;
        // Replaced wholesale rather than merged: the file is the authority for this track, and a
        // credit removed from it upstream must disappear here too.
        sqlx::query("DELETE FROM track_credits WHERE track_id = ?1")
            .bind(&track.id)
            .execute(&mut *tx)
            .await?;

        let mut ord = 0i64;
        for credit in &entry.credits {
            let is_org = i64::from(credit.is_organisation());
            let name_norm = normalise(&credit.name);
            for role in &credit.roles {
                // The primary key is (track, name_norm, role), so a name credited twice under the
                // same role in one file collapses instead of failing the insert.
                sqlx::query(
                    "INSERT OR IGNORE INTO track_credits \
                       (track_id, name, name_norm, role, is_org, ord) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .bind(&track.id)
                .bind(&credit.name)
                .bind(&name_norm)
                .bind(role)
                .bind(is_org)
                .bind(ord)
                .execute(&mut *tx)
                .await?;
                ord += 1;
            }
        }

        // The explicit marker too. The file states it per track, which is better than the advisory
        // tag: plenty of files carry no rating at all, and the badge is tri-state precisely so
        // "unknown" and "clean" stay distinguishable.
        let advisory = if entry.explicit { "explicit" } else { "clean" };
        sqlx::query(
            "UPDATE tracks \
                SET credits_source_hash = ?2, \
                    credits_at = strftime('%s','now'), \
                    advisory = COALESCE(advisory, ?3) \
              WHERE id = ?1",
        )
        .bind(&track.id)
        .bind(&hash)
        .bind(advisory)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        applied += 1;
    }

    if applied > 0 {
        info!(source = %source, tracks = applied, "applied tracklist credits");
    } else {
        debug!(source = %source, "tracklist already current");
    }
    Ok(applied)
}

/// Apply the sidecar for the folder `path` sits in, to every track indexed from that folder.
///
/// Folder-level rather than per-file because the duration guard needs the whole tracklist to be
/// meaningful — one track agreeing proves very little, where a whole album agreeing is conclusive.
/// Called after each file is indexed anyway: `apply` short-circuits on an unchanged source hash, so
/// the repeats cost one query per file rather than a rewrite.
pub async fn apply_for_dir(
    db: &SqlitePool,
    library_id: &str,
    path: &Path,
) -> anyhow::Result<usize> {
    let Some(dir) = path.parent() else {
        return Ok(0);
    };
    if find_sidecar(dir).is_none() {
        return Ok(0);
    }

    // Every indexed track whose file sits directly in this folder. `LIKE` with the separator
    // appended keeps a sibling folder with the same prefix ("Album" vs "Album Deluxe") out.
    let prefix = format!("{}{}", dir.to_string_lossy(), std::path::MAIN_SEPARATOR);
    let rows: Vec<(String, Option<i64>, i64)> = sqlx::query_as(
        "SELECT t.id, t.track_no, t.duration_ms            FROM tracks t            JOIN files f ON f.track_id = t.id           WHERE t.library_id = ?1 AND f.path LIKE ?2 || '%'",
    )
    .bind(library_id)
    .bind(&prefix)
    .fetch_all(db)
    .await?;

    let tracks: Vec<IndexedTrack> = rows
        .into_iter()
        .map(|(id, track_no, duration_ms)| IndexedTrack {
            id,
            track_no,
            duration_ms,
        })
        .collect();
    apply(db, dir, &tracks, false).await
}

/// Lowercased with runs of whitespace collapsed, so one person is one row across an album.
fn normalise(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::normalise;

    #[test]
    fn one_person_is_one_row_however_the_file_spaced_them() {
        assert_eq!(normalise("Joe  LaPorta"), "joe laporta");
        assert_eq!(normalise("  joe laporta "), "joe laporta");
        assert_eq!(normalise("Joe LaPorta"), normalise("JOE LAPORTA"));
    }

    #[test]
    fn different_people_stay_different() {
        assert_ne!(normalise("Sean Daley"), normalise("Sean Daly"));
    }
}
