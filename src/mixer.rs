//! The mixing engine: the crossfade lifecycle, the two-deck dance that makes it possible,
//! and the sync loop that keeps an already-playing grid and an arriving one on top of each
//! other once a transition is running.
//!
//! A real crossfade overlaps two tracks: the incoming one starts while the outgoing one is
//! still playing, and the two are ramped past each other. mpv decodes one track at a time,
//! so this takes two mpv processes - a pair of decks that trade roles at every transition.
//! Both hold the same playlist, so the deck that takes over already knows where it is in
//! the queue and carries on advancing normally.
//!
//! Split out of `main.rs` alongside `measure.rs`: this half owns `cross` (the transition in
//! flight), `spare` (the second deck), the sync loop's own state (`sync_trim`,
//! `release_lag`, `grid_taken`...) and the timing math a transition runs on. It reads what
//! `measure.rs` found - a tempo, a grid - but never resolves one itself.

use super::*;
use crate::measure::{BEATS_PER_BAR, Grid, Measured};

/// How far a grid may be used from where it was read before it is read again.
///
/// Short, because this is the error that does not announce itself: the grid is right, the
/// arithmetic is right, and the answer is wrong in proportion to how long ago the question
/// was asked. Two bars of tolerance rather than twenty.
const SYNC_STALE: f64 = 4.0;

/// How long before a track ends the beat tracker starts listening. It needs five seconds
/// of history before it will commit to anything, and costs a faster redraw while it runs,
/// so it is switched on late and left off for the rest of the song.
const BEAT_LISTEN_LEAD: f64 = 30.0;

/// How far ahead of the overlap the incoming deck is cued. A streamed entry costs a
/// yt-dlp run and a buffer fill before its first sample; opening it this early is what
/// lets the overlap itself start on time.
const CROSS_LEAD: f64 = 6.0;

/// Fold one transition's landing error into the running estimate of the deck's start lag.
///
/// Eased rather than followed: a single landing on a busy machine is one sample of a noisy
/// quantity, and an estimate that jumps to its last measurement is worse than one that
/// drifts toward all of them. Bounded at both ends too - a landing far enough out is not
/// the machine being slow, it is a grid that was read badly, and feeding that forward would
/// carry one bad measurement into every transition after it.
pub(crate) fn learn_release_lag(current: f64, slip: f64) -> f64 {
    /// How much of each new measurement to believe.
    const TRUST: f64 = 0.4;
    /// Past this a landing is a bad grid rather than a slow deck, and is ignored.
    const PLAUSIBLE: f64 = 0.12;
    /// And the estimate itself never exceeds this, whatever it is told.
    const MOST: f64 = 0.15;
    if !slip.is_finite() || slip.abs() > PLAUSIBLE {
        return current;
    }
    (current + slip * TRUST).clamp(0.0, MOST)
}

/// A crossfade in flight: two decks audible at once, one going, one coming.
#[derive(Clone, Copy)]
pub(crate) struct Crossfade {
    /// The user's volume - what j/k set and the rail shows. Both decks are scaled from
    /// it, and the incoming deck is left holding exactly it when the transition ends.
    pub(crate) base: f64,
    /// Set once both decks are audible. `None` while the incoming deck is still opening
    /// its stream, paused at zero - the overlap starts on its cue, not on a guess.
    pub(crate) started: Option<Instant>,
    /// Seconds of overlap actually played. Advanced by wall clock, but only while the
    /// player is running: a pause mid-transition freezes the ramp instead of letting it
    /// run out against silence.
    pub(crate) elapsed: f64,
    /// The swap was placed on a bar line rather than wherever the clock fell.
    pub(crate) on_the_beat: bool,
    /// The decks have already traded places; the rest of the score runs on the new one.
    pub(crate) swapped: bool,
    /// The *user* paused, so the ramp is on hold.
    ///
    /// Deliberately not `snap.paused`: the outgoing deck pauses itself at its own end -
    /// that is what `keep-open` is for - and reading that as "the user paused" freezes
    /// the ramp exactly when it is about to finish, so the swap never happens.
    pub(crate) frozen: bool,
    /// Previous tick, so `elapsed` can be advanced by the time that really passed.
    pub(crate) last: Instant,
    /// Overlap length actually used, seconds. Shortened when the outgoing track has
    /// less left than the configured time by the time the incoming one is ready.
    pub(crate) seconds: f64,
    /// Last volume written to each deck, to skip sub-1% IPC. `NAN` forces a write.
    pub(crate) out_applied: f64,
    pub(crate) in_applied: f64,
}

impl Player {
    /// Follow the crossfade setting with mpv's gapless decode, and take playlist
    /// prefetch back: with a spare deck cueing the next entry itself, a prefetch on the
    /// playing deck is a second yt-dlp run for a track it will never reach.
    pub(crate) fn sync_seamless(&mut self) {
        let on = self.settings.crossfade;
        let _ = self.mpv.set_gapless(true);
        let _ = self.mpv.set_prefetch(!on);
        if on {
            self.ensure_spare();
            self.mirror_playlist();
        } else {
            self.abort_crossfade();
            // Dropping it kills the process: crossfade off costs no second decoder.
            self.spare = None;
        }
        self.sync_playback_modes();
    }

