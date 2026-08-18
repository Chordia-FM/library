//! The `ALBUM - Tracklist.txt` parser, against a real file.
//!
//! `SAMPLE` is copied verbatim from one a downloader produced, alignment padding and all. That is
//! the point: the format is hand-written by a third-party tool, so a fixture I composed myself would
//! only prove the parser agrees with my idea of it.

use std::time::Duration;

use chordia_library::tracklist::{self, Credit};

const SAMPLE: &str = "\
======================================================================
ALBUM      : Triple X Years In The Game [E]
COMPOSER   : Various Composers
MAIN ART.  : Atmosphere
LABEL      : Rhymesayers
GENRE      : Hip-Hop/Rap
RELEASE    : 2025-05-23
QUALITY    : FLAC (24-Bit / 44.1 kHz)
======================================================================

01. God's Bathroom Floor [E]                                 [03:58]
    * Joe LaPorta, Masterer
    * Atmosphere, MainArtist
    * STRESS, Producer
    * Ant, Producer, Beats, MainArtist
    * Slug, Vocals, MainArtist
    * Anthony Davis, ComposerLyricist
    * Sean Daley, ComposerLyricist
    * Joe Mabbott, Mixer
    * ANT TURN THAT SNARE DOWN, MusicPublisher
    * Upside Down Heart music, MusicPublisher

02. Scapegoat [E]                                            [03:53]
    * Joe LaPorta, Masterer
    * Atmosphere, MainArtist
    * Ant, Producer, Beats, MainArtist
    * Slug, Vocals, MainArtist
    * Anthony Davis, ComposerLyricist
    * Sean Daley, ComposerLyricist
    * Joe Mabbott, Mixer
";

fn sample() -> tracklist::Tracklist {
    tracklist::parse(SAMPLE).expect("the sample is a tracklist")
}

#[test]
fn reads_the_header() {
    let album = sample().album;
    assert_eq!(album.title.as_deref(), Some("Triple X Years In The Game"));
    assert!(album.explicit, "the [E] on the album line");
    assert_eq!(album.main_artist.as_deref(), Some("Atmosphere"));
    assert_eq!(album.label.as_deref(), Some("Rhymesayers"));
    assert_eq!(album.genre.as_deref(), Some("Hip-Hop/Rap"));
    assert_eq!(album.release_date.as_deref(), Some("2025-05-23"));
    assert_eq!(album.quality.as_deref(), Some("FLAC (24-Bit / 44.1 kHz)"));
    assert_eq!(album.composer.as_deref(), Some("Various Composers"));
}

/// The album title must come back WITHOUT `[E]`, or it will never match the tagged album and every
/// lookup that joins on it silently misses.
#[test]
fn the_explicit_marker_is_not_part_of_the_title() {
    let list = sample();
    assert!(!list.album.title.unwrap().contains("[E]"));
    assert_eq!(list.tracks[0].title, "God's Bathroom Floor");
    assert!(list.tracks[0].explicit);
}

#[test]
fn reads_every_track_row() {
    let tracks = sample().tracks;
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].number, 1);
    assert_eq!(tracks[0].duration, Some(Duration::from_secs(238)));
    assert_eq!(tracks[1].number, 2);
    assert_eq!(tracks[1].title, "Scapegoat");
    assert_eq!(tracks[1].duration, Some(Duration::from_secs(233)));
}

/// One name, several roles — not one row per role. A credits panel that repeated "Ant" three times
/// would be the visible symptom.
#[test]
fn a_person_keeps_all_of_their_roles() {
    let tracks = sample().tracks;
    let ant = tracks[0]
        .credits
        .iter()
        .find(|c| c.name == "Ant")
        .expect("Ant is credited");
    assert_eq!(ant.roles, vec!["Producer", "Beats", "MainArtist"]);
}

#[test]
fn credits_attach_to_the_track_above_them() {
    let tracks = sample().tracks;
    assert_eq!(tracks[0].credits.len(), 10);
    // Scapegoat has no STRESS and no publishers — proof the parser did not spill the first track's
    // block into the second.
    assert_eq!(tracks[1].credits.len(), 7);
    assert!(!tracks[1].credits.iter().any(|c| c.name == "STRESS"));
}

/// Publishers arrive in the same list as performers. Without separating them they end up in artist
/// lists and search as though they were people.
#[test]
fn publishers_are_not_people() {
    let tracks = sample().tracks;
    let organisations: Vec<&str> = tracks[0]
        .credits
        .iter()
        .filter(|c| c.is_organisation())
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        organisations,
        vec!["ANT TURN THAT SNARE DOWN", "Upside Down Heart music"]
    );
    let slug = tracks[0].credits.iter().find(|c| c.name == "Slug").unwrap();
    assert!(!slug.is_organisation());
}

/// The guard against a tracklist left in the wrong folder. It parses perfectly and would write
/// confident, wrong credits — every field looks like a real value, so nobody would catch it.
#[test]
fn durations_decide_whether_this_file_describes_this_album() {
    let list = sample();
    let tolerance = Duration::from_secs(2);

    let right = [(1, Duration::from_secs(238)), (2, Duration::from_secs(232))];
    assert!(list.matches(&right, tolerance), "one second off is fine");

    let wrong = [(1, Duration::from_secs(180)), (2, Duration::from_secs(200))];
    assert!(!list.matches(&wrong, tolerance), "a different album");

    let unrelated = [(40, Duration::from_secs(238))];
    assert!(
        !list.matches(&unrelated, tolerance),
        "sharing no track numbers is not a match"
    );

    assert!(!list.matches(&[], tolerance), "nothing to compare");
}

#[test]
fn a_file_that_is_not_a_tracklist_is_rejected() {
    assert!(tracklist::parse("").is_none());
    assert!(tracklist::parse("just some prose about the album").is_none());
    // Header but no tracks: describes nothing.
    assert!(tracklist::parse("ALBUM : Something\nLABEL : Someone").is_none());
    // Numbered lines but no header: as likely a README as a credits file.
    assert!(tracklist::parse("01. A thing\n02. Another").is_none());
}

/// A bare name says nothing about what the person did, and a blank role column is worse than one
/// entry fewer.
#[test]
fn a_credit_with_no_role_is_dropped() {
    let text = "ALBUM : X\n\n01. Track [02:00]\n    * Somebody\n    * Someone Else, Mixer\n";
    let list = tracklist::parse(text).expect("parses");
    assert_eq!(
        list.tracks[0].credits,
        vec![Credit {
            name: "Someone Else".into(),
            roles: vec!["Mixer".into()],
        }]
    );
}

/// Titles contain brackets of their own, so the duration must be read from the LAST group rather
/// than the first.
#[test]
fn a_bracketed_title_does_not_become_the_duration() {
    let text = "ALBUM : X\n\n01. Song [Live] [E]                    [04:20]\n";
    let list = tracklist::parse(text).expect("parses");
    assert_eq!(list.tracks[0].title, "Song [Live]");
    assert!(list.tracks[0].explicit);
    assert_eq!(list.tracks[0].duration, Some(Duration::from_secs(260)));
}

#[test]
fn a_long_mix_may_carry_hours() {
    let text = "ALBUM : X\n\n01. DJ Set [1:12:30]\n";
    let list = tracklist::parse(text).expect("parses");
    assert_eq!(list.tracks[0].duration, Some(Duration::from_secs(4350)));
}
