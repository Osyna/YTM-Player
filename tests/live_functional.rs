//! The functional half: settings, queue, covers, and anything else whose assertion is a
//! discrete state rather than a stopwatch. See `live.rs` for the timing-sensitive half
//! this was split from, and for MPRIS - it claims a single well-known D-Bus name shared
//! by every player process the whole suite spawns, so whichever instance is running
//! when it is checked is not guaranteed to be this test's, and it stays serial.
//!
//! Contention between the mpv processes several of these spawn at once is fine here
//! because nothing below measures *when* something happened, only *whether*.

mod common;

use common::{
    Ipc, Pty, Scratch, find_mpv_child, make_cover_track, make_tracks, set_setting, signal,
    write_config,
};
use std::time::Duration;

/// More presses than the menu has rows, so the walk always reaches the row it wants
/// without the test needing to know where it is.
const SETTING_ROW_SCAN: usize = 30;

#[test]
fn the_beat_mixing_switch_actually_switches() {
    require_tools!();
    let scratch = Scratch::new("switch");
    let media = scratch.join("media");
    make_tracks(&media, &["one", "two"], 30);
    write_config(&scratch.0, "crossfade=on\nbeat_mixing=off\n");

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 34);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));

    // Open settings and walk down to the row by name, rather than by counting - the
    // whole point of this test is that positions are not to be trusted.
    pty.send_only("s");
    assert!(pty.wait_for(Duration::from_secs(5), |s| s.contains("Beat mixing")));
    // Walk down until the cursor is on it. By name, not by counting - the point of this
    // test is that a row's position is not to be trusted.
    let mut arrived = false;
    for _ in 0..SETTING_ROW_SCAN {
        if pty.screen.text().contains("\u{25b8} Beat mixing") {
            arrived = true;
            break;
        }
        pty.send_only("j");
        pty.pump(Duration::from_millis(80));
    }
    assert!(arrived, "never reached the row:\n{}", pty.screen.text());
    let before = pty.screen.text();
    let value = |screen: &str| {
        screen
            .lines()
            .find(|line| line.contains("Beat mixing"))
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        value(&before).contains("Off"),
        "not off to begin with: {}",
        value(&before)
    );

    // Turn it on.
    pty.send_only("l");
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| {
            s.text()
                .lines()
                .find(|line| line.contains("Beat mixing"))
                .is_some_and(|line| line.contains("On"))
        }),
        "the switch did nothing: {}",
        value(&pty.screen.text())
    );
    // The row below it must not have moved with it - that was the actual bug.
    let after = pty.screen.text();
    assert!(
        after.contains("Transition"),
        "the transition row vanished: {after}"
    );

    // And back off again.
    pty.send_only("h");
    assert!(pty.wait_for(Duration::from_secs(5), |s| {
        s.text()
            .lines()
            .find(|line| line.contains("Beat mixing"))
            .is_some_and(|line| line.contains("Off"))
    }));
    pty.quit();
}

#[test]
fn a_cover_gets_a_box_and_a_track_without_one_gets_the_width() {
    require_tools!();
    let scratch = Scratch::new("cover");
    let media = scratch.join("media");
    // First a track carrying a picture, then one that is not.
    make_cover_track(&media.join("01.mp3"), "red", 40);
    make_tracks(&media, &["02"], 40);
    write_config(&scratch.0, "crossfade=off\n");

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));

    // The picture is prepared off-thread, so give it a moment to arrive.
    let mut with_cover = String::new();
    for _ in 0..80 {
        pty.pump(Duration::from_millis(120));
        let screen = pty.screen.text();
        if screen.contains('\u{2580}') {
            with_cover = screen;
            break;
        }
    }
    assert!(
        !with_cover.is_empty(),
        "no picture appeared for a track that carries one"
    );
    // Drawn inside the NOW panel, not somewhere else on the screen.
    let art_row = with_cover
        .lines()
        .position(|line| line.contains('\u{2580}'))
        .expect("art row");
    assert!(
        (1..6).contains(&art_row),
        "the picture landed on row {art_row}, outside the NOW panel"
    );

    // Now a track with no picture: the box goes away rather than sitting there empty.
    deck.command(serde_json::json!(["playlist-play-index", 1]));
    let mut without = String::new();
    for _ in 0..80 {
        pty.pump(Duration::from_millis(120));
        let screen = pty.screen.text();
        if screen.contains("02") && !screen.contains('\u{2580}') {
            without = screen;
            break;
        }
    }
    pty.quit();
    assert!(
        !without.is_empty(),
        "the picture stayed on screen after moving to a track without one"
    );
    // And the progress row is wider than it was with the box in the way.
    let bar_len = |text: &str| {
        text.lines()
            .find(|line| line.contains('\u{25b0}') || line.contains('\u{25b1}'))
            .map_or(0, |line| {
                line.matches('\u{25b1}').count() + line.matches('\u{25b0}').count()
            })
    };
    assert!(
        bar_len(&without) > bar_len(&with_cover),
        "the progress bar did not take back the width: {} vs {}",
        bar_len(&without),
        bar_len(&with_cover)
    );
}

