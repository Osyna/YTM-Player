//! The timing-sensitive half: crossfade overlap, beat sync, release lag - anything that
//! measures a real window of wall-clock time against mpv's own decoding. Load-shy by
//! nature (a 30 s fade overlapped by another test's mpv reads short), so this half stays
//! serial; see `live_functional.rs` for the half that does not care.
//!
//! MPRIS lives here too, for a different reason: it claims a single well-known D-Bus
//! name shared by every player process the whole suite spawns, so a copy of it running
//! alongside another test is not guaranteed to be the one `playerctl` actually reaches.
//!
//! Each test drives the real binary on a pseudo-terminal and then asks mpv itself what
//! happened, because "the screen said so" is not proof that two decks were audible at
//! once. They are slow by nature - a crossfade takes as long as a crossfade - so they
//! use short generated tracks and seek to the interesting moment rather than waiting for
//! it.

mod common;

use common::{
    Ipc, Pty, Scratch, make_beat_track, make_beat_track_with_lead, make_tagged_track, make_tracks,
    playerctl, set_setting, write_config,
};
use std::time::Duration;

#[test]
fn a_crossfade_really_overlaps_two_decks() {
    require_tools!();
    let scratch = Scratch::new("crossfade");
    let media = scratch.join("media");
    make_tracks(&media, &["one", "two", "three"], 20);

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(
        pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")),
        "player never drew a frame:\n{}",
        pty.screen.text()
    );
    set_setting(&mut pty, "Crossfade", 1);

    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10))
        .expect("turning crossfade on must spawn the second decoder");

    // Jump to just before the transition and watch both decks across it.
    deck.seek(11.0);
    let mut overlapped = false;
    let mut swapped = false;
    for _ in 0..80 {
        pty.pump(Duration::from_millis(200));
        let playing = deck.number("volume").unwrap_or(0.0);
        let incoming = spare.number("volume").unwrap_or(0.0);
        let incoming_pos = spare.number("time-pos").unwrap_or(-1.0);
        // The whole claim: both decks audible at once, and the incoming one moving.
        if playing > 5.0 && incoming > 5.0 && incoming_pos > 0.0 {
            overlapped = true;
        }
        // ...and afterwards they have traded roles: the deck that was playing is
        // silenced and parked, and the other one carries the level.
        if overlapped && playing < 5.0 && incoming > 50.0 && incoming_pos > 0.0 {
            swapped = true;
            break;
        }
    }
    assert!(
        overlapped,
        "the decks never played at the same time - this is a dip, not a crossfade:\n{}",
        pty.screen.text()
    );
    assert!(swapped, "the overlap never completed into a swap");
    pty.quit();
}