    /// Push repeat and loudness matching onto every deck.
    ///
    /// Both decks need them: the one that is parked now is the one playing after the
    /// next transition, and a track that arrives un-levelled halfway through an overlap
    /// is exactly the volume jump the setting exists to remove.
    pub(crate) fn sync_playback_modes(&mut self) {
        let (queue, track) = match self.settings.repeat {
            settings::Repeat::Off => (false, false),
            settings::Repeat::All => (true, false),
            settings::Repeat::One => (false, true),
        };
        let gain = self.settings.normalize.mpv_value();
        let _ = self.mpv.set_repeat(queue, track);
        let _ = self.mpv.set_replaygain(gain);
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.set_repeat(queue, track);
            let _ = deck.set_replaygain(gain);
        }
    }

    /// Shuffle or unshuffle the queue on both decks.
    ///
    /// Order is the one thing the two decks must agree on exactly - the parked deck is
    /// cued by index, so a deck shuffled differently would cue the wrong track - and
    /// mpv's own shuffle is per-process and randomly seeded. So the playing deck is
    /// shuffled, its resulting order is read back, and the parked deck is rebuilt from
    /// it rather than shuffled itself.
    pub(crate) fn sync_shuffle(&mut self) {
        if self.mpv.shuffle_playlist(self.settings.shuffle).is_err() {
            return;
        }
        // Our own list keeps its order; the map between the two is what moves.
        let order = self.mpv.playlist_urls();
        self.rebuild_playlist_map(&order);
        self.mirror_playlist();
        self.selected = self.entry_index();
        self.cross_refused = false;
        self.abort_crossfade();
        self.dirty = true;
    }

    /// Bring up the second deck, parked and silent. Idempotent, and self-healing: a deck
    /// whose process died is replaced. A failure just means no crossfade this session.
    pub(crate) fn ensure_spare(&mut self) {
        if self.spare.as_mut().is_some_and(Mpv::has_exited) {
            self.spare = None;
        }
        if self.spare.is_some() || self.source.is_empty() {
            return;
        }
        let path = std::env::temp_dir().join(format!("mpvsocket_{}_b", process::id()));
        let Ok(mut deck) = Mpv::spawn(Media::Idle, &path, true) else {
            self.show_toast("⇄ crossfade needs a second mpv - not available".to_string());
            return;
        };
        // The deck can become the visible player, so its `tct` frames need the same
        // single writer the first deck's go through.
        if let Some(video) = deck.video_out.take() {
            let forwarder = self.term.clone();
            std::thread::spawn(move || forwarder.forward(video));
        }
        let _ = deck.set_pause(true);
        let _ = deck.set_volume(0.0);
        let _ = deck.set_gapless(true);
        let _ = deck.set_prefetch(false);
        self.spare = Some(deck);
        // A deck that does not know the queue cannot take it over.
        self.mirror_playlist();
    }

    /// Append to the playing deck and mirror it onto the parked one: the deck that takes
    /// over at the next transition has to be holding the same queue.
    pub(crate) fn append_to_playlist(&mut self, url: &str) {
        let _ = self.mpv.playlist_append(url);
        if let Some(deck) = self.spare.as_mut() {
            // `append`, not `append-play`: a parked deck must never start on its own.
            let _ = deck.playlist_append_quiet(url);
        }
    }

    pub(crate) fn move_in_playlist(&mut self, from: i64, to: i64) {
        let _ = self.mpv.playlist_move(from, to);
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.playlist_move(from, to);
        }
    }

    /// Give the parked deck exactly the queue the playing one has, in exactly its order.
    ///
    /// Read back from the playing deck rather than rebuilt from the playlist file: after
    /// a shuffle, an add-next or a reorder that file is no longer what mpv is holding,
    /// and the parked deck is cued *by index* - a deck that disagrees about the order
    /// cues the wrong track.
    pub(crate) fn mirror_playlist(&mut self) {
        if self.spare.is_none() {
            return;
        }
        let order = self.mpv.playlist_urls();
        let Some(deck) = self.spare.as_mut() else {
            return;
        };
        let _ = deck.set_ytdl_enabled(true);
        let _ = deck.set_ytdl_format(youtube::AUDIO_ONLY_FORMAT);
        let _ = deck.stop_keep_playlist();
        let _ = deck.playlist_replace_quiet(&order);
    }

    /// Re-derive `playlist_map` from mpv's playlist order.
    ///
    /// Shuffling permutes mpv's list and not ours, so afterwards position `n` in mpv is
    /// some other entry of ours. Entries are matched back by URL, first unused wins, so
    /// a queue holding the same track twice still maps one mpv slot to one entry.
    pub(crate) fn rebuild_playlist_map(&mut self, order: &[String]) {
        if order.is_empty() {
            self.playlist_map = None;
            return;
        }
        let mut used = vec![false; self.source.entries.len()];
        let mut map = Vec::with_capacity(order.len());
        for url in order {
            let hit = self
                .source
                .entries
                .iter()
                .enumerate()
                .position(|(i, entry)| !used[i] && entry.url.as_deref() == Some(url.as_str()));
            match hit {
                Some(i) => {
                    used[i] = true;
                    map.push(i);
                }
                // An entry mpv has and we cannot name: keep the slot so every later
                // index still lines up.
                None => map.push(0),
            }
        }
        self.playlist_map = Some(map);
    }

    /// Whether anything is queued after the current entry - the gate on starting a
    /// transition at all: the last track of a queue always plays out clean.
    pub(crate) fn has_next(&self) -> bool {
        self.source.is_playlist && self.entry_index() + 1 < self.source.entries.len()
    }

    /// Volume changes route through here so a live crossfade scales the *user's* level
    /// instead of compounding with the transition's own gains.
    pub(crate) fn nudge_volume(&mut self, delta: f64) {
        match &mut self.cross {
            Some(cross) => {
                cross.base = (cross.base + delta).clamp(0.0, 150.0);
                // Force both decks to be rewritten at the new base on the next tick.
                cross.out_applied = f64::NAN;
                cross.in_applied = f64::NAN;
            }
            None => {
                let _ = self.mpv.add_volume(delta);
            }
        }
    }

    pub(crate) fn set_user_volume(&mut self, volume: f64) {
        match &mut self.cross {
            Some(cross) => {
                cross.base = volume.clamp(0.0, 150.0);
                cross.out_applied = f64::NAN;
                cross.in_applied = f64::NAN;
            }
            None => {
                let _ = self.mpv.set_volume(volume);
            }
        }
    }

    /// Tear a transition down without completing it: the playing deck gets the user's
    /// level and its normal end-of-file behaviour back, the incoming one goes silent.
    /// Put back any effect the transition switched on.
    pub(crate) fn release_effects(&mut self) {
        if self.effects_engaged.is_empty() {
            return;
        }
        for at in std::mem::take(&mut self.effects_engaged) {
            if let Some(flag) = self.effects_on.get_mut(at) {
                *flag = false;
            }
        }
    }

    /// The same for a loop roll: whatever `HookAction::Loop` set, cleared on both decks.
    ///
    /// A score is supposed to end its own rolls with `loop off`, and every built-in one
    /// does - but a score only gets to finish if the transition does, and one that is
    /// abandoned half way (the track stopped early, the user skipped, the fade curve
    /// moved the swap in front of the hook) would otherwise leave an `ab-loop` set on a
    /// deck that is about to be parked and re-used. The next track cued onto it would
    /// play the first half-bar of itself forever. Effects are already put back for
    /// exactly this reason; a loop is the same kind of borrowed state.
    pub(crate) fn release_loops(&mut self) {
        let _ = self.mpv.set_ab_loop(None);
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.set_ab_loop(None);
        }
    }

    pub(crate) fn abort_crossfade(&mut self) {
        let Some(cross) = self.cross.take() else {
            return;
        };
        let _ = self.mpv.set_keep_open(false);
        let _ = self.mpv.set_volume(cross.base);
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.set_volume(0.0);
            let _ = deck.set_pause(true);
            let _ = deck.stop_keep_playlist();
        }
        // The transition's filters and its tempo lock go with it: a deck left holding a
        // lowpass would play the rest of the track through it, and one left at 1.03 speed
        // would play the rest of the night slightly sharp.
        self.cross_af = (None, None);
        self.reset_tempo();
        self.release_effects();
        self.release_loops();
        self.beat_waited = false;
        self.apply_af();
        self.dirty = true;
    }

    /// Drive the overlap, once per snapshot.
    ///
    /// Three steps, each on its own tick: cue the spare deck a few seconds out (paused,
    /// silent, so it can open the stream in its own time), start the ramp the moment the
    /// outgoing track is `crossfade_secs` from its end, then trade the decks over when
    /// the ramp completes.
    pub(crate) fn crossfade_tick(&mut self) {
        // The picture belongs to one deck and swapping them would tear it down mid-frame,
        // so video mode plays the seam straight.
        if !self.settings.crossfade || !self.source.is_playlist || self.video_mode {
            self.abort_crossfade();
            return;
        }
        let overlap = self.overlap_seconds();
        let Some(cross) = self.cross else {
            self.maybe_cue_crossfade(overlap);
            return;
        };

        if cross.started.is_none() {
            self.maybe_start_overlap(cross, overlap);
            return;
        }
        // A seek back out of the tail (or a track that turned out to be longer than it
        // said) means the transition was started too early - take it back.
        //
        // Only before the decks trade, though. After that `remaining` is the *arriving*
        // track's, which has a whole song left, so this read as "seeked out of the tail"
        // and abandoned every transition the instant it swapped - taking the second half
        // of the span, which is where a long blend does its last four bars, with it.
        let remaining = self.remaining();
        if !cross.swapped && remaining.is_some_and(|left| left > cross.seconds + CROSS_LEAD) {
            self.abort_crossfade();
            return;
        }
        let now = Instant::now();
        let t = {
            let Some(cross) = &mut self.cross else { return };
            let step = now.duration_since(cross.last).as_secs_f64();
            cross.last = now;
            if !cross.frozen {
                cross.elapsed += step;
            }
            cross.elapsed / cross.seconds.max(0.001)
        };
        // Bars, not just progress: a counted move ("wait four, then drop") has to be read
        // against the beat grid or it is only the right shape at the wrong tempo.
        let bar = match (self.active_transition().bars(), self.bar_seconds()) {
            (Some(bars), Some(seconds)) if seconds > 0.0 => (t * cross.seconds / seconds).min(bars),
            (Some(bars), _) => t * bars,
            (None, _) => t,
        };
        let clock = transitions::Clock { t, bar };
        // Hooks fire on the real clock, lanes are read through the curve - see `shaped`.
        // A beat roll is pinned to a bar line; a fader move is exactly what a curve is
        // for, and the two must not be shaped by the same rule.
        self.fire_hooks(clock);
        let curve = self.settings.fade_curve;
        let shape = transitions::shape(self.active_transition(), clock, curve);

        // The decks trade places when the leaving track falls silent, which is where its
        // own music runs out - not when the score finishes. Everything after that is the
        // arriving track alone, still being worked on: a blend that empties out at bar
        // twelve of sixteen spends its last four bars here, and until this split existed
        // the interface went on naming the old track throughout them.
        //
        // Against the curve's own idea of that moment, not the score's: under `Late` a
        // score that empties out half way really does it seven tenths of the way through,
        // and swapping at the written figure would cut the old deck off with its fader
        // still up - the exact fault the split exists to avoid.
        if !cross.swapped && t >= self.active_transition().out_end_at(curve) {
            self.swap_decks(cross.base);
        }
        self.apply_shape(&shape);
        if t >= 1.0 {
            self.end_crossfade();
        }
    }

    /// Play/pause. Two audible decks pause together, or the "paused" player carries on
    /// singing the incoming track.
    pub(crate) fn toggle_pause(&mut self) {
        // The snapshot is pre-toggle, so the state being asked for is its opposite.
        let pausing = !self.snap.paused;
        let _ = self.mpv.toggle_pause();
        if self.cross.is_some_and(|cross| cross.started.is_some()) {
            if let Some(deck) = self.spare.as_mut() {
                let _ = deck.set_pause(pausing);
            }
            if let Some(cross) = &mut self.cross {
                cross.frozen = pausing;
            }
        }
        self.dirty = true;
    }

    /// Decline this transition, and say so.
    ///
    /// A crossfade that quietly does not happen is indistinguishable from one that is
    /// broken - which is exactly the complaint that started this whole subsystem - so
    /// the refusal is spoken once per track rather than swallowed.
    pub(crate) fn refuse_crossfade(&mut self, why: &str) {
        if !self.cross_refused {
            self.show_toast(why.to_string());
        }
        self.cross_refused = true;
    }

    /// Whether the beat tracker should be running right now.
    ///
    /// Only in the run-up to a transition that would use it. The tracker needs five
    /// seconds of history before it will say anything and wants frames as fast as the
    /// UI can produce them, so "the last half-minute of a track" is both early enough to
    /// be ready and late enough to cost nothing for the rest of the song.
    pub(crate) fn listening_for_beats(&self) -> bool {
        if !self.settings.crossfade || !self.active_transition().wants_beat_alignment() {
            return false;
        }
        if self.video_mode || !self.source.is_playlist {
            return false;
        }
        self.remaining()
            .is_some_and(|left| left <= self.listen_lead())
    }

    /// The playing deck's position, asked for rather than remembered.
    pub(crate) fn position_of_main(&mut self) -> Option<f64> {
        self.mpv
            .get_property("time-pos")
            .and_then(|value| value.as_f64())
    }

    /// Measure how far the two grids have slipped, and lean on the arriving deck.
    ///
    /// A ratio worked out from two tempo estimates is never exactly right, and it does not
    /// need to be wrong by much: half a per cent is seventy milliseconds over a sixteen
    /// bar blend, which is a flam anyone can hear. Setting the speed once and hoping is
    /// open-loop control of a quantity that is being measured continuously, which is the
    /// wrong shape for the problem.
    ///
    /// So it is closed. Both decks report their position to the microsecond, and both
    /// grids are known from the same estimator, so the phase error is a subtraction. What
    /// goes back is a small proportional trim on the arriving deck's speed - the same
    /// correction a hand on a platter makes, and for the same reason.
    ///
    /// The gain is deliberately low. A tenth of the error per second converges in a few
    /// seconds, stays well inside the range a stretch is inaudible over, and cannot
    /// oscillate; a stiffer loop would chase the jitter in mpv's own position reporting
    /// and turn a steady pitch into a wobbling one.
    pub(crate) fn hold_the_sync(&mut self, locked: f64, loudness: f64) -> f64 {
        /// Seconds to take out a phase error. Short enough that a mix is in time well
        /// before the arriving track is loud, long enough that the pitch move it implies
        /// - well under one per cent - is nothing anyone can hear.
        const SETTLE: f64 = 1.5;
        /// The most it is allowed to lean once the arriving track can be heard. About
        /// seventeen cents, held for a second or two: less than the wobble on a record and
        /// well inside what a DJ does to a platter by hand.
        const MOST: f64 = 0.010;
        /// And what it may do while the track is still coming up out of nothing.
        ///
        /// The offset left at the release is a one-off - the cost of a seek and a pause
        /// being let go - and taking it out at the polite rate means a few seconds of
        /// converging. Those seconds are exactly the ones where the arriving track is
        /// under a bass cut and climbing from silence, so a lean nobody could hear anyway
        /// is free there and the mix is in time by the time it is loud enough to matter.
        const MOST_WHILE_QUIET: f64 = 0.035;
        /// Close enough that leaning further would be chasing the measurement.
        const DEADBAND: f64 = 0.006;
        let Some(cued) = self.sync_cued else {
            return 0.0;
        };
        // Only while the score still says the two are locked together. Once it starts
        // handing the tempo back there is nothing to hold, and holding anyway would fight
        // the lane for control of the same number.
        if locked > 0.05 {
            self.sync_trim *= 0.5;
            return self.sync_trim;
        }
        let Some(playing) = self.playing_grid() else {
            return 0.0;
        };
        // Nothing to hold together unless the arriving track was measured too - without
        // it there is no ratio, so there is no error to correct either.
        if self
            .next_entry_index()
            .and_then(|next| self.measured(next))
            .is_none()
        {
            return 0.0;
        }
        // Both positions read here and now, and the playing one read twice so the other
        // can be placed between them. The snapshot the interface draws from is refreshed
        // on the redraw tick, so it can be a couple of hundred milliseconds old - and
        // subtracting a stale position from a fresh one produces exactly the same number
        // as real slip. Fed to a loop whose job is to drive that number to zero, it does
        // not read the error, it *becomes* the error: the decks get held apart by however
        // long the snapshot happened to be behind.
        let before = self.position_of_main();
        let there = self
            .spare
            .as_mut()
            .and_then(|deck| deck.get_property("time-pos"))
            .and_then(|value| value.as_f64());
        let after = self.position_of_main();
        let (Some(before), Some(there), Some(after)) = (before, there, after) else {
            return 0.0;
        };
        // Three round trips over a unix socket, so the skew is normally well under a
        // millisecond - but if the player is wedged or swapping, throw the sample away
        // rather than act on it.
        if after - before > 0.05 || after < before {
            return self.sync_trim;
        }
        let here = f64::midpoint(before, after);
        let Some(grid) = Grid::of(playing) else {
            return 0.0;
        };
        let beat = grid.bar / f64::from(BEATS_PER_BAR);
        // Both grids read fresh, and both compared as a fraction of their own beat rather
        // than in seconds. The arriving deck is being played at a ratio, so its beats in
        // file time are not the length of the playing deck's; what has to line up is where
        // each track is *within* a beat, and that is a fraction. Turning the difference
        // back into seconds at the end uses the playing track's beat, because that is the
        // clock the listener is on.
        let arriving = self
            .next_entry_index()
            .and_then(|next| self.measured(next))
            .and_then(Grid::of);
        let theirs = match arriving {
            Some(theirs) => (there - theirs.first) / (theirs.bar / f64::from(BEATS_PER_BAR)),
            // Nothing fresher than where it was parked, which was a bar line of its own.
            None => (there - cued) / beat,
        };
        let ours = (here - grid.first) / beat;
        let mut slip = (ours.rem_euclid(1.0) - theirs.rem_euclid(1.0)) * beat;
        if slip > beat / 2.0 {
            slip -= beat;
        } else if slip < -beat / 2.0 {
            slip += beat;
        }
        // Past a quarter of a beat this is not slip any more, it is a misalignment, and
        // the two are not the same problem. A loop that leans on the pitch can pull out
        // tens of milliseconds without anyone hearing it; it cannot pull out a hundred and
        // fifty, and trying means winding the correction to its limit and holding it
        // there. Worse, near half a beat the sign is arbitrary - the nearer grid line
        // changes from one sample to the next - so a loop that acts on it thrashes. Hold
        // what is already applied and leave it alone.
        if slip.abs() > beat / 4.0 {
            return self.sync_trim;
        }
        // A grid has a shelf life. It is a bar line and a spacing, so using it away from
        // where it was read means multiplying the spacing's error by the bars in between,
        // and a sixteen bar blend is long enough for a tenth of a per cent to become tens
        // of milliseconds by the end of it. Reading it again costs a third of a second of
        // ffmpeg on a thread nobody is waiting on, which is nothing, so read it again.
        if self
            .grid_taken
            .is_none_or(|when| here - when > SYNC_STALE || here < when)
        {
            self.measure_playing_grid();
            // And the arriving one, from where *it* is. It was measured from its opening
            // at the cue, which was right at the time and is a dozen bars out of date by
            // the middle of a long blend; leaving it stale while refreshing the other
            // only moves the error from one side of the subtraction to the other.
            if let Some(next) = self.next_entry_index() {
                self.refresh_grid(next, there);
            }
        }
        // The first honest look at how far out the release landed, which is the machine's
        // own lag and nothing to do with this pair of tracks. Taken once, before the loop
        // has begun pulling it in, and only ever moved half way - a single transition on a
        // busy machine is one sample of a noisy quantity, and a estimate that chases its
        // last measurement is worse than one that eases toward them all.
        if !self.lag_learned {
            self.lag_learned = true;
            self.release_lag = learn_release_lag(self.release_lag, slip);
        }
        // A deadband, so the loop stops once it is right.
        //
        // The ratio comes from two tempo estimates that share their bias, so when it is
        // applied the two decks already run together and need no help at all. What they do
        // need is the initial offset taken out. Past that point every further correction
        // is answering the noise in a grid rather than a fault in the mix, and a loop that
        // keeps leaning to hold a track against a slightly wrong idea of where its beats
        // are will drag it off the beats it actually has.
        if slip.abs() < DEADBAND {
            self.sync_trim = 0.0;
            return 0.0;
        }
        // Positive slip means the arriving deck is behind, so it needs to go faster.
        //
        // Proportional, not accumulating. What is being controlled is a phase, and speed
        // is its rate of change, so the plant is already an integrator: asking for a speed
        // proportional to the error closes it exponentially, with `SETTLE` as the time
        // constant, and cannot wind up. An integrating controller on top of an integrating
        // plant is two of them in series, which is how a correction ends up pinned to its
        // limit while the error it is answering is eleven milliseconds.
        let most = if loudness < 0.5 {
            MOST_WHILE_QUIET
        } else {
            MOST
        };
        self.sync_trim = (slip / SETTLE).clamp(-most, most);
        self.sync_trim
    }

    /// Put the arriving deck on one of its own bar lines, at the cue.
    ///
    /// Done here rather than at the moment it is released because an exact seek is not
    /// free: mpv has to decode to land on the sample asked for, and that is tens of
    /// milliseconds of work. Spent at the cue it is invisible - the deck is paused and
    /// silent, and has seconds to spare. Spent at the release it is spent precisely as the
    /// bar line goes past, which is the one moment in the whole transition where a delay
    /// turns into the thing being avoided.
    pub(crate) fn cue_sync_seek(&mut self) {
        if !self.active_transition().wants_tempo_match() {
            return;
        }
        let Some(offset) = self.sync_offset() else {
            return;
        };
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.seek_absolute(offset);
        }
        self.sync_cued = Some(offset);
    }

    /// Where to start the arriving track so it opens on one of its own bar lines.
    ///
    /// `None` when nothing was measured, or when the first bar line is far enough in that
    /// seeking to it would skip real music rather than a lead-in. Getting this wrong is
    /// worse than not doing it: a mix that begins eight seconds into the next song is not
    /// a mix, it is a mistake.
    pub(crate) fn sync_offset(&mut self) -> Option<f64> {
        const MOST: f64 = 6.0;
        let entry = self.next_entry_index()?;
        let measured = self.measured(entry)?;
        let downbeat = measured.downbeat?;
        (downbeat > 0.02 && downbeat <= MOST).then_some(downbeat)
    }

    /// How long to wait, from now, so that the swap lands on a bar line.
    ///
    /// `None` when there is no trustworthy grid, which is the common case on a heavily
    /// limited master - the automix then transitions on its timer, as it always did.
    /// The caller decides whether there is room to wait; this only says how long.
    pub(crate) fn beat_delay(&mut self, overlap: f64) -> Option<f64> {
        if !self.active_transition().wants_beat_alignment() {
            return None;
        }
        // The live position, not the drawn one. A wait is the distance from *now* to the
        // bar line, so computing it from a snapshot taken a redraw ago makes it longer by
        // however old the snapshot is, and the release lands exactly that late. It was
        // costing about a hundred and fifty milliseconds, which is a third of a beat.
        let position = match self.sync_grid_of_playing() {
            Some(_) => self.position_of_main().or(self.snap.position)?,
            None => self.snap.position?,
        };
        // The instant both beat-aligned styles actually swap on is the middle of the
        // window, not its start - so that is the moment to put on the bar line.
        let aim = position + overlap / 2.0;
        // When there is a sync to hold, the bar line has to be the one the *offline* grid
        // says, because that is the grid the arriving deck was parked on and the grid the
        // correction loop measures against. The live tracker's answer is a good one and it
        // is what every other transition waits for, but it is a different answer: it
        // listens through the tap at whatever irregular rate the interface redraws, so its
        // idea of where the bar falls sits a little off the offline one. Releasing on one
        // grid having seeked to the other put the two decks half a beat apart, which is
        // the worst distance there is - far enough to hear, and too far for a loop that
        // corrects by leaning on the pitch to pull back.
        if let Some(grid) = self.sync_grid_of_playing() {
            // For a sync it is the *release* that goes on the bar line, not the swap in
            // the middle. From the instant the arriving deck is let go the two grids have
            // to coincide, because that is when both are audible and that is what the
            // correction loop is measuring; a score whose overlap is a whole number of
            // bars - which is every score that asks for a sync - puts the swap on a line
            // as a consequence.
            return Some(grid.next_bar_after(position) - position);
        }
        let _ = aim;
        self.beats
            .next_downbeat_in(position + overlap / 2.0, BEATS_PER_BAR)
    }

    /// The offline grid of the track now playing, when one was measured and a sync wants it.
    pub(crate) fn sync_grid_of_playing(&self) -> Option<Grid> {
        if !self.active_transition().wants_tempo_match() {
            return None;
        }
        let measured = self.playing_grid()?;
        Grid::of(measured)
    }

    /// Work out how much to slow or speed the arriving track so it runs with this one.
    ///
    /// Returns the ratio to apply to the incoming deck: `1.0` when there is nothing to match
    /// against, which is the honest answer far more often than not. A ratio is only used
    /// when both tempos are known and the stretch is small - beyond a few per cent it stops
    /// sounding like a mix and starts sounding like a fault, and no amount of pitch
    /// correction hides it.
    pub(crate) fn tempo_ratio_for(&mut self, entry: usize) -> f64 {
        if !self.active_transition().wants_tempo_match() {
            return 1.0;
        }
        let Some(playing) = self.playing_grid().map(|m| m.bpm) else {
            return 1.0;
        };
        let Some(arriving) = self.incoming_bpm(entry) else {
            return 1.0;
        };
        // Match to the nearest octave of the arriving tempo, or a 174 BPM track pulled to
        // 87 would be played at half speed rather than matched.
        let mut best = arriving;
        for factor in [0.25, 0.5, 1.0, 2.0, 4.0] {
            if (arriving * factor - playing).abs() < (best - playing).abs() {
                best = arriving * factor;
            }
        }
        let ratio = playing / best;
        // A few per cent is a beat-match; more is a fault with a name.
        if (0.94..=1.06).contains(&ratio) {
            ratio
        } else {
            1.0
        }
    }

    /// What the track now playing was measured to be, offline.
    ///
    /// Not the live tap's answer, though that exists and is what drives the visualiser and
    /// the bar counting. This one is measured the same way as the arriving track's, which
    /// is the only thing that makes their quotient trustworthy.
    pub(crate) fn playing_grid(&self) -> Option<Measured> {
        self.measured(self.entry_index())
    }

    /// The transition actually in force.
    ///
    /// Not always the one chosen in settings: with the beat-aware machinery off, every
    /// mix is the plain fade, whatever score is selected. Routed through one accessor so
    /// there is no path where half the mix is scored and the other half is not.
    pub(crate) fn active_transition(&self) -> transitions::Style {
        if self.beat_mixing() {
            self.settings.transition
        } else {
            transitions::Style::plain()
        }
    }

    /// Whether the scored, beat-aware machinery runs at all.
    ///
    /// Off, the player still crossfades - a clean equal-power fade of the length set, with
    /// no analysis and no reading ahead. That is not a degraded mode, it is the one most
    /// listening wants, and it is what keeps video working: measuring the next track means
    /// decoding it while this one plays, which is a different job from showing a picture.
    ///
    /// So the two are exclusive, and video wins when both are asked for. A user watching
    /// something has said what they want more clearly than a setting left on from last week.
    pub(crate) fn beat_mixing(&self) -> bool {
        self.settings.beat_mixing && !self.video_mode
    }

    /// Put both decks back to their own tempo and forget the ratio.
    pub(crate) fn reset_tempo(&mut self) {
        self.tempo_ratio = 1.0;
        self.tempo_applied = (1.0, 1.0);
        // Forget what was measured, not the cheaper tempo/frames/media caches underneath
        // it - a fresh probe re-settles those for free from what is still here.
        for (_, facts) in &mut self.facts {
            facts.landed = false;
            facts.measured = None;
        }
        let _ = self.mpv.set_speed(1.0);
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.set_speed(1.0);
        }
    }

    /// How long this transition should run for.
    ///
    /// A counted score says how many bars it is, and bars are seconds only once there is a
    /// tempo - sixteen of them are thirty seconds at 128 BPM and twenty-two at 174. Without
    /// a grid it falls back to the configured time, which runs the same moves in the same
    /// order too fast, which is worse than right and much better than nothing.
    pub(crate) fn overlap_seconds(&self) -> f64 {
        if !self.beat_mixing() {
            return f64::from(self.settings.crossfade_secs);
        }
        match (self.active_transition().bars(), self.bar_seconds()) {
            (Some(bars), Some(seconds)) => bars * seconds,
            _ => f64::from(self.settings.crossfade_secs),
        }
    }

    /// How long before the end of a track to start listening for its tempo.
    ///
    /// Long enough that the grid exists before the transition needs it, which for a counted
    /// score means before a thirty-second move plus its cue. A bar is about two seconds at
    /// the tempos this is used at, so the count is turned into a generous guess rather than
    /// waiting for the grid it is supposed to precede.
    pub(crate) fn listen_lead(&self) -> f64 {
        match self.active_transition().bars() {
            Some(bars) => bars * 2.5 + 20.0,
            None => BEAT_LISTEN_LEAD,
        }
    }

    /// One bar, in seconds, when the grid is trustworthy enough to say.
    pub(crate) fn bar_seconds(&self) -> Option<f64> {
        // The offline grid first, when a sync is holding one. Not because it is more
        // accurate - the live tap is good, and it is what every other transition counts
        // bars against - but because during a sync it is the grid the arriving deck was
        // parked on and the grid the correction loop measures against, and counting bars
        // against a *fourth* opinion is how a transition ends up half a beat out with
        // every part of it individually correct.
        if let Some(grid) = self.sync_grid_of_playing() {
            return Some(grid.bar);
        }
        self.beats
            .grid()
            .filter(|grid| grid.confidence >= 0.5)
            .map(|grid| grid.beat_seconds * f64::from(BEATS_PER_BAR))
    }

    /// Seconds left of the playing track, when both ends of that sum are known.
    pub(crate) fn remaining(&self) -> Option<f64> {
        match (self.snap.position, self.snap.duration) {
            (Some(pos), Some(dur)) if dur > 0.0 => Some(dur - pos),
            _ => None,
        }
    }

    /// Cue the spare deck on the next entry, paused at zero, a little before the overlap
    /// is due. Opening a stream costs a yt-dlp run and a buffer fill; doing it early is
    /// what makes the overlap itself start on time.
    pub(crate) fn maybe_cue_crossfade(&mut self, overlap: f64) {
        if self.cross_refused || self.snap.paused || !self.has_next() {
            return;
        }
        if !effects::long_enough_to_cross(self.snap.duration, overlap) {
            return;
        }
        let Some(remaining) = self.remaining() else {
            return;
        };
        if remaining > overlap + CROSS_LEAD {
            return;
        }
        let (Some(base), Some(next)) = (self.snap.volume, self.next_entry_index()) else {
            return;
        };
        let Some(index) = self.mpv_index_of(next) else {
            self.refuse_crossfade("⇄ no overlap: the next track never resolved");
            return;
        };
        self.ensure_spare();
        if self.spare.is_none() {
            self.refuse_crossfade("⇄ no overlap: the second decoder did not start");
            return;
        }
        let Some(deck) = self.spare.as_mut() else {
            return;
        };
        // Effects belong to both decks so the incoming track sounds like the outgoing
        // one; the visualizer tap does not - two writers would interleave garbage into
        // its FIFOs. The tap moves across with the swap.
        let _ = deck.set_af(effects::chain(&self.effects_on).as_deref());
        let _ = deck.set_keep_open(false);
        let _ = deck.set_pause(true);
        let _ = deck.set_volume(0.0);
        if deck.playlist_play_index(index).is_err() {
            self.refuse_crossfade("⇄ no overlap: the next track would not cue");
            return;
        }
        // Measuring the arriving track means decoding a slice of it, so it starts here at
        // the cue - seconds before anything needs the answer - and lands on another thread.
        // Both of them, through the same estimator. See `probe_tempo`: the ratio is a
        // quotient, so what matters is not how accurate each number is but that the two
        // are wrong in the same direction.
        // The arriving track from its opening, because that is where it will start. The
        // playing one from where it is now, because that is where its grid has to be
        // right - see `sample_from`.
        // Only when the scored machinery is going to use it. With beat mixing off this is
        // three ffmpeg runs and a decode per track for a number nothing will read, which is
        // exactly the cost the setting exists to avoid.
        if self.beat_mixing() {
            // An early attempt that came back empty gets one more chance here, with the
            // network given a second bite - a blip at the start of the last track is not
            // a fact about this one. Everything else already learned (tempo, frames,
            // media) stays; only the landed flag resets, so the retry still reuses it.
            if let Some((_, facts)) = self.facts.iter_mut().find(|(entry, _)| *entry == next)
                && facts.measured.is_none()
            {
                facts.landed = false;
            }
            self.probe_tempo(next, 0.0);
            self.measure_playing_grid();
        }
        self.tempo_ratio = self.tempo_ratio_for(next);
        self.tempo_applied = (1.0, 1.0);
        if (self.tempo_ratio - 1.0).abs() > 0.001
            && let Some(deck) = self.spare.as_mut()
        {
            let _ = deck.set_speed(self.tempo_ratio);
        }
        // No toast for the pull. The status line reports the whole measurement - what the
        // arriving track was found to be and what was done about it - for as long as the
        // run-up lasts, and a toast saying half of that would sit on top of the line
        // saying all of it. Which it did: the toast is drawn over the status area, so
        // announcing the match was what hid the announcement of the match.
        self.grid_refreshed = false;
        self.lag_learned = false;
        self.cross = Some(Crossfade {
            base,
            started: None,
            elapsed: 0.0,
            on_the_beat: false,
            swapped: false,
            frozen: false,
            last: Instant::now(),
            seconds: overlap,
            out_applied: f64::NAN,
            in_applied: f64::NAN,
        });
    }

    /// Let the overlap go once the incoming deck has actually opened its stream and the
    /// outgoing track has reached its last `overlap` seconds - whichever is later.
    pub(crate) fn maybe_start_overlap(&mut self, mut cross: Crossfade, overlap: f64) {
        // A paused player is not approaching anything; the cue waits for it.
        if self.snap.paused {
            return;
        }
        let Some(remaining) = self.remaining() else {
            return;
        };
        // A beat-aligned style may start up to a bar early, because that is the only way
        // there is room to wait for a bar line: by the time the plain window opens, at
        // `remaining == overlap`, waiting would push the transition past the end.
        let bar = self.bar_seconds().unwrap_or(0.0);
        let aligning = self.active_transition().wants_beat_alignment() && bar > 0.0;
        // The score's own zero has to land on the end of the track, so what is being
        // waited for is that part of the span, not all of it - and under the curve, the
        // real fraction of the span rather than the written one.
        let lead = overlap
            * self
                .active_transition()
                .out_end_at(self.settings.fade_curve);
        // With the start close but not here, measure the playing track's grid again. This
        // has to happen before the "not yet" below, or it happens at the moment it was
        // meant to have already finished.
        if !self.grid_refreshed && remaining <= lead + 10.0 {
            self.grid_refreshed = true;
            self.measure_playing_grid();
        }
        if remaining > lead + if aligning { bar } else { 0.0 } {
            return;
        }
        let ready = self
            .spare
            .as_mut()
            .is_some_and(|deck| deck.get_property("duration").is_some());
        if !ready {
            // Still opening. The outgoing track keeps playing at full level; if it runs
            // out first the transition simply did not happen, which is a plain cut and
            // not a hole.
            if remaining <= 0.0 {
                self.abort_crossfade();
            }
            return;
        }
        // With the start close, measure the playing track's grid again - see
        // `measure_playing_grid` for why the one taken at the cue is not good enough.
        if !self.grid_refreshed && remaining <= lead + 8.0 {
            self.grid_refreshed = true;
            self.measure_playing_grid();
        }
        // Styles that swap rather than blend want that swap on a bar line, so hold the
        // start until one is due. Never past the end of the track: a transition that
        // would be truncated to catch a bar is worse than one slightly off it, so in
        // that case the wait is abandoned and the timer carries it.
        let mut on_the_beat = false;
        if aligning && let Some(wait) = self.beat_delay(overlap) {
            // Held to half a poll interval rather than the older eightieth of a second:
            // whatever is left when the wait is given up is a straight offset between the
            // two grids, and the correction loop has to lean on the pitch to take it out.
            // Twenty-five milliseconds it can absorb in a couple of seconds without anyone
            // hearing; eighty takes long enough to be part of the mix.
            if wait + overlap <= remaining && wait > 0.025 {
                if !self.beat_waited {
                    self.beat_waited = true;
                    self.show_toast(format!("⇄ holding {wait:.1}s for the bar"));
                }
                return;
            }
            // Either we are on the line already, or there is no longer room to wait.
            on_the_beat = wait <= 0.025;
        }
        // The outgoing deck must not advance into the track the incoming one is already
        // playing - it pauses at its own end instead, and is stopped at the swap.
        let _ = self.mpv.set_keep_open(true);
        // Match the phase by choosing where the arriving track starts, rather than by
        // trusting the release to land on the line.
        //
        // Waiting for a bar line cannot be made exact: the interface ticks in tens of
        // milliseconds and a bar is nearly two seconds, so the window where the wait has
        // just run out is usually stepped straight over, and the fallback then lets go at
        // whatever phase the tick happened to fall on. Half a beat out, most times.
        //
        // But the phase does not have to be caught, it can be *chosen*. Read where the
        // playing track is in its bar at the moment of release, and start the arriving one
        // the same distance into its own. From that instant the two are counting together
        // whatever time it happens to be - which is what dropping a record on the right
        // spot has always been, rather than waiting for the right moment to press play.
        if let (Some(grid), Some(cued), Some(here)) = (
            self.sync_grid_of_playing(),
            self.sync_cued,
            self.position_of_main(),
        ) {
            let into_bar = (here - grid.first).rem_euclid(grid.bar);
            // Plus however far the deck is expected to overshoot getting going. See
            // `release_lag`: this is the difference between landing in time and taking a
            // few seconds to get there.
            let start = cued + into_bar + self.release_lag;
            if let Some(deck) = self.spare.as_mut() {
                let _ = deck.seek_absolute(start.max(0.0));
            }
        }
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.set_pause(false);
        }
        // Only as much of it as the track has left, though: what has to fit is the part
        // before the join, so the whole span is scaled by that rather than clipped -
        // again by where the curve really puts the join, not where the score writes it.
        let out_end = self
            .active_transition()
            .out_end_at(self.settings.fade_curve);
        cross.seconds = overlap.min(remaining / out_end).max(0.5);
        cross.started = Some(Instant::now());
        cross.elapsed = 0.0;
        cross.last = Instant::now();
        cross.on_the_beat = on_the_beat;
        self.hooks_fired = 0;
        self.cross = Some(cross);
        self.dirty = true;
    }

    /// Fire any hook the transition has just passed.
    ///
    /// Lanes cover what a mixer's controls do; a hook covers everything else, so a score
    /// can express a whole move rather than only the part of it that is a fader. Each
    /// fires once, on the way past, and a hook whose moment has already gone when the
    /// transition starts is skipped rather than fired late.
    pub(crate) fn fire_hooks(&mut self, clock: transitions::Clock) {
        let hooks = self.active_transition().hooks();
        if hooks.is_empty() {
            return;
        }
        for (index, hook) in hooks.iter().enumerate().take(64) {
            let bit = 1u64 << index;
            if self.hooks_fired & bit != 0 {
                continue;
            }
            let due = match hook.at {
                transitions::At::Bar(bar) => clock.bar >= bar,
                transitions::At::Frac(frac) => clock.t >= frac,
            };
            if !due {
                continue;
            }
            self.hooks_fired |= bit;
            match &hook.action {
                transitions::HookAction::Effect { name, on } => {
                    let wanted = name.clone();
                    let on = *on;
                    match effects::ALL
                        .iter()
                        .position(|effect| effect.name.eq_ignore_ascii_case(&wanted))
                    {
                        Some(at) => {
                            if self.effects_on.get(at).copied() != Some(on) {
                                if let Some(flag) = self.effects_on.get_mut(at) {
                                    *flag = on;
                                }
                                // Remember what the score switched on, so it can be put
                                // back whether or not the score gets to finish.
                                self.effects_engaged.retain(|held| *held != at);
                                if on {
                                    self.effects_engaged.push(at);
                                }
                                self.apply_af();
                                // The transition owns the deck's chain while it runs, so
                                // the change has to go through the shape rather than
                                // around it.
                                self.cross_af = (None, None);
                            }
                        }
                        None => self.show_toast(format!("⇄ no effect called {wanted}")),
                    }
                }
                transitions::HookAction::Toast(text) => {
                    let text = text.clone();
                    self.show_toast(text);
                }
                transitions::HookAction::Loop { deck, bars } => {
                    let (deck, bars) = (*deck, *bars);
                    // The bar length is only needed to start a roll - `loop off` clears
                    // properties mpv already has, and asks nothing of the beat tracker.
                    let bar = bars.and_then(|_| self.bar_seconds());
                    let targets: Vec<transitions::Deck> = match deck {
                        Some(one) => vec![one],
                        None => vec![transitions::Deck::Outgoing, transitions::Deck::Incoming],
                    };
                    for target in targets {
                        let Some(handle) = self.deck_handle(target) else {
                            continue;
                        };
                        match bars {
                            None => {
                                let _ = handle.set_ab_loop(None);
                            }
                            Some(fraction) => {
                                let now = handle.get_property("time-pos").and_then(|v| v.as_f64());
                                if let (Some(now), Some(bar)) = (now, bar) {
                                    let span = (bar * fraction).max(0.05);
                                    let _ = handle.set_ab_loop(Some(((now - span).max(0.0), now)));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Which mpv instance a hook's deck maps onto right now - `None` once that deck has
    /// finished its part of the transition, the same rule [`Self::apply_shape`] uses to
    /// drop a lane once its deck has nothing left to say.
    pub(crate) fn deck_handle(&mut self, deck: transitions::Deck) -> Option<&mut Mpv> {
        let swapped = self.cross.is_some_and(|cross| cross.swapped);
        match (deck, swapped) {
            (transitions::Deck::Outgoing, true) => None,
            (transitions::Deck::Outgoing, false) => Some(&mut self.mpv),
            (transitions::Deck::Incoming, true) => Some(&mut self.mpv),
            (transitions::Deck::Incoming, false) => self.spare.as_mut(),
        }
    }

    /// Apply one instant of the transition to both decks.
    ///
    /// Gains are quantised to 1% so a slow ramp costs little IPC. Filter chains are
    /// re-sent only when the string actually changes - [`transitions`] returns a stable
    /// handful across a whole transition precisely so that this can be a comparison
    /// rather than a rebuild of mpv's filter graph thirty times a second.
    pub(crate) fn apply_shape(&mut self, shape: &transitions::Shape) {
        // Worked out once per tick and handed to whichever half of this runs, so the loop
        // is closed at the tick rate rather than twice or not at all.
        self.pending_trim = self.hold_the_sync(shape.incoming_tempo, shape.incoming_gain);
        let Some(cross) = self.cross else { return };
        // Once the decks have traded, `self.mpv` *is* the arriving track: the score's
        // incoming lanes belong to it, and its outgoing lanes belong to a deck that is
        // stopped and silent, so they are simply dropped.
        if cross.swapped {
            self.apply_to_survivor(shape, cross.base);
            return;
        }
        let out_target = cross.base * shape.outgoing_gain;
        let in_target = cross.base * shape.incoming_gain;

        if cross.out_applied.is_nan() || (out_target - cross.out_applied).abs() >= 1.0 {
            let _ = self.mpv.set_volume(out_target);
            if let Some(cross) = &mut self.cross {
                cross.out_applied = out_target;
            }
        }
        if cross.in_applied.is_nan() || (in_target - cross.in_applied).abs() >= 1.0 {
            if let Some(deck) = self.spare.as_mut() {
                let _ = deck.set_volume(in_target);
            }
            if let Some(cross) = &mut self.cross {
                cross.in_applied = in_target;
            }
        }

        // Tempo, expressed by the score as "how much of your own speed" so a recipe can be
        // written before anyone knows which two tracks it will join.
        // A brake is `speed` on top of whatever the tempo match asked for, and it drags
        // the pitch with it - that is the sound, so pitch correction goes off for it.
        let out_speed = shape.outgoing_speed;
        let trim = self.pending_trim;
        let in_speed = (1.0 + (self.tempo_ratio - 1.0) * (1.0 - shape.incoming_tempo))
            * shape.incoming_speed
            * (1.0 + trim);
        if (out_speed - self.tempo_applied.0).abs() >= 0.002 {
            self.tempo_applied.0 = out_speed;
            let _ = self.mpv.set_rate(out_speed, (out_speed - 1.0).abs() < 1e-9);
        }
        if (in_speed - self.tempo_applied.1).abs() >= 0.002 {
            self.tempo_applied.1 = in_speed;
            if let Some(deck) = self.spare.as_mut() {
                let _ = deck.set_rate(in_speed, (shape.incoming_speed - 1.0).abs() < 1e-9);
            }
        }

        if self.cross_af.0 != shape.outgoing_filter {
            self.cross_af.0.clone_from(&shape.outgoing_filter);
            // The playing deck keeps the visualizer tap through the transition, or the
            // scope goes blank exactly when there is most to look at.
            let af = self.af_chain(shape.outgoing_filter.as_deref(), true);
            let _ = self.mpv.set_af(af.as_deref());
        }
        if self.cross_af.1 != shape.incoming_filter {
            self.cross_af.1.clone_from(&shape.incoming_filter);
            let af = self.af_chain(shape.incoming_filter.as_deref(), false);
            if let Some(deck) = self.spare.as_mut() {
                let _ = deck.set_af(af.as_deref());
            }
        }
    }

    /// The tail of a score, after the decks have traded and only one is playing.
    ///
    /// The arriving track keeps being worked on here - a bass that has not dropped yet, a
    /// filter still opening, a tempo returning to its own - which is the whole reason a
    /// transition may be longer than the overlap that produced it.
    pub(crate) fn apply_to_survivor(&mut self, shape: &transitions::Shape, base: f64) {
        let target = base * shape.incoming_gain;
        if self.cross.is_some_and(|cross| {
            cross.in_applied.is_nan() || (target - cross.in_applied).abs() >= 1.0
        }) {
            let _ = self.mpv.set_volume(target);
            if let Some(cross) = &mut self.cross {
                cross.in_applied = target;
            }
        }
        let trim = self.pending_trim;
        let speed = (1.0 + (self.tempo_ratio - 1.0) * (1.0 - shape.incoming_tempo))
            * shape.incoming_speed
            * (1.0 + trim);
        if (speed - self.tempo_applied.1).abs() >= 0.002 {
            self.tempo_applied.1 = speed;
            let _ = self
                .mpv
                .set_rate(speed, (shape.incoming_speed - 1.0).abs() < 1e-9);
        }
        if self.cross_af.1 != shape.incoming_filter {
            self.cross_af.1.clone_from(&shape.incoming_filter);
            // It is the playing deck now, so it carries the visualizer tap.
            let af = self.af_chain(shape.incoming_filter.as_deref(), true);
            let _ = self.mpv.set_af(af.as_deref());
        }
    }

    /// One deck's whole `af`: the user's effects, then the transition's filter, then the
    /// tap on whichever deck is being measured.
    ///
    /// Order matters. Effects first so the scope shows what the ears get; the transition
    /// after them because it is shaping the finished sound of that deck; the tap last so
    /// it measures everything. Each part is a self-contained `lavfi=[…]` node, so a comma
    /// is a valid join.
    pub(crate) fn af_chain(&self, transition: Option<&str>, with_tap: bool) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        // The same tone-then-effects order playback uses, so switching a transition on
        // does not quietly re-order the chain underneath the listener.
        if let Some(shaped) = self.shaping_chain() {
            parts.push(shaped);
        }
        if let Some(filter) = transition {
            parts.push(filter.to_string());
        }
        if with_tap && let Some(tap) = &self.tap {
            parts.push(tap.graph());
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }

    /// The leaving track has run out: the decks trade places.
    ///
    /// The incoming one is already playing the current track and knows its own place in the
    /// queue, so it becomes the player; the outgoing one is stopped and parked for the next
    /// transition, keeping its playlist so cueing it again costs one command. The score
    /// carries on running - its remaining lanes now land on the deck that just took over.
    pub(crate) fn swap_decks(&mut self, base: f64) {
        let Some(incoming) = self.spare.take() else {
            return;
        };
        // Whatever the outgoing deck was capturing belongs to the track it just finished.
        if self.recorder.is_recording() {
            self.recorder.discard(&mut self.mpv);
        }
        let mut outgoing = std::mem::replace(&mut self.mpv, incoming);

        // Order matters: the outgoing deck is sitting paused at its own EOF because of
        // `keep-open`. Clearing that first would release it into the next entry - the one
        // now playing on the other deck - for however many milliseconds the rest of this
        // takes. Silence it, stop it, and only then give it its behaviour back.
        let _ = outgoing.set_volume(0.0);
        let _ = outgoing.stop_keep_playlist();
        let _ = outgoing.set_keep_open(false);
        let _ = outgoing.set_pause(true);
        let _ = outgoing.set_af(None);
        let _ = outgoing.set_speed(1.0);
        let _ = outgoing.set_stream_record(None);
        self.spare = Some(outgoing);

        // Everything that rode the old deck moves to the new one. The filter it is
        // carrying is the score's, and the score is still running, so the chain is
        // forgotten rather than cleared - the next tick reapplies whatever is current.
        self.cross_af = (None, None);
        self.tempo_applied = (1.0, 1.0);
        let _ = self.mpv.set_volume(base);
        let _ = self.mpv.set_gapless(true);
        let _ = self.mpv.set_prefetch(false);
        self.video_url = None;
        self.video_track_loaded = false;
        self.video_track_id = None;
        self.video_unavailable = false;
        self.cross_refused = false;
        if let Some(cross) = &mut self.cross {
            cross.swapped = true;
        }
        self.snap = Snapshot::read(&mut self.mpv);
        self.attach_recorder();
        self.selected = self.entry_index();
        self.dirty = true;
    }

    /// The score has finished. Hand the surviving deck back, clean.
    pub(crate) fn end_crossfade(&mut self) {
        let Some(cross) = self.cross.take() else {
            return;
        };
        // A transition that ended before the decks traded - a track that stopped early,
        // say - still has to leave one deck playing and one parked.
        if !cross.swapped {
            self.swap_decks(cross.base);
            self.cross = None;
        }
        let _ = self.mpv.set_volume(cross.base);
        self.cross_af = (None, None);
        self.beat_waited = false;
        self.hooks_fired = 0;
        self.reset_tempo();
        self.release_effects();
        self.release_loops();
        self.apply_af();
        // A new track is a new tempo; the old grid would put bar lines in the wrong place.
        self.beats.reset();
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_lag_eases_toward_what_it_measures_and_ignores_nonsense() {
        // Eased, not followed: one landing is one noisy sample.
        let once = learn_release_lag(0.0, 0.050);
        assert!(once > 0.0 && once < 0.050, "jumped straight to it: {once}");

        // Repeated agreement converges on it.
        let mut lag = 0.0;
        for _ in 0..12 {
            lag = learn_release_lag(lag, 0.050 - lag);
        }
        assert!(
            (lag - 0.050).abs() < 0.005,
            "twelve consistent landings should have settled near 50 ms, got {lag}"
        );

        // A landing far enough out is a misread grid, not a slow deck, and is not carried
        // forward - otherwise one bad transition poisons every one after it.
        assert_eq!(learn_release_lag(0.03, 0.400), 0.03);
        assert_eq!(learn_release_lag(0.03, f64::NAN), 0.03);

        // Never negative and never absurd, whatever it is told.
        assert!(learn_release_lag(0.0, -0.100) >= 0.0);
        for slip in [0.119, -0.119] {
            let mut lag = 0.0;
            for _ in 0..500 {
                lag = learn_release_lag(lag, slip);
            }
            assert!((0.0..=0.15).contains(&lag), "ran away to {lag}");
        }
    }
}