#[test]
fn shuffle_reorders_both_decks_identically() {
    require_tools!();
    let scratch = Scratch::new("shuffle");
    let media = scratch.join("media");
    // Long enough that the queue cannot run out while the menu is being walked, which
    // ends the session and leaves the rest of the test typing into a dead terminal.
    make_tracks(&media, &["a", "b", "c", "d", "e", "f"], 30);
    // Crossfade pre-set rather than walked to: one menu walk is slow, two is a minute.
    write_config(&scratch.0, "crossfade=on\ncrossfade_secs=5\n");

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));

    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare deck");
    let original = deck.playlist();
    assert_eq!(original.len(), 6, "the queue did not load");
    assert_eq!(
        spare.playlist(),
        original,
        "the decks started out disagreeing"
    );

    set_setting(&mut pty, "Shuffle", 1);
    let shuffled = deck.playlist();
    assert_ne!(shuffled, original, "shuffle did not reorder anything");
    // The invariant that matters: the parked deck is cued *by index*, so a deck that
    // disagrees about the order cues the wrong track.
    assert_eq!(
        spare.playlist(),
        shuffled,
        "the decks disagree after a shuffle - the next transition would play the wrong track"
    );

    set_setting(&mut pty, "Shuffle", 1);
    assert_eq!(
        deck.playlist(),
        original,
        "unshuffle did not restore the order"
    );
    assert_eq!(
        spare.playlist(),
        original,
        "the decks disagree after unshuffling"
    );
    pty.quit();
}

#[test]
fn a_session_resumes_on_the_track_and_the_second() {
    require_tools!();
    let scratch = Scratch::new("resume");
    let media = scratch.join("media");
    make_tracks(&media, &["one", "two", "three"], 20);
    let path = media.to_str().expect("utf-8 path").to_string();

    // Play into the second track, then leave.
    let mut first = Pty::spawn(&[&path], &scratch.0, 100, 30);
    assert!(first.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&first.deck_socket(), Duration::from_secs(5)).expect("deck");
    deck.command(serde_json::json!(["playlist-play-index", 1]));
    first.pump(Duration::from_secs(2));
    // Which file that is depends on the order the player built the queue in, not on the
    // order they were created in - so ask rather than assume.
    let playing = deck
        .get("media-title")
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("something is playing");
    deck.seek(5.0);
    // The resume point is written on a timer, so give it one - but stay well short of
    // the end, or the track advances and the point is for the one after.
    first.pump(Duration::from_secs(7));
    drop(deck);
    first.quit();
    drop(first);

    // A bare launch offers it, and taking the offer lands where we left off.
    let mut second = Pty::spawn(&[], &scratch.0, 100, 30);
    assert!(
        second.wait_for(Duration::from_secs(15), |s| s.contains("Resume")),
        "no resume offer on the URL bar:\n{}",
        second.screen.text()
    );
    assert!(
        second.screen.contains(playing.trim_end_matches(".mp3")),
        "the offer names something other than {playing}:\n{}",
        second.screen.text()
    );
    second.key("\t");
    assert!(
        second.wait_for(Duration::from_secs(20), |s| s.contains("2/3")),
        "resume did not land on the second track:\n{}",
        second.screen.text()
    );
    let mut deck = Ipc::connect(&second.deck_socket(), Duration::from_secs(5)).expect("deck");
    let position = deck.number("time-pos").unwrap_or(0.0);
    assert!(
        position > 4.0,
        "resumed at {position:.1}s, not near the 5s it stopped at"
    );
    second.quit();
}

#[test]
fn the_centre_panel_cycles_and_its_controls_follow() {
    require_tools!();
    let scratch = Scratch::new("panel");
    let media = scratch.join("media");
    make_tracks(&media, &["a", "b"], 10);

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("VIEW ▸ QUEUE")));
    // The queue brings its own controls...
    assert!(
        pty.screen.contains("✎ Edit (E)") && pty.screen.contains("⬇ All (a)"),
        "queue controls missing:\n{}",
        pty.screen.text()
    );

    // ...clicking the panel body moves to the next pane, and they go with it.
    pty.click(50, 20);
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| s.contains("VIEW ▸ SCOPE")),
        "a click on the panel did not cycle it:\n{}",
        pty.screen.text()
    );
    assert!(
        !pty.screen.contains("⬇ All (a)"),
        "queue controls survived onto the scope:\n{}",
        pty.screen.text()
    );

    // `v` opens the chooser over the view rather than replacing it.
    pty.key("v");
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| s.contains("╸VIEW╺")),
        "the chooser did not open:\n{}",
        pty.screen.text()
    );
    assert!(
        pty.screen.contains("YTM://PLAYER"),
        "the chooser replaced the view instead of covering it"
    );
    pty.key("\x1b");
    pty.quit();
}