#[test]
fn sync_puts_the_two_grids_on_top_of_each_other() {
    require_tools!();
    let scratch = Scratch::new("phase");
    let media = scratch.join("media");
    // The same tempo on both, so nothing here is measuring the tempo match: what is left
    // is purely how well the two grids line up, which is what a sync is for.
    const BPM: f64 = 128.0;
    const LEAD_A: f64 = 0.0;
    const LEAD_B: f64 = 0.75;
    make_beat_track_with_lead(&media.join("01.mp3"), BPM, 70, LEAD_A);
    make_beat_track_with_lead(&media.join("02.mp3"), BPM, 70, LEAD_B);
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=8\ntransition=sync_blend\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(70.0);
    deck.seek(duration - 52.0);

    let beat = 60.0 / BPM;
    let mut errors: Vec<f64> = Vec::new();
    let mut first_error: Option<f64> = None;

    for _ in 0..600 {
        pty.pump(Duration::from_millis(120));
        let b_vol = spare.number("volume").unwrap_or(0.0);
        let a_vol = deck.number("volume").unwrap_or(0.0);
        if a_vol < 5.0 || b_vol < 5.0 {
            if !errors.is_empty() {
                break;
            }
            continue;
        }
        // Bracket B's reading with two of A's so the two positions can be compared at the
        // same instant: they are three round trips, not one, and a millisecond of skew
        // would otherwise be indistinguishable from a millisecond of drift.
        let (before, mid, after) = (
            deck.number("time-pos"),
            spare.number("time-pos"),
            deck.number("time-pos"),
        );
        let (Some(before), Some(b_pos), Some(after)) = (before, mid, after) else {
            continue;
        };
        let a_pos = f64::midpoint(before, after);
        if after - before > 0.05 {
            continue; // too much skew to trust this sample
        }
        let phase_a = (a_pos - LEAD_A).rem_euclid(beat);
        let phase_b = (b_pos - LEAD_B).rem_euclid(beat);
        let mut error = phase_a - phase_b;
        if error > beat / 2.0 {
            error -= beat;
        } else if error < -beat / 2.0 {
            error += beat;
        }
        if first_error.is_none() {
            first_error = Some(error * 1000.0);
        }
        errors.push(error * 1000.0);
    }

    assert!(errors.len() > 8, "only {} usable samples", errors.len());
    // The median of the settled part, not the worst of it. Two decks' positions are read
    // over separate sockets and mpv reports each to the resolution of its own audio
    // period, so adjacent samples of this measurement disagree by twenty milliseconds in a
    // tenth of a second - which no audio could actually do, and is the instrument rather
    // than the mix. A median sees through that; a maximum reports it.
    let settled = &errors[errors.len() / 4..];
    let mean = settled.iter().sum::<f64>() / settled.len() as f64;
    let mut spread: Vec<f64> = settled.iter().map(|e| e.abs()).collect();
    spread.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let median = spread[spread.len() / 2];
    println!(
        "PHASE median {median:.1} ms  mean {mean:+.1} ms  first {:+.1} ms  over {} samples",
        first_error.unwrap_or(0.0),
        errors.len()
    );
    pty.quit();
    // A flam is audible from about fifteen milliseconds and obvious by thirty. Ten is the
    // point at which two records are simply in time with each other.
    assert!(
        median < 10.0,
        "the two grids sit {median:.1} ms apart - that is a flam, not a sync"
    );
    assert!(
        mean.abs() < 10.0,
        "the arriving track runs {mean:+.1} ms from the one it is mixed with"
    );
}

#[test]
fn a_plain_fade_needs_no_analysis_and_a_scored_one_does() {
    require_tools!();
    let scratch = Scratch::new("modes");
    let media = scratch.join("media");
    make_beat_track(&media.join("01.mp3"), 128.0, 60);
    make_beat_track_with_lead(&media.join("02.mp3"), 132.0, 60, 0.75);
    // Beat mixing off, but a scored transition named all the same. The score should be
    // ignored: no analysis, no tempo pull, just a fade.
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=off\ncrossfade_secs=8\ntransition=sync_blend\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(60.0);
    deck.seek(duration - 14.0);

    let mut both_up = false;
    let mut stretched = false;
    let mut analysed = false;
    for _ in 0..320 {
        pty.pump(Duration::from_millis(100));
        if pty.screen.text().contains("ANALYSING") || pty.screen.text().contains('\u{25c8}') {
            analysed = true;
        }
        let a = deck.number("volume").unwrap_or(0.0);
        let b = spare.number("volume").unwrap_or(0.0);
        if a > 5.0 && b > 5.0 {
            both_up = true;
        }
        if (spare.number("speed").unwrap_or(1.0) - 1.0).abs() > 0.004 {
            stretched = true;
        }
        if both_up && a < 1.0 {
            break;
        }
    }
    pty.quit();

    assert!(both_up, "a plain fade still has to overlap the two tracks");
    assert!(
        !stretched,
        "the arriving track was pitched with beat mixing off - that is the scored path running"
    );
    assert!(
        !analysed,
        "the next track was analysed with beat mixing off - that is the cost the setting \
         exists to avoid"
    );
}

