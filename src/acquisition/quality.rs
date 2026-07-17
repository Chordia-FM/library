//! Quality scoring: rank Prowlarr releases against a download job's profile, best first.
//!
//! The format tier dominates so the best available format always wins (e.g. lossless over a
//! high-bitrate lossy with more seeders); a bounded seeder bonus only breaks ties within a tier.

use chordia_contracts::acquisition::DownloadQualityProfile;

use super::Release;

/// Default ranking when a job carries no profile (best → worst).
const DEFAULT: &[&str] = &[
    "flac_24", "flac", "alac", "wav", "mp3_320", "mp3_v0", "aac_256", "mp3",
];

/// Whether a release title reads as a LOSSLESS encoding — the strictly-better gate for quality
/// upgrades (v1 upgrades are lossy → lossless; sweeps only propose all-lossy albums).
pub fn is_lossless(title: &str) -> bool {
    matches!(detect_format(title), "flac_24" | "flac" | "alac" | "wav")
}

/// Detect a coarse format key from a release title.
///
/// Hi-res specs (24-bit, ≥96 kHz) imply a LOSSLESS source even when the title never says "FLAC". No
/// lossy codec carries 24-bit/96 kHz, so e.g. "No Fixed Address [96khz 24bits]" is `flac_24`, not a
/// bottom-tier `mp3`. An explicit lossy bitrate/codec ("320", "V0", "AAC") still wins over a mentioned
/// hi-res *source* (a "320 from the 24-bit master" is a 320 kbps MP3).
pub fn detect_format(title: &str) -> &'static str {
    let t = title.to_lowercase();
    let hires = t.contains("24bit")
        || t.contains("24-bit")
        || t.contains("24 bit")
        || t.contains("96khz")
        || t.contains("96 khz")
        || t.contains("192khz")
        || t.contains("192 khz")
        || t.contains("/24")
        || t.contains("hi-res")
        || t.contains("hires");
    // Explicit FLAC is unambiguous.
    if t.contains("flac") {
        return if hires { "flac_24" } else { "flac" };
    }
    // A stated lossy bitrate/codec is the actual encoding, even if a hi-res source is named.
    if t.contains("320") {
        return "mp3_320";
    }
    if t.contains(" v0") || t.contains("v0)") || t.contains("vbr") {
        return "mp3_v0";
    }
    // Other lossless containers / an explicit "lossless" tag.
    if t.contains("alac") {
        return "alac";
    }
    if t.contains("wav") || t.contains("aiff") || t.contains("wavpack") {
        return "wav";
    }
    if t.contains("lossless") || t.contains(".ape") || t.contains(" ape") || t.contains(".wv") {
        return "flac";
    }
    // Hi-res specs with no codec word at all imply a lossless source.
    if hires {
        return "flac_24";
    }
    if t.contains("aac") || t.contains("m4a") || t.contains("256") {
        return "aac_256";
    }
    "mp3"
}

pub fn label_for(title: &str) -> String {
    detect_format(title).to_string()
}

/// Filter to allowed formats (with seeders, within any size cap) and sort best-first.
pub fn rank(releases: &mut Vec<Release>, profile: Option<&DownloadQualityProfile>) {
    let allowed: Vec<String> = profile
        .map(|p| p.allowed_formats.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT.iter().map(|s| s.to_string()).collect());
    let prefer_seeders = profile.map(|p| p.prefer_seeders).unwrap_or(true);
    let max_size = profile
        .and_then(|p| p.max_size_mb)
        .map(|m| m as i64 * 1024 * 1024);

    releases.retain(|r| {
        let fmt = detect_format(&r.title);
        allowed.iter().any(|a| a == fmt) && r.seeders > 0 && max_size.is_none_or(|m| r.size <= m)
    });

    // Cutoff: once a release at or above the cutoff format tier exists, drop everything below it
    // (we stop "upgrading" past the quality the user declared good enough). `allowed` is best-first,
    // so a smaller tier index = better; "at or above" means tier index <= the cutoff's.
    if let Some(cutoff_idx) = profile
        .and_then(|p| p.cutoff.as_deref())
        .and_then(|c| allowed.iter().position(|a| a == c))
    {
        let tier = |r: &Release| {
            allowed
                .iter()
                .position(|a| a == detect_format(&r.title))
                .unwrap_or(allowed.len())
        };
        if releases.iter().any(|r| tier(r) <= cutoff_idx) {
            releases.retain(|r| tier(r) <= cutoff_idx);
        }
    }

    releases.sort_by(|a, b| {
        score(b, &allowed, prefer_seeders).cmp(&score(a, &allowed, prefer_seeders))
    });
}

/// Score a release: dominant format-tier weight + a bounded seeder bonus (capped below one tier
/// step, so a better format always outranks more seeders).
fn score(r: &Release, allowed: &[String], prefer_seeders: bool) -> i64 {
    let fmt = detect_format(&r.title);
    let tier = allowed
        .iter()
        .position(|a| a == fmt)
        .unwrap_or(allowed.len());
    let tier_weight = allowed.len().saturating_sub(tier) as i64 * 1000;
    let seeder_bonus = if prefer_seeders {
        (((r.seeders.max(0) as f64).ln_1p()) * 60.0) as i64
    } else {
        0
    };
    tier_weight + seeder_bonus.min(900)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hires_without_flac_is_lossless() {
        // The real bug: a 24-bit/96 kHz release that doesn't say "FLAC" must NOT be a bottom-tier mp3.
        assert_eq!(
            detect_format("Nickelback   No Fixed Address (2014) [96khz   24bits]"),
            "flac_24"
        );
        assert_eq!(detect_format("Some Album [24-bit Hi-Res]"), "flac_24");
        assert_eq!(detect_format("Some Album (Lossless)"), "flac");
    }

    #[test]
    fn explicit_flac_and_lossy_markers() {
        assert_eq!(detect_format("Album [FLAC]"), "flac");
        assert_eq!(detect_format("Album [FLAC 24bit 96kHz]"), "flac_24");
        assert_eq!(detect_format("Album [320]"), "mp3_320");
        assert_eq!(detect_format("Album {MP3 2014}"), "mp3");
        // A stated 320 bitrate wins even when a hi-res master is mentioned.
        assert_eq!(
            detect_format("Album 320kbps (from 24bit master)"),
            "mp3_320"
        );
    }

    fn rel(title: &str, seeders: i32) -> Release {
        Release {
            guid: title.into(),
            title: title.into(),
            download_url: None,
            magnet_url: None,
            info_hash: None,
            size: 0,
            seeders,
            leechers: 0,
            indexer: None,
        }
    }

    #[test]
    fn lossless_outranks_more_seeded_lossy() {
        // The 9-seeder hi-res lossless must beat the 15-seeder 320. Format tier dominates seeders.
        let mut releases = vec![
            rel("Nickelback No Fixed Address [2014] 320", 15),
            rel("Nickelback No Fixed Address (2014) [96khz 24bits]", 9),
        ];
        rank(&mut releases, None);
        assert_eq!(detect_format(&releases[0].title), "flac_24");
        assert_eq!(releases[0].seeders, 9);
    }

    #[test]
    fn seeders_break_ties_within_a_tier() {
        let mut releases = vec![rel("Album [320]", 1), rel("Album [320]", 20)];
        rank(&mut releases, None);
        assert_eq!(releases[0].seeders, 20); // same tier → more seeders first
    }
}