#[test]
fn a_wedged_mpv_is_reported_instead_of_drawn_over() {
    require_tools!();
    let scratch = Scratch::new("stall");
    let media = scratch.join("media");
    make_tracks(&media, &["a"], 20);

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    assert!(
        !pty.screen.contains("NOT RESPONDING"),
        "warned about a healthy mpv:\n{}",
        pty.screen.text()
    );

    // Stop mpv dead without killing it: the socket stays open and accepts writes, but
    // nothing will ever answer again. This is the failure the status bar exists for,
    // and the one that used to be invisible.
    let pid = find_mpv_child(pty.child_id()).expect("mpv is running");
    signal(pid, libc::SIGSTOP);
    let warned = pty.wait_for(Duration::from_secs(15), |s| s.contains("NOT RESPONDING"));
    signal(pid, libc::SIGCONT);
    assert!(
        warned,
        "a stopped mpv was drawn as if it were playing:\n{}",
        pty.screen.text()
    );

    // ...and it stops apologising once mpv answers again.
    assert!(
        pty.wait_for(Duration::from_secs(15), |s| !s.contains("NOT RESPONDING")),
        "kept warning after mpv recovered:\n{}",
        pty.screen.text()
    );
    pty.quit();
}

#[test]
fn the_library_indexes_and_filters() {
    require_tools!();
    let scratch = Scratch::new("library");
    let queue = scratch.join("media");
    make_tracks(&queue, &["a"], 10);
    // ~/Music is where an unindexed library looks first.
    let music = scratch.join("Music/Artist/Album");
    make_tracks(&music, &["zither song", "banjo song"], 2);

    let mut pty = Pty::spawn(&[queue.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));

    // Chooser -> Library is the last row.
    pty.key("v");
    for _ in 0..3 {
        pty.key("j");
    }
    pty.key("\r");
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| s.contains("VIEW ▸ LIBRARY")),
        "the library pane did not open:\n{}",
        pty.screen.text()
    );

    pty.key("R");
    assert!(
        pty.wait_for(Duration::from_secs(30), |s| s.contains("zither song")),
        "the scan did not find the tracks under ~/Music:\n{}",
        pty.screen.text()
    );

    // Filter mode narrows as it is typed, and leaves the letters out of the hotkeys.
    pty.key("/");
    pty.key("banjo");
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| !s.contains("zither song")),
        "the filter did not narrow the list:\n{}",
        pty.screen.text()
    );
    assert!(
        pty.screen.contains("banjo song"),
        "the filter dropped the row it should have kept:\n{}",
        pty.screen.text()
    );
    pty.key("\x1b");
    pty.quit();
}

#[test]
fn the_equalizer_reaches_the_deck_and_flat_really_is_nothing() {
    require_tools!();
    let scratch = Scratch::new("eq");
    let media = scratch.join("media");
    make_tracks(&media, &["a"], 20);

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 110, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("deck");

    // Flat is the default, and it is nothing at all rather than five filters at 0 dB -
    // so nothing an equalizer emits should be in the chain yet.
    let mut af = || -> String {
        deck.get("af")
            .map(|value| value.to_string())
            .unwrap_or_default()
    };
    assert!(
        !af().contains("equalizer=") && !af().contains("bass=g="),
        "Flat put a filter in the chain: {}",
        af()
    );

    // Open the equalizer, walk down to a preset with a shape, and take it.
    pty.key("g");
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| s.contains("EQUALIZER")),
        "the equalizer view did not open:\n{}",
        pty.screen.text()
    );
    assert!(
        pty.screen.contains("Club") && pty.screen.contains("Late night"),
        "the preset list is not there:\n{}",
        pty.screen.text()
    );
    pty.key("j");
    pty.key("\r");
    // In the view, the feedback is the `●` marker moving onto the row - the status bar
    // is behind this view, so a toast would be invisible from here.
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| s.contains("▸ ● Club")),
        "choosing a preset did not mark it as the one in use:\n{}",
        pty.screen.text()
    );

    // The chain is what proves it: the preset's own filters, on the deck, now.
    let mut shaped = false;
    for _ in 0..40 {
        pty.pump(Duration::from_millis(100));
        if af().contains("bass=g=6") && af().contains("equalizer=f=1000") {
            shaped = true;
            break;
        }
    }
    assert!(shaped, "Club never reached the deck's af chain: {}", af());
    // And the preamp with it - a boost nothing makes room for is a clipped boost.
    assert!(
        af().contains("volume=-"),
        "the preset boosts without taking anything back: {}",
        af()
    );

    // Back to Flat, and straight out of the view: the toast set on choosing is only ever
    // visible from the main view, and it steps aside after a few seconds, so this is the
    // one moment it can be read.
    // Sent in one burst rather than three settled presses: the confirmation steps aside
    // after a few seconds by design, so waiting out a settle between each keystroke is
    // spending the very thing being checked for.
    pty.send_only("k");
    pty.send_only("\r");
    pty.send_only("g");
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| s.contains("EQ · Flat")),
        "no confirmation once the view was closed:\n{}",
        pty.screen.text()
    );

    // And the chain is clean again rather than left holding a flat curve. Read off the
    // deck, so it does not matter which view is up by now.
    let mut cleared = false;
    for _ in 0..40 {
        pty.pump(Duration::from_millis(100));
        if !af().contains("equalizer=") && !af().contains("bass=g=") {
            cleared = true;
            break;
        }
    }
    assert!(cleared, "Flat left filters behind: {}", af());
    pty.quit();
}