#[test]
fn the_player_says_what_it_found_in_the_next_track() {
    require_tools!();
    let scratch = Scratch::new("analyse");
    let media = scratch.join("media");
    make_beat_track(&media.join("01.mp3"), 128.0, 70);
    make_beat_track_with_lead(&media.join("02.mp3"), 132.0, 70, 0.75);
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=8\ntransition=sync_blend\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(70.0);
    deck.seek(duration - 45.0);

    // Two stages, and the first must come long before the second. The next track is
    // analysed as soon as it is known - a whole-song read with the whole current track's
    // length to finish in - so "ready" has to appear while the transition is still far
    // off. Then the run-up restates it as what was actually done: pulled and locked.
    let mut said_ready_at: Option<f64> = None;
    let mut said_matched = String::new();
    let began = std::time::Instant::now();
    for _ in 0..320 {
        pty.pump(Duration::from_millis(100));
        let screen = pty.screen.text();
        if said_ready_at.is_none() && screen.contains("ready to mix") {
            said_ready_at = Some(began.elapsed().as_secs_f64());
        }
        if let Some(line) = screen
            .lines()
            .find(|line| line.contains("pulled") || line.contains("in time"))
        {
            said_matched = line
                .split('\u{2500}')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            break;
        }
    }

    pty.quit();
    let ready_at = said_ready_at.expect(
        "the next track was never reported as analysed ahead of time - the whole point \
         of starting early is that this appears long before the mix needs it",
    );
    assert!(
        ready_at < 20.0,
        "analysis of the next track only finished after {ready_at:.0} s"
    );
    // 132 BPM, measured from a track nobody has heard yet - not the 128 of the one that
    // is playing, which would mean it had reported the wrong deck.
    assert!(
        said_matched.contains("13"),
        "the run-up never restated the analysis as a stretch: {said_matched:?}"
    );
}

#[test]
fn a_sync_transition_locks_the_tempo_and_walks_it_back() {
    require_tools!();
    let scratch = Scratch::new("sync");
    let media = scratch.join("media");
    // Two tempos far enough apart to see the lock and close enough that the player is
    // willing to stretch: beyond about six per cent it refuses, which is the point.
    make_beat_track(&media.join("01.mp3"), 128.0, 70);
    // A lead-in on the arriving track, as every real record has: its first bar line is
    // three quarters of a second in, so pressing play on it would put the two decks out
    // of time even at an identical tempo.
    make_beat_track_with_lead(&media.join("02.mp3"), 132.0, 70, 0.75);
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=8\ntransition=sync_blend\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(70.0);
    deck.seek(duration - 52.0);

    let mut locked_at: Option<f64> = None;
    let mut started_on_a_bar = false;
    let mut both_audible = false;
    let mut released = false;
    let mut lowest = 1.0f64;

    for _ in 0..600 {
        pty.pump(Duration::from_millis(120));
        let b_vol = spare.number("volume").unwrap_or(0.0);
        let b_speed = spare.number("speed").unwrap_or(1.0);
        let b_pos = spare.number("time-pos").unwrap_or(-1.0);
        let a_vol = deck.number("volume").unwrap_or(0.0);

        // Pulled to the other deck's tempo while still silent.
        if b_vol < 1.0 && (b_speed - 1.0).abs() > 0.005 {
            locked_at.get_or_insert(b_speed);
            lowest = lowest.min(b_speed);
        }
        // The first moment it is audible, it should already be past its lead-in: the
        // player seeks it onto its own bar line rather than pressing play on the file.
        if !started_on_a_bar && b_vol > 0.5 && b_pos > 0.3 {
            started_on_a_bar = true;
        }
        if a_vol > 5.0 && b_vol > 5.0 {
            both_audible = true;
        }
        // ...and walked back to its own speed once it is alone.
        if both_audible && a_vol < 1.0 && (b_speed - 1.0).abs() < 0.004 && locked_at.is_some() {
            released = true;
            break;
        }
    }

    let locked = locked_at.expect("the arriving track was never pulled to the other tempo");
    // 128/132 is about 0.970; anything near 1.0 would mean it never really locked.
    assert!(
        (0.94..0.99).contains(&locked),
        "locked at {locked:.3}, which is not 128 against 132"
    );
    assert!(
        started_on_a_bar,
        "the arriving deck started at the very top of the file - it was beat-matched but \
         not put in time"
    );
    assert!(both_audible, "the two never overlapped");
    assert!(
        released,
        "the tempo was never walked back - it locked at {lowest:.3} and stayed there"
    );
    pty.quit();
}

#[test]
fn a_fade_length_is_the_whole_move_not_just_the_overlap() {
    require_tools!();
    let scratch = Scratch::new("split");
    let media = scratch.join("media");
    // Long enough for the longest fade: the player declines to overlap at all unless the
    // track is at least twice the span, which for thirty seconds is a minute.
    make_tracks(&media, &["one", "two"], 70);
    // The longest fade there is, so the ceiling is exercised rather than described. The
    // plain fade puts the join in the middle, so fifteen of the thirty fall before the old
    // track ends and fifteen after.
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=30\ntransition=crossfade\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(70.0);
    deck.seek(duration - 26.0);

    let start = std::time::Instant::now();
    let mut overlap_began: Option<f64> = None;
    let mut old_track_ended: Option<f64> = None;
    let mut still_working_after: Option<f64> = None;

    for _ in 0..600 {
        pty.pump(Duration::from_millis(120));
        let now = start.elapsed().as_secs_f64();
        let a_vol = deck.number("volume").unwrap_or(0.0);
        let b_vol = spare.number("volume").unwrap_or(0.0);
        let a_af = deck.filter_graph();
        let b_af = spare.filter_graph();
        // Whichever deck carries the tap is the one the player considers current.
        let (player_af, player_is_b) = if b_af.contains("asplit") {
            (b_af.clone(), true)
        } else {
            (a_af.clone(), false)
        };

        if overlap_began.is_none() && b_vol > 1.0 {
            overlap_began = Some(now);
        }
        if overlap_began.is_some() && old_track_ended.is_none() && player_is_b {
            old_track_ended = Some(now);
        }
        // After the join the surviving deck is alone but still being worked on - that is
        // the second half of the span, and the thing that makes it fifteen and not seven.
        if old_track_ended.is_some() && player_af.contains("highpass") {
            still_working_after = Some(now);
        }
        if old_track_ended.is_some_and(|at| now - at > 17.0) {
            break;
        }
        let _ = a_vol;
    }

    let began = overlap_began.expect("the overlap never started");
    let ended = old_track_ended.expect("the decks never traded places");
    let overlap = ended - began;
    assert!(
        (12.0..=18.0).contains(&overlap),
        "a 30 s fade overlapped for {overlap:.1} s - it should be about half of it, \
         with the rest falling after the old track ends"
    );
    let worked_until = still_working_after
        .expect("nothing happened after the join - the second half of the span was padding");
    let tail = worked_until - ended;
    assert!(
        tail > 5.0,
        "the arriving track was only shaped for {tail:.1} s after the join - a 30 s fade \
         should go on working for about fifteen of them"
    );
    pty.quit();
}

#[test]
fn the_long_blend_plays_out_its_whole_score() {
    require_tools!();
    let scratch = Scratch::new("blend");
    let media = scratch.join("media");
    // Two tempos close enough to be worth matching and far enough apart to see it happen.
    make_beat_track(&media.join("01.mp3"), 128.0, 70);
    make_beat_track(&media.join("02.mp3"), 132.0, 70);
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=8\ntransition=long_blend\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(70.0);
    // Far enough out that the tempo is found and the whole sixteen bars fit.
    deck.seek(duration - 52.0);

    let mut tempo_matched = false;
    let mut a_bass_out = false;
    let mut a_highs_out = false;
    let mut b_came_up_bassless = false;
    let mut a_faded_out = false;
    let mut waited_bassless = 0;
    let mut dropped = false;
    let mut tempo_returned = false;

    // Generous: the whole move is sixteen bars, and it now begins later than it used to -
    // a counted score is positioned so its own silence lands on the end of the old track,
    // which for this one is three quarters of the way in rather than all the way.
    for _ in 0..600 {
        pty.pump(Duration::from_millis(120));
        let a_vol = deck.number("volume").unwrap_or(0.0);
        let b_vol = spare.number("volume").unwrap_or(0.0);
        let b_speed = spare.number("speed").unwrap_or(1.0);
        let a_af = deck.filter_graph();
        let b_af = spare.filter_graph();

        // The arriving deck is pulled to the leaving one's tempo before it is audible.
        if b_vol < 1.0 && (b_speed - 1.0).abs() > 0.005 {
            tempo_matched = true;
        }
        if a_af.contains("bass=g=-40") {
            a_bass_out = true;
        }
        if a_bass_out && a_af.contains("treble=g=-40") {
            a_highs_out = true;
        }
        // It comes up with no low end at all - that is the whole trick.
        if b_vol > 50.0 && b_af.contains("bass=g=-40") {
            b_came_up_bassless = true;
        }
        if b_came_up_bassless && a_vol < 5.0 {
            a_faded_out = true;
        }
        // ...and stays bassless for the wait.
        if a_faded_out && !dropped && b_af.contains("bass=g=-40") {
            waited_bassless += 1;
        }
        // The drop: its filter clears in one step rather than fading.
        if a_faded_out && waited_bassless > 4 && b_vol > 50.0 && !b_af.contains("bass=g=-") {
            dropped = true;
        }
        if dropped && (b_speed - 1.0).abs() < 0.005 {
            tempo_returned = true;
            break;
        }
    }

    assert!(
        tempo_matched,
        "the arriving track was never pulled to the leaving one's tempo"
    );
    assert!(a_bass_out, "the leaving track kept its low end");
    assert!(a_highs_out, "the leaving track kept its top end");
    assert!(
        b_came_up_bassless,
        "the arriving track came up with its bass already in"
    );
    assert!(a_faded_out, "the leaving track never went away");
    assert!(
        waited_bassless > 4,
        "the four bars of no bass did not happen - only {waited_bassless} samples of it"
    );
    assert!(dropped, "the bass never dropped");
    assert!(
        tempo_returned,
        "the arriving track never got back to its own tempo"
    );
    pty.quit();
}

#[test]
fn the_automix_lands_a_transition_on_the_beat() {
    require_tools!();
    let scratch = Scratch::new("beat");
    let media = scratch.join("media");
    make_beat_track(&media.join("01.mp3"), 128.0, 40);
    make_beat_track(&media.join("02.mp3"), 128.0, 40);
    // Pre-set rather than walked through the menu: the walk takes long enough that the
    // track under test has moved on by the time it finishes.
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=6\ntransition=bass_swap\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(40.0);
    // Inside the window where the tracker listens, with room to find the tempo first.
    deck.seek(duration - 26.0);

    // It must find the tempo it was given...
    let found = pty.wait_for(Duration::from_secs(20), |s| s.contains("BPM"));
    assert!(
        found,
        "the automix never found a tempo:\n{}",
        pty.screen.text()
    );
    let line = pty.screen.find_line("BPM").unwrap_or_default();
    let bpm: f64 = line
        .split_whitespace()
        .find_map(|word| word.parse().ok())
        .unwrap_or_default();
    assert!(
        (bpm - 128.0).abs() <= 3.0 || (bpm - 64.0).abs() <= 2.0,
        "read {bpm} BPM from a 128 BPM track: {line}"
    );

    // ...and use it. Either outcome proves the machinery ran: the transition was held
    // back for a bar line, or it was already on one when the window opened - which is the
    // better of the two and happens when the grid lands early. Demanding both makes this
    // fail on a loaded machine, where the tracker commits later and there is no room left
    // to wait, which is the designed fallback rather than a fault.
    let mut held = false;
    let mut aligned = false;
    for _ in 0..250 {
        pty.pump(Duration::from_millis(200));
        held |= pty.screen.contains("holding");
        aligned |= pty.screen.contains("ON THE BEAT");
        if held || aligned {
            break;
        }
    }
    assert!(
        held || aligned,
        "the transition neither waited for a bar line nor landed on one:\n{}",
        pty.screen.text()
    );
    pty.quit();
}

#[test]
fn the_automix_applies_the_chosen_transition_style() {
    require_tools!();
    let scratch = Scratch::new("automix");
    let media = scratch.join("media");
    make_tracks(&media, &["one", "two", "three"], 30);
    // The default style applies no filters to either deck at all, so a filter appearing
    // is itself the proof that this setting reached the engine.
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=5\ntransition=bass_swap\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));

    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare deck");

    // Start from a known place. Walking the settings menu takes long enough that the
    // queue has moved on by now, and a transition sampled from halfway through shows only
    // its second half - which is exactly the half a bass swap has no incoming filter in.
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(20.0);
    // Far enough out that the cue has not happened yet, so the whole overlap is observed.
    deck.seek(duration - 13.0);

    let mut incoming_held_back = false;
    let mut outgoing_pulled_out = false;
    let mut both_audible = false;
    let mut tap_on_exactly_one_deck = true;
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..220 {
        pty.pump(Duration::from_millis(60));
        let playing = deck.number("volume").unwrap_or(0.0);
        let incoming = spare.number("volume").unwrap_or(0.0);
        let out_af = deck.filter_graph();
        let in_af = spare.filter_graph();
        let note = format!("out {playing:>5.0} [{out_af:.34}] in {incoming:>5.0} [{in_af:.34}]");
        if seen.last() != Some(&note) {
            seen.push(note);
        }

        if playing > 5.0 && incoming > 5.0 {
            both_audible = true;
        }
        // The arriving track has its low end held down while it arrives...
        if incoming > 1.0 && in_af.contains("bass=g=-") {
            incoming_held_back = true;
        }
        // ...and the leaving one has its own pulled out as it goes. Both are things a
        // plain crossfade never does: it applies no filter to either deck at all.
        if playing > 1.0 && out_af.contains("bass=g=-") {
            outgoing_pulled_out = true;
        }
        // Exactly one deck carries the visualizer tap throughout, or the scope goes blank
        // at the moment there is most to look at. Deliberately not "the deck on the first
        // socket": the two trade roles at the swap.
        let tapped = usize::from(out_af.contains("asplit")) + usize::from(in_af.contains("asplit"));
        if (playing > 5.0 || incoming > 5.0) && tapped != 1 {
            tap_on_exactly_one_deck = false;
        }
        if incoming_held_back && outgoing_pulled_out && both_audible {
            break;
        }
    }
    let log = seen.join("\n  ");
    assert!(both_audible, "no overlap happened at all:\n  {log}");
    assert!(
        incoming_held_back,
        "the arriving track was never filtered - the style did not reach the decks:\n  {log}"
    );
    assert!(
        outgoing_pulled_out,
        "the leaving track kept its low end - this is a crossfade, not a bass swap:\n  {log}"
    );
    assert!(
        tap_on_exactly_one_deck,
        "the tap was not on exactly one deck while audio was playing:\n  {log}"
    );
    pty.quit();
}

/// Deliberately fails, to prove the harness cleans up after a failing test. Ignored so it
/// never fails the suite; run it with `--ignored` and check no mpv survives.
#[test]
#[ignore = "fails on purpose; proves Drop cleans up"]
fn a_failing_test_leaves_no_mpv_behind() {
    require_tools!();
    let scratch = Scratch::new("leak");
    let media = scratch.join("media");
    make_tracks(&media, &["a"], 20);
    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    panic!("failing on purpose while an mpv is playing");
}

/// Against a real streaming playlist, so it needs the network and is run by hand:
/// `cargo test --release --test live real_playlist -- --ignored --nocapture`.
#[test]
#[ignore = "needs the network and a real SoundCloud playlist"]
fn real_playlist_analyses_ahead_instead_of_falling_to_a_timer() {
    require_tools!();
    let scratch = Scratch::new("realsc");
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=8\ntransition=sync_blend\n",
    );
    let mut pty = Pty::spawn(
        &["https://soundcloud.com/irvin-heslan/sets/ben-birthday"],
        &scratch.0,
        110,
        32,
    );
    assert!(
        pty.wait_for(Duration::from_secs(60), |s| s.contains("YTM://PLAYER")),
        "player never started"
    );
    // The whole point: the next track is resolved with yt-dlp, decoded over one
    // connection and surveyed - so "ready to mix" must appear, and the timer line
    // must not.
    let mut ready = false;
    let mut timered = false;
    let began = std::time::Instant::now();
    while began.elapsed() < Duration::from_secs(180) {
        pty.pump(Duration::from_millis(250));
        let screen = pty.screen.text();
        if screen.contains("ready to mix") || screen.contains("ANALYSING") {
            ready |= screen.contains("ready to mix");
        }
        if screen.contains("no steady beat") {
            timered = true;
        }
        if ready {
            break;
        }
    }
    println!(
        "REAL ready={ready} timered={timered} after {:.0}s",
        began.elapsed().as_secs_f64()
    );
    pty.quit();
    assert!(ready, "the real playlist was never analysed ahead of time");
    assert!(!timered, "the mix still fell back to a timer");
}

#[test]
fn the_desktop_can_see_and_drive_the_player() {
    require_tools!();
    if std::process::Command::new("playerctl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipping: playerctl is needed to speak to the MPRIS layer");
        return;
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        eprintln!("skipping: no session bus, so there is no desktop to be seen by");
        return;
    }
    let scratch = Scratch::new("mpris");
    let media = scratch.join("media");
    // Tagged, because the metadata the desktop shows is the thing most likely to rot:
    // it was published as the source label ("Local") for a while and nothing noticed.
    //
    // Long enough that the waits below cannot outlive the track. At twenty seconds the
    // metadata poll's own eighteen-second budget was nearly the whole of it, so a slow
    // start on a loaded machine meant the loop ran out *after* the track had ended and
    // read "Paused" - a race between the test's patience and its fixture, not a fault in
    // what it was testing.
    make_tagged_track(
        &media.join("01.mp3"),
        "Windowlicker",
        "Aphex Twin",
        "Windowlicker",
        90,
    );
    make_tagged_track(
        &media.join("02.mp3"),
        "Come to Daddy",
        "Aphex Twin",
        "Come to Daddy",
        90,
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    // The bus name is taken on a background thread; give it a moment to appear.
    let mut seen = false;
    for _ in 0..30 {
        pty.pump(Duration::from_millis(300));
        if playerctl(&["-l"]).contains("ytmplayer") {
            seen = true;
            break;
        }
    }
    assert!(seen, "playerctl cannot see the player at all");

    // Seeing the bus name is not the same as mpv having started, and status landing is
    // not the same as the metadata that goes with it having landed too - each is its own
    // property update, on its own schedule, same as everything else read off a socket.
    // Waiting on the actual answer instead of a fixed pump is what makes this safe to
    // run alongside other tests instead of needing the machine to itself.
    let mut status = String::new();
    let mut artist = String::new();
    for _ in 0..60 {
        pty.pump(Duration::from_millis(300));
        status = playerctl(&["-p", "ytmplayer", "status"]);
        artist = playerctl(&["-p", "ytmplayer", "metadata", "xesam:artist"]);
        if status == "Playing" && !artist.is_empty() {
            break;
        }
    }
    assert_eq!(status, "Playing");
    assert_eq!(
        artist, "Aphex Twin",
        "the desktop is being told the wrong artist"
    );
    assert_eq!(
        playerctl(&["-p", "ytmplayer", "metadata", "xesam:title"]),
        "Windowlicker"
    );

    // ...and it drives, not just reports.
    playerctl(&["-p", "ytmplayer", "play-pause"]);
    let mut paused_status = String::new();
    for _ in 0..30 {
        pty.pump(Duration::from_millis(300));
        paused_status = playerctl(&["-p", "ytmplayer", "status"]);
        if paused_status == "Paused" {
            break;
        }
    }
    assert_eq!(paused_status, "Paused");
    playerctl(&["-p", "ytmplayer", "play"]);
    for _ in 0..30 {
        pty.pump(Duration::from_millis(300));
        if playerctl(&["-p", "ytmplayer", "status"]) == "Playing" {
            break;
        }
    }

    playerctl(&["-p", "ytmplayer", "next"]);
    let mut next_title = String::new();
    for _ in 0..30 {
        pty.pump(Duration::from_millis(300));
        next_title = playerctl(&["-p", "ytmplayer", "metadata", "xesam:title"]);
        if next_title == "Come to Daddy" {
            break;
        }
    }
    assert_eq!(next_title, "Come to Daddy", "next did not reach the player");

    pty.quit();
    // The name must go with the process, or the next session collides with this one.
    // `pty.quit()` already waits for the process to exit, so this is really waiting on
    // the bus daemon to notice a closed connection, not on the player - a much shorter
    // and much less loaded wait than the fixed sleep this replaced.
    let mut released = false;
    for _ in 0..20 {
        if !playerctl(&["-l"]).contains("ytmplayer") {
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(released, "the bus name outlived the player");
}

/// Against a real YouTube video, so it needs the network and is run by hand:
/// `cargo test --release --test live a_saved_track -- --ignored --nocapture`.
#[test]
#[ignore = "needs the network and a real YouTube video"]
fn a_saved_track_says_downloaded_and_a_reopened_session_knows_it() {
    require_tools!();
    let scratch = Scratch::new("realdl");
    write_config(&scratch.0, "crossfade=on\nbeat_mixing=off\n");
    // Stable, short-enough-to-download-fast, and about as likely to still be there
    // next year as any video on the internet gets.
    let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";

    let mut pty = Pty::spawn(&[url], &scratch.0, 100, 30);
    assert!(
        pty.wait_for(Duration::from_secs(30), |s| s.contains("YTM://PLAYER")),
        "player never started"
    );
    // Let the stream actually open before asking to save it - `d` on a target with no
    // URL yet is a silent no-op the same as it would be on an empty session.
    pty.pump(Duration::from_secs(3));

    pty.key("d");
    assert!(
        pty.wait_for(Duration::from_secs(60), |s| s.contains("SAVED")),
        "the download never finished:\n{}",
        pty.screen.text()
    );
    // The "just saved" toast steps aside on its own after a few seconds (`MSG_LINGER`);
    // the status line underneath it should say the track is downloaded once it does,
    // and the point of this feature is that it must, not just that it eventually might.
    assert!(
        pty.wait_for(Duration::from_secs(10), |s| s.contains("DOWNLOADED")),
        "the status line never settled on DOWNLOADED:\n{}",
        pty.screen.text()
    );
    assert!(
        !pty.screen.contains("CACHED"),
        "a real download should never read as the ephemeral recorder cache"
    );
    pty.quit();

    let downloads = scratch.join("downloads");
    assert!(
        std::fs::read_dir(&downloads).is_ok_and(|mut d| d.next().is_some()),
        "nothing landed in {}",
        downloads.display()
    );

    // A fresh session against the same URL should recognise the save immediately - from
    // the on-disk index (`$XDG_STATE_HOME/ytmplayer/downloads.jsonl`), not from anything
    // the first process happened to still remember.
    let mut second = Pty::spawn(&[url], &scratch.0, 100, 30);
    assert!(second.wait_for(Duration::from_secs(30), |s| s.contains("YTM://PLAYER")));
    assert!(
        second.wait_for(Duration::from_secs(20), |s| s.contains("DOWNLOADED")),
        "a reopened session did not recognise the earlier download:\n{}",
        second.screen.text()
    );
    second.quit();
}

#[test]
fn a_beat_roll_loops_the_leaving_deck_and_always_lets_go() {
    require_tools!();
    let scratch = Scratch::new("roll");
    let media = scratch.join("media");
    make_beat_track(&media.join("01.mp3"), 128.0, 40);
    make_beat_track(&media.join("02.mp3"), 128.0, 40);
    write_config(
        &scratch.0,
        "crossfade=on\nbeat_mixing=on\ncrossfade_secs=8\ntransition=beat_roll\n",
    );

    let mut pty = Pty::spawn(&[media.to_str().expect("utf-8 path")], &scratch.0, 100, 30);
    assert!(pty.wait_for(Duration::from_secs(20), |s| s.contains("YTM://PLAYER")));
    let mut deck = Ipc::connect(&pty.deck_socket(), Duration::from_secs(5)).expect("playing deck");
    let mut spare = Ipc::connect(&pty.spare_socket(), Duration::from_secs(10)).expect("spare deck");
    deck.command(serde_json::json!(["playlist-play-index", 0]));
    pty.pump(Duration::from_secs(2));
    let duration = deck.number("duration").unwrap_or(40.0);
    deck.seek(duration - 26.0);

    // `ab-loop-a` reads "no" until something sets it, and a number once a `loop` hook
    // has. Watching the property rather than the screen is the point: this is the one
    // move that is playback jumping backwards rather than a control moving, so the
    // screen would happily say "rolling" whether or not the deck ever heard about it.
    let looped_at = |ipc: &mut Ipc| -> Option<f64> { ipc.number("ab-loop-a") };
    let mut rolled = false;
    let mut released = false;
    for _ in 0..600 {
        pty.pump(Duration::from_millis(100));
        // Either deck: which of the two is "outgoing" changes at the swap, and the roll
        // is on whichever one that is when the hook fires.
        if looped_at(&mut deck).is_some() || looped_at(&mut spare).is_some() {
            rolled = true;
        }
        // Cleared again afterwards - by the score's own `loop off`, or by the teardown
        // if the transition was abandoned. Either way a deck must never be handed on
        // still looping, or the next track plays half a bar of itself forever.
        if rolled && looped_at(&mut deck).is_none() && looped_at(&mut spare).is_none() {
            released = true;
            break;
        }
    }
    pty.quit();

    assert!(rolled, "no `loop` hook ever reached a deck");
    assert!(released, "a deck was left looping after the roll");
}
