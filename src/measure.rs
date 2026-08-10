//! The measurement pipeline: turn a track (local file or streamed URL) into a tempo and,
//! where the material allows it, a bar line - once per track, cached, and re-read from
//! memory for as long as [`Player::touch_facts`]'s cap keeps it around.
//!
//! Split out of `main.rs` alongside `mixer.rs`: the two halves of the automix used to sit
//! in one impl block, and every session collided with the other half's unrelated state
//! while editing this one. `survey_tempo`/`consensus` settle the tempo once from several
//! independent looks at the track; `measure_track` is the entry point a background thread
//! calls per probe, folding a fresh read over whatever was already known
//! ([`fold_probe`]). Nothing here talks to mpv or a deck - that is `mixer.rs`'s job, once
//! this has an answer.

use super::*;
use audio_tap::BAND_COUNT;

/// The same, for measuring a track a sync is going to lean on: a quarter of the step, and
/// the same cost, because what it buys is not detail in the picture but agreement.
///
/// The onset detector reports a beat a little after the transient that caused it, which
/// does not matter at all when both tracks are measured the same way - a lag common to
/// both cancels the moment one grid is subtracted from the other. At a 21 ms step it is
/// not common: two takes of the same beat came back 14 ms and 42 ms late, and the 28 ms
/// between them is a flam nobody asked for. At 5 ms they came back 33 ms and 35 ms late,
/// both further from the truth and two milliseconds apart, which is what a sync needs.
const SYNC_INTERVAL: f64 = 256.0 / 48000.0;

/// How much audio to read when measuring a grid for a sync.
///
/// Short, and both tracks get the same. A tracker's idea of the phase is anchored to the
/// end of what it heard, so asking where the bar falls at the *start* of its window means
/// counting backwards over the whole of it - and a tempo pinned down to a tenth of a per
/// cent loses a couple of milliseconds per bar doing that. Over twenty-five seconds it
/// came to thirty-seven; over eight it comes to fifteen.
///
/// Fifteen would still be too many, except that it is the same fifteen for both tracks.
/// The pair of them measured this way came back within a twentieth of a millisecond of
/// each other, and what a sync needs is not two right answers but two answers that are
/// wrong together - the bias falls out the moment one grid is subtracted from the other.
const SYNC_WINDOW: f64 = 12.0;

/// Bar length assumed when aligning a transition. Four is right for almost everything the
/// player is used for, and being wrong costs a transition on the wrong beat of the bar,
/// not a wrong tempo.
pub(crate) const BEATS_PER_BAR: u32 = 4;

/// A track's bar lines, as a line rather than a list: where the first one falls and how
/// far apart they are. Everything a sync needs to say when the next one is due.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Grid {
    pub(crate) first: f64,
    pub(crate) bar: f64,
}

impl Grid {
    /// `None` unless the measurement is usable - a tempo that would make the arithmetic
    /// meaningless is worse than admitting there is no grid.
    pub(crate) fn of(measured: Measured) -> Option<Self> {
        // No bar line, no grid - not a grid anchored at zero. A measurement can carry a
        // tempo without a downbeat (the phase windows all landed on breakdowns), and that
        // is a tempo match, not a sync: pretending the first bar falls at the top of the
        // file would seed the release and the correction loop with a reference that is
        // wrong by some arbitrary fraction of a beat, and both would then hold the mix
        // faithfully to it. An absent grid degrades to tempo-only; a fabricated one plays
        // the whole mix off by a constant.
        let first = measured.downbeat?;
        let bar = 60.0 / measured.bpm * f64::from(BEATS_PER_BAR);
        (bar.is_finite() && (0.5..8.0).contains(&bar)).then_some(Self { first, bar })
    }

    /// The first bar line at or after `when`.
    pub(crate) fn next_bar_after(&self, when: f64) -> f64 {
        let since = when - self.first;
        self.first + (since / self.bar).ceil().max(0.0) * self.bar
    }
}

/// What a probe thread sends back.
///
/// Separate answers because they have separate lifetimes: the tempo is settled once, the
/// bar line has to be asked again wherever it will be used, the frames are kept so that
/// asking again never touches the network twice, and the media URL is kept so that the
/// yt-dlp run that found it happens once per track rather than once per question.
#[derive(Default)]
pub(crate) struct ProbeResult {
    pub(crate) surveyed: Option<f64>,
    pub(crate) measured: Option<Measured>,
    pub(crate) frames: Option<Arc<preview::Frames>>,
    /// The URL ffmpeg actually decoded, when the target needed resolving first.
    pub(crate) media: Option<String>,
    /// The stream's own cover picture, found in the same process and round trip as the
    /// media URL above.
    pub(crate) thumbnail: Option<String>,
    /// The provider refused the track outright - DRM, region, gone. The entry will never
    /// play, for mpv either, and the automix should treat it accordingly.
    pub(crate) refused: bool,
}

/// Everything learned about one playlist entry ahead of it playing.
///
/// Used to be five parallel `Vec<(usize, T)>` - `probed`, `surveyed`, `analysed`,
/// `resolved_media` and `ahead_tried` - each with its own retain/push/cap dance run at a
/// different time: the exact "one fact, two orderings" shape that produced the
/// settings-row bug, the fade-range bug and the 0.971 tempo ratio, just wearing five hats.
/// One `Vec<(usize, TrackFacts)>` now, one eviction policy (see `Player::touch_facts`),
/// one invalidation point on playlist mutation.
#[derive(Clone, Default)]
pub(crate) struct TrackFacts {
    /// A probe has actually landed for this entry, successfully or not. Separate from
    /// `ahead_tried` below so marking a track for an early read never itself counts as
    /// having read it - `Player::probe_tempo` needs to tell "asked" from "answered".
    pub(crate) landed: bool,
    /// The settled tempo. A track is surveyed once; after that every re-read is only
    /// about where the bar falls now, not what the tempo is.
    pub(crate) tempo: Option<f64>,
    /// What the last probe found, folded with whatever came before it - see `fold_probe`.
    /// `None` here means measured and empty, not unasked; `landed` carries that half.
    pub(crate) measured: Option<Measured>,
    /// Whole-song frames, decoded once and sliced from memory ever after.
    pub(crate) frames: Option<Arc<preview::Frames>>,
    /// The stream URL ffmpeg actually resolved, so yt-dlp runs once per track.
    pub(crate) media_url: Option<String>,
    /// The stream's own cover picture, resolved alongside `media_url` - ffmpeg reads it
    /// directly, the same as an embedded one, so the cover box works for streams too.
    pub(crate) thumbnail: Option<String>,
    /// An ahead-of-time probe has already been kicked off for this entry, so a survey
    /// that legitimately failed is not retried every redraw for the rest of the track.
    pub(crate) ahead_tried: bool,
}

/// What a re-read is allowed to change: something, never nothing.
///
/// The new reading wins wherever it has an answer, and the old one stands in wherever it
/// does not - a fresh tempo with no bar line keeps the old bar line, and a read that came
/// back empty changes nothing at all. Trading a good answer for an absence is how the loop
/// holding two decks together loses its reference in the middle of the one moment it is
/// needed.
pub(crate) fn fold_probe(kept: Option<Measured>, new: Option<Measured>) -> Option<Measured> {
    match (kept, new) {
        (Some(old), None) => Some(old),
        (Some(old), Some(new)) if new.downbeat.is_none() => Some(Measured {
            bpm: new.bpm,
            downbeat: old.downbeat,
        }),
        (_, new) => new,
    }
}

/// Measure a track: settle its tempo if it is not already settled, and read where its bar
/// falls around `from`.
///
/// The two failures are not the same size, and treating them the same was the single
/// biggest source of "no steady beat found". The tempo comes from a survey of the whole
/// track and is the thing a sync cannot run without; the phase comes from one local window
/// and is merely the thing that makes a sync exact. A window that lands on a breakdown -
/// sixteen bars with the drums pulled out, which is not an edge case, it is the shape of
/// the music this is for - used to take the surveyed tempo down with it, and a mix that
/// could have been tempo-matched ran on a timer instead.
///
/// So the phase is tried in a few places and allowed to fail. Tempo without phase is a mix
/// that is matched but not bar-locked; tempo with phase is the full sync; neither is a
/// timer.
pub(crate) fn measure_track(
    target: &str,
    from: f64,
    known: Option<f64>,
    cached: Option<Arc<preview::Frames>>,
    media: Option<String>,
) -> ProbeResult {
    // A streaming page has to become a stream first. mpv resolves its own URLs at load
    // time through its ytdl hook; ffmpeg has no hook, and every probe on a SoundCloud set
    // used to hand it the page HTML and fail at open - which is why "no steady beat
    // found" appeared on almost every streamed mix while every local file measured fine.
    let mut refused = false;
    let mut thumbnail = None;
    let media = media.or_else(|| {
        if !youtube::needs_media_resolution(target) {
            return Some(target.to_string());
        }
        match youtube::stream_media_url(target) {
            youtube::MediaUrl::Direct {
                url,
                thumbnail: found,
            } => {
                thumbnail = found;
                Some(url)
            }
            youtube::MediaUrl::Refused => {
                refused = true;
                None
            }
            youtube::MediaUrl::Failed => None,
        }
    });
    let Some(source) = media.clone() else {
        return ProbeResult {
            surveyed: None,
            measured: None,
            frames: None,
            media: None,
            thumbnail,
            refused,
        };
    };
    // The whole track, read once, sequentially - no seeks, one connection for a stream.
    // If this read fails the seeked windows below are still there as the fallback, so a
    // stream that hates range requests and one that dropped a connection both survive.
    let whole = cached
        .or_else(|| preview::sample_from(&source, 0.0, ANALYSE_CAP, SYNC_INTERVAL).map(Arc::new));
    let surveyed = known
        .or_else(|| whole.as_deref().and_then(survey_frames))
        .or_else(|| survey_tempo(&source));
    let Some(bpm) = surveyed else {
        return ProbeResult {
            surveyed: None,
            measured: None,
            frames: whole,
            media: Some(source),
            thumbnail,
            refused: false,
        };
    };
    // Where to look for the bar line: where asked, then a little later, then a little
    // earlier. A breakdown is rarely sixteen bars in *both* directions.
    let retries = [from, from + 8.0, (from - 8.0).max(0.0)];
    let mut downbeat = None;
    let mut tried = [f64::NAN; 3];
    for (i, &at) in retries.iter().enumerate() {
        // From the top of a file the "earlier" retry is the same window again.
        if tried[..i].iter().any(|&seen| (seen - at).abs() < 0.5) {
            continue;
        }
        tried[i] = at;
        // Out of the in-memory frames when they cover it, out of a seeked read when they
        // do not (a track longer than the cap, or the whole-song read failed).
        let tracker = match &whole {
            Some(whole) => feed_window(whole, at, SYNC_WINDOW),
            None => beat_read(&source, at, SYNC_WINDOW).map(|(tracker, _)| tracker),
        };
        let Some(tracker) = tracker else {
            continue;
        };
        let Some(grid) = tracker.grid() else {
            continue;
        };
        // The tempo is the surveyed one; this window only says where the bar falls. A
        // local window that disagrees with the survey is the window that is wrong - but
        // its *phase* is only trusted when it was reasonably sure of itself, because a
        // wrong bar line is worse than none: none degrades to a tempo match, wrong plays
        // the whole mix half a beat off.
        if grid.confidence < 0.5 {
            continue;
        }
        downbeat = tracker
            .next_downbeat_in(0.0, BEATS_PER_BAR)
            .map(|delay| at + delay);
        if downbeat.is_some() {
            break;
        }
    }
    ProbeResult {
        surveyed,
        measured: Some(Measured { bpm, downbeat }),
        frames: whole,
        media: Some(source),
        thumbnail,
        refused: false,
    }
}

/// Write `bpm` into the file's own tags, on a thread, best effort.
///
/// A remux rather than a re-encode: `-c copy` moves the streams across untouched, so the
/// audio is bit-identical and the cost is a read and a write rather than a decode. Into a
/// sibling temporary file and renamed over the original, because a container rewrite that
/// dies half way through a file the user owns is not an acceptable failure mode - and
/// `rename(2)` within a directory is atomic, so it is either the old file or the new one.
///
/// Silent on failure by design. A read-only music folder, a container ffmpeg will not
/// remux, a full disk: none of them are worth interrupting playback over, and the answer
/// is already in the player's own cache either way. The tag is the durable copy, not the
/// only one.
fn write_bpm_tag(path: std::path::PathBuf, bpm: f64) {
    std::thread::spawn(move || {
        let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
            return;
        };
        let temp = path.with_extension(format!("bpm{}.{extension}", std::process::id()));
        let ok = youtube::succeeds(
            std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-nostdin", "-y", "-i"])
                .arg(&path)
                .args(["-map", "0", "-c", "copy"])
                // Both spellings: `TBPM` is the ID3v2 frame, `BPM` what Vorbis comments
                // and Matroska tags use, and one file format reads only one of them.
                .args(["-metadata", &format!("TBPM={bpm:.0}")])
                .args(["-metadata", &format!("BPM={bpm:.0}")])
                .arg(&temp),
        );
        if ok && std::fs::rename(&temp, &path).is_ok() {
            return;
        }
        let _ = std::fs::remove_file(&temp);
    });
}

/// One look at one place in a track: what the beat tracker made of it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Look {
    pub(crate) bpm: f64,
    pub(crate) confidence: f32,
}

/// Measure a track's tempo by looking at several places and taking the answer they agree on.
///
/// One look is not enough, and the reason is not noise. Tracks open with something that is
/// not the track: eight bars of pad, a spoken intro, a filtered build with no drums in it.
/// A window that lands there does not return "no idea" - it returns a confident answer
/// about the wrong thing. Measured on a 92 BPM track, the first twelve seconds gave 149.88
/// at a confidence of 0.98, and twenty seconds further in gave 92.40 at 0.996. Both looked
/// equally trustworthy from the inside; only one was right.
///
/// So the confidence that matters is not the tracker's, it is whether separate looks at
/// separate parts of the track come back with the same number. That is a thing no single
/// window can fake, and it costs three ffmpeg runs of about fifty milliseconds each, on a
/// thread nobody is waiting on.
pub(crate) fn survey_tempo(target: &str) -> Option<f64> {
    /// Where to look. Past the intro, spread out, and no assumption about how long the
    /// track is - a window past the end simply returns nothing and does not vote.
    const PLACES: [f64; 4] = [15.0, 45.0, 80.0, 120.0];
    /// Where to look again when a track turned out to be too short for most of those. The
    /// two overlap, so they are weaker evidence than two windows a minute apart - but a
    /// three minute edit that gets measured is worth more than one that gets refused.
    const CLOSE: [f64; 2] = [3.0, 12.0];
    /// Long enough to hold a couple of dozen bars at any tempo worth mixing.
    const WINDOW: f64 = 20.0;

    let look = |from: f64| -> Option<Look> {
        let (tracker, _) = beat_read(target, from, WINDOW)?;
        let grid = tracker.grid()?;
        Some(Look {
            bpm: f64::from(grid.bpm),
            confidence: grid.confidence,
        })
    };

    let mut looks: Vec<Look> = PLACES.iter().filter_map(|&from| look(from)).collect();
    if looks.len() < 2 {
        looks.extend(CLOSE.iter().filter_map(|&from| look(from)));
    }
    consensus(&looks)
}

/// How far a window's loudest onset must stand above its own background before the window
/// is treated as containing beats. Measured across a corpus: real beats came back between
/// twenty-two and twenty-six, a held chord at nine and a half.
const ONSET_RELIEF: f32 = 14.0;

/// How much of a track the whole-song analysis reads, at most. Ten minutes covers all but
/// mixes and live sets, and a set's first ten minutes still yield a dozen honest windows.
const ANALYSE_CAP: f64 = 600.0;

/// Feed `tracker`-fodder out of already-decoded frames: the window `[from, from + len)`.
///
/// This is what analysing the whole song once buys. Every window used to be its own ffmpeg
/// run - and for a stream, its own HTTP connection and its own seek, each one a separate
/// chance to fail, taken four to six times per track. Now the audio is read once, from the
/// top, sequentially - the one access pattern every server and every cache handles - and
/// every window after that is a slice of memory. A survey over the whole track costs the
/// same one decode as a survey over four corners of it, so it looks everywhere instead.
///
/// The onset gate stands in front of this reader like every other: a window without beats
/// in it hands back nothing rather than a tracker that will confidently describe a pad.
pub(crate) fn feed_window(
    whole: &preview::Frames,
    from: f64,
    len: f64,
) -> Option<analysis::BeatTracker> {
    let per = whole.interval;
    if !(per.is_finite() && per > 0.0) {
        return None;
    }
    let first = (from / per).floor().max(0.0) as usize;
    let count = (len / per).ceil() as usize;
    let quantized = whole
        .bands
        .get(first..(first + count).min(whole.bands.len()))?;
    if quantized.len() < count / 2 {
        return None; // ran off the end of the track
    }
    // Dequantized once here, not at every read below - see `preview::Frames`.
    let bands: Vec<[f32; BAND_COUNT]> = quantized
        .iter()
        .map(|frame| frame.map(preview::dequantize))
        .collect();
    let mut flux = Vec::with_capacity(bands.len());
    for pair in bands.windows(2) {
        let rise: f32 = pair[1]
            .iter()
            .zip(&pair[0])
            .map(|(now, before)| (now - before).max(0.0))
            .sum();
        flux.push(rise);
    }
    let mean = flux.iter().sum::<f32>() / flux.len().max(1) as f32;
    let peak = flux.iter().copied().fold(0.0f32, f32::max);
    if mean <= 0.0 || !mean.is_finite() || peak / mean < ONSET_RELIEF {
        return None;
    }
    let mut tracker = analysis::BeatTracker::new();
    for (i, band) in bands.iter().enumerate() {
        let rms = whole
            .rms
            .get(first + i)
            .copied()
            .map_or(0.0, preview::dequantize);
        tracker.feed(band, rms, i as f64 * per);
    }
    Some(tracker)
}

/// Survey a whole decoded track: a window every half-minute, and the tempo they agree on.
///
/// More looks than the four seeked corners ever gave - a four minute track yields eight or
/// nine - which is what makes the consensus hard to starve. A track whose beats live only
/// in its middle third still seats several agreeing voters, where four fixed corners could
/// all land wrong.
pub(crate) fn survey_frames(whole: &preview::Frames) -> Option<f64> {
    const WINDOW: f64 = 20.0;
    const STEP: f64 = 30.0;
    let duration = whole.bands.len() as f64 * whole.interval;
    let mut looks = Vec::new();
    let mut from = 5.0f64.min((duration - WINDOW).max(0.0));
    while from + WINDOW * 0.5 <= duration && looks.len() < 20 {
        if let Some(tracker) = feed_window(whole, from, WINDOW)
            && let Some(grid) = tracker.grid()
        {
            looks.push(Look {
                bpm: f64::from(grid.bpm),
                confidence: grid.confidence,
            });
        }
        from += STEP;
    }
    consensus(&looks)
}

/// Decode a window and hand back a tracker that has heard it - or nothing, when the window
/// does not contain beats.
///
/// The gate is on the way in, for every reader alike, because every reader has the same
/// failure without it: a sustained pad autocorrelates perfectly, and the tracker will
/// happily name a tempo for it - 61 BPM, measured, from a chord held for ninety seconds -
/// and a bar line to go with it. What separates music from a drone is not how regular it
/// is but whether anything in it *starts*: real beats came back with flux peaks twenty-odd
/// times their background, the drone under ten. The first version gated only the tempo
/// survey, and the phase read then "found" a downbeat in a breakdown's pad - a fabricated
/// reference that the release and the correction loop would have held the whole mix to.
pub(crate) fn beat_read(
    target: &str,
    from: f64,
    window: f64,
) -> Option<(analysis::BeatTracker, f64)> {
    let frames = preview::sample_from(target, from, window, SYNC_INTERVAL)?;
    // Dequantized once here, not at every read below - see `preview::Frames`.
    let bands: Vec<[f32; BAND_COUNT]> = frames
        .bands
        .iter()
        .map(|frame| frame.map(preview::dequantize))
        .collect();
    let mut flux = Vec::with_capacity(bands.len());
    for pair in bands.windows(2) {
        let rise: f32 = pair[1]
            .iter()
            .zip(&pair[0])
            .map(|(now, before)| (now - before).max(0.0))
            .sum();
        flux.push(rise);
    }
    let mean = flux.iter().sum::<f32>() / flux.len().max(1) as f32;
    let peak = flux.iter().copied().fold(0.0f32, f32::max);
    if mean <= 0.0 || !mean.is_finite() || peak / mean < ONSET_RELIEF {
        return None;
    }
    let mut tracker = analysis::BeatTracker::new();
    for (i, band) in bands.iter().enumerate() {
        let rms = frames.rms.get(i).copied().map_or(0.0, preview::dequantize);
        tracker.feed(band, rms, frames.start + i as f64 * frames.interval);
    }
    Some((tracker, from))
}

/// The tempo a set of looks agree on, or `None` when they do not.
///
/// Agreement comes in two widths. Machine-made music repeats to a fraction of a per cent,
/// and windows that agree within 1.5% are reporting one tempo. Played music is not like
/// that: a drummer moves a few per cent across a song, and four honest windows on a live
/// record come back 91, 94, 96 - which the strict rule read as disagreement and refused,
/// sending a track with an obvious groove to a timer. So when nothing agrees strictly, the
/// question is asked again at the width a human plays to, and the mean of that cluster is
/// the answer. It is a softer number - the correction loop runs against it rather than
/// trusting it - but a mix matched to the middle of a drifting tempo beats one matched to
/// nothing by a distance.
pub(crate) fn consensus(looks: &[Look]) -> Option<f64> {
    /// Two looks are the same answer if they are this close. Wide enough for the quantised
    /// grid the tracker works on, narrow enough that 128 and 130 are still two answers.
    const SAME: f64 = 0.015;
    /// And the width played music wobbles over, for the second asking.
    const HUMAN: f64 = 0.05;
    /// A look this unsure does not get a vote at all.
    const FLOOR: f32 = 0.45;

    let mut votes: Vec<f64> = looks
        .iter()
        .filter(|look| look.confidence >= FLOOR && look.bpm.is_finite() && look.bpm > 20.0)
        .map(|look| look.bpm)
        .collect();
    if votes.is_empty() {
        return None;
    }
    // Fold to a common octave before comparing. Half and double time are the same answer
    // about the same music, and a tracker that says 87 where another says 174 is agreeing.
    let anchor = votes[0];
    for vote in &mut votes {
        for factor in [0.25, 0.5, 1.0, 2.0, 4.0] {
            if (*vote * factor - anchor).abs() < (*vote - anchor).abs() {
                *vote *= factor;
            }
        }
    }
    // The largest group that agrees with each other. Not the average of everything: one
    // window that landed on an intro should be outvoted, not blended in.
    let mut best: Option<(usize, f64)> = None;
    for &candidate in &votes {
        let agreeing: Vec<f64> = votes
            .iter()
            .copied()
            .filter(|vote| (vote - candidate).abs() / candidate <= SAME)
            .collect();
        let mean = agreeing.iter().sum::<f64>() / agreeing.len() as f64;
        if best.is_none_or(|(count, _)| agreeing.len() > count) {
            best = Some((agreeing.len(), mean));
        }
    }
    let (count, bpm) = best?;
    if count >= 2 {
        return Some(bpm);
    }
    // Nothing agreed strictly. Ask again at the width a human plays to before giving up.
    let mut widest: Option<(usize, f64)> = None;
    for &candidate in &votes {
        let agreeing: Vec<f64> = votes
            .iter()
            .copied()
            .filter(|vote| (vote - candidate).abs() / candidate <= HUMAN)
            .collect();
        let mean = agreeing.iter().sum::<f64>() / agreeing.len() as f64;
        if widest.is_none_or(|(n, _)| agreeing.len() > n) {
            widest = Some((agreeing.len(), mean));
        }
    }
    // One vote is still not agreement, at any width. With a single usable look there is
    // nothing to check it against, and a confident wrong answer is worse than an admitted
    // absence: the mix falls back to a timer, which is at least honest about what it is
    // doing.
    widest.and_then(|(n, mean)| (n >= 2).then_some(mean))
}

/// What a slice of the arriving track turned out to be, measured before it was audible.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Measured {
    pub(crate) bpm: f64,
    /// Seconds from the start of the file to its first bar line, when one was found. This
    /// is the difference between two decks at the same speed and two decks in time.
    pub(crate) downbeat: Option<f64>,
}

impl Player {
    /// The tempo of the track about to arrive, measured before it is audible.
    ///
    /// `None` until the measurement lands, which is the answer for the first few hundred
    /// milliseconds after it is asked for and forever if the track cannot be decoded.
    pub(crate) fn incoming_bpm(&mut self, entry: usize) -> Option<f64> {
        self.measured(entry).map(|m| m.bpm)
    }

    /// What was measured for `entry`, if the measurement has landed.
    pub(crate) fn measured(&self, entry: usize) -> Option<Measured> {
        self.facts
            .iter()
            .find(|(cached, _)| *cached == entry)
            .and_then(|(_, facts)| facts.measured)
    }

    /// Merge `patch` into whatever is already known about `entry`, creating a fresh
    /// [`TrackFacts`] if this is the first thing learned about it, and moving the entry to
    /// the back of the cap - the one eviction policy every fact here now shares, in place
    /// of the four independent ones this replaced.
    pub(crate) fn touch_facts(&mut self, entry: usize, patch: impl FnOnce(&mut TrackFacts)) {
        let mut facts = self
            .facts
            .iter()
            .find(|(cached, _)| *cached == entry)
            .map(|(_, facts)| facts.clone())
            .unwrap_or_default();
        patch(&mut facts);
        self.facts.retain(|(cached, _)| *cached != entry);
        self.facts.push((entry, facts));
        // Frames are the expensive part - a few megabytes each - so they set the cap even
        // though the rest of what is here would happily hold more.
        while self.facts.len() > 3 {
            self.facts.remove(0);
        }
    }

    /// Start measuring a track, off the UI thread.
    ///
    /// It decodes a slice with ffmpeg: about ninety milliseconds for a local file, a third
    /// of a second over the network, and a twenty-second ceiling when something is wrong.
    /// None of that can happen on the thread that paints frames, so it happens on its own
    /// and the answer is collected later - fine, because this starts at the cue, several
    /// seconds before anything needs it.
    ///
    /// Two numbers come back. The tempo is what a beat match needs; the offset of the
    /// track's first bar line is what a *sync* needs, because two decks at the same BPM
    /// whose bars do not line up are not in time, they are merely the same speed.
    ///
    /// Both tracks go through here, including the one already playing, and that is the
    /// point. The live tap can measure the playing track too, but it samples at whatever
    /// irregular rate the interface happens to redraw at, so its answer carries a
    /// different bias from this one. Dividing one estimator's number by another's turned
    /// two identical files into a ratio of 0.971 and three per cent of drift. Measured the
    /// same way, their errors are the same error, and it cancels.
    pub(crate) fn probe_tempo(&mut self, entry: usize, from: f64) {
        if self
            .facts
            .iter()
            .any(|(cached, facts)| *cached == entry && facts.landed)
            || self.probing.iter().any(|(cached, _)| *cached == entry)
        {
            return;
        }
        // A track played before already has an answer on disk - ready to mix at t=0,
        // forever, no probe needed. A later sync still re-reads the bar line for itself
        // (`refresh_grid` calls `spawn_probe` directly), so a stale downbeat here only
        // ever costs one extra correction tick, never a wrong mix.
        let url = self.source.track_url(entry);
        if let Some(known) = (!url.is_empty()).then(|| self.analysis.get(&url)).flatten() {
            self.touch_facts(entry, |facts| {
                facts.landed = true;
                facts.tempo = Some(known.bpm);
                facts.measured = Some(Measured {
                    bpm: known.bpm,
                    downbeat: known.downbeat,
                });
            });
            return;
        }
        self.spawn_probe(entry, from);
    }

    /// The same, whether or not there is already an answer for `entry`.
    pub(crate) fn spawn_probe(&mut self, entry: usize, from: f64) {
        let target = self.source.track_url(entry);
        if target.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        // Whether this probe also has to settle the tempo, or only find the phase. A track
        // is surveyed properly once; after that every re-read is about *where the bar is
        // now*, and rediscovering the tempo from a twelve second window each time is how a
        // grid that was right at the cue turns into a different one halfway through a mix.
        let (known, cached, media) = self
            .facts
            .iter()
            .find(|(at, _)| *at == entry)
            .map(|(_, facts)| (facts.tempo, facts.frames.clone(), facts.media_url.clone()))
            .unwrap_or_default();
        // A file saved since the last probe outranks whatever the network resolved
        // earlier - same audio, free to read again, where a resolved stream URL is not
        // (and often will not even still be valid by the time it is needed).
        let media = self
            .downloads
            .get(&target)
            .map(|path| path.display().to_string())
            .or(media);
        std::thread::spawn(move || {
            let _ = tx.send(measure_track(&target, from, known, cached, media));
        });
        self.probing.push((entry, rx));
        if self.next_entry_index() == Some(entry) {
            self.analysis_since = Some(Instant::now());
            // The status line has just changed, so repaint rather than waiting for
            // something else to happen to notice.
            self.dirty = true;
        }
    }

    /// Start analysing the next track as soon as it is known, not at the cue.
    ///
    /// The cue is six seconds before the overlap, and a whole-song read of a streamed
    /// track can take longer than that on a slow line - at which point the measurement
    /// lands after the transition has started and is thrown away, and the mix runs on a
    /// timer that says "no steady beat found" about a track that was never listened to.
    /// Started here, the read has the whole length of the current track to finish in, and
    /// by the cue the answer is a cache hit.
    ///
    /// Once per entry, not per redraw: a survey that legitimately failed - a dead link, a
    /// drone - would otherwise be retried forever. The cue gets one deliberate retry,
    /// because a network blip at the start of a track should not decide the whole mix.
    pub(crate) fn maybe_probe_ahead(&mut self) {
        if !self.beat_mixing() || self.snap.paused || !self.has_next() {
            return;
        }
        let Some(next) = self.next_entry_index() else {
            return;
        };
        if self
            .facts
            .iter()
            .any(|(cached, facts)| *cached == next && facts.ahead_tried)
        {
            return;
        }
        self.touch_facts(next, |facts| facts.ahead_tried = true);
        self.probe_tempo(next, 0.0);
    }

    /// What to say about the track coming next, if anything.
    ///
    /// Only in the run-up to a transition, which is the only time any of it is true: the
    /// measurement is taken at the cue and thrown away once the mix is over. Outside that
    /// window there is no next track being prepared and the line belongs to something else.
    pub(crate) fn analysis_state(&self) -> Option<ui::Analysis> {
        if !self.beat_mixing() {
            return None;
        }
        let next = self.next_entry_index()?;
        // Held for a moment after it finishes, not just while it runs. Reading a local
        // file takes about a third of a second, which at the rate the interface repaints
        // is one or two frames - long enough to be true and not long enough to be seen,
        // and a state nobody can read is the same as one that was never shown.
        const LINGER: Duration = Duration::from_millis(1200);
        let reading = self.probing.iter().any(|(entry, _)| *entry == next)
            || self
                .analysis_since
                .is_some_and(|began| began.elapsed() < LINGER);
        if reading {
            return Some(ui::Analysis::Reading);
        }
        let (_, facts) = self
            .facts
            .iter()
            .find(|(cached, facts)| *cached == next && facts.landed)?;
        let Some(measured) = facts.measured else {
            // Only said while a transition is actually being set up. Before that there is
            // nothing at stake yet - the cue will retry, and announcing a failure that
            // may not survive the retry is noise.
            return self.cross.is_some().then_some(ui::Analysis::Unreadable);
        };
        // Measured ahead of time, transition not yet cued: worth saying, because this is
        // the state the whole track spends most of its length in, and it is the answer to
        // "did the analysis work" long before the mix depends on it.
        if self.cross.is_none() {
            return Some(ui::Analysis::Ready { bpm: measured.bpm });
        }
        // The stretch is what was actually applied, not what could have been: a score with
        // no tempo lane leaves the arriving deck alone however well the two would have
        // matched, and saying "locked" then would be a lie about the mix.
        if !self.active_transition().wants_tempo_match() {
            return Some(ui::Analysis::Free { bpm: measured.bpm });
        }
        Some(ui::Analysis::Matched {
            bpm: measured.bpm,
            stretch: self.tempo_ratio - 1.0,
        })
    }

    /// Measure the playing track's grid over a window ending where it is now.
    ///
    /// Called twice: once at the cue, so there is something to work with, and once again a
    /// few seconds before the transition starts. The second one is the one that matters. A
    /// grid is a bar line and a spacing, and using it later than it was taken means
    /// multiplying the spacing's error by the bars in between - the cue happens most of a
    /// minute before a sixteen bar blend begins, which is twenty-odd bars of that, and at
    /// the tenth of a per cent a tempo can be pinned down to it comes to some tens of
    /// milliseconds. Measured again with the transition a few seconds off, there are two
    /// bars to get wrong instead of twenty.
    pub(crate) fn measure_playing_grid(&mut self) {
        self.grid_taken = self.snap.position;
        let entry = self.entry_index();
        // From here forwards, not from here back: the arriving track's grid is read from
        // its own opening, and the two have to be read the same way or their biases stop
        // cancelling. Reading ahead of the playhead is free - it is a file.
        let Some(at) = self.snap.position else {
            return;
        };
        // The old answer stays in place until the new one lands, so nothing that depends
        // on there being a grid sees a gap while this runs.
        self.refresh_grid(entry, at);
    }

    /// Read `entry`'s grid again from `at`, unless that is already happening.
    ///
    /// The old answer stays in place until the new one lands, so nothing that depends on
    /// there being a grid sees a gap while this runs.
    pub(crate) fn refresh_grid(&mut self, entry: usize, at: f64) {
        if self.probing.iter().any(|(cached, _)| *cached == entry) {
            return;
        }
        self.spawn_probe(entry, at.max(0.0));
    }

    /// Collect any finished measurements.
    ///
    /// Kept to a handful: the only ones that matter are the track playing and the one
    /// after it, and the first of those is usually already here because it was the
    /// arriving track last time round - so a sync normally costs one decode, not two.
    pub(crate) fn drain_tempo_probe(&mut self) {
        let mut still_running = Vec::new();
        let mut landed = Vec::new();
        for (entry, rx) in std::mem::take(&mut self.probing) {
            match rx.try_recv() {
                Ok(outcome) => landed.push((entry, outcome)),
                Err(TryRecvError::Empty) => still_running.push((entry, rx)),
                Err(TryRecvError::Disconnected) => landed.push((entry, ProbeResult::default())),
            }
        }
        self.probing = still_running;
        if landed.is_empty() {
            return;
        }
        for (entry, outcome) in landed {
            // A definitive refusal from the provider is a fact about the entry, not about
            // the analysis: mpv's own loader will hit the same wall. Marking it means the
            // queue dims it and the automix skips over it instead of cueing a transition
            // into a track that will never load.
            if outcome.refused
                && let Some(flag) = self.missing.get_mut(entry)
            {
                *flag = true;
            }
            // A failed re-read does not erase a good answer. Grids are re-read during a
            // mix so they stay anchored near the playhead, and a re-read can land on a
            // breakdown and come back with nothing - at which point the loop holding the
            // two decks together used to lose its reference mid-transition and stop
            // correcting. A slightly stale grid drifts by milliseconds; no grid at all
            // stops the sync dead. Only ever trade something for something - `fold_probe`
            // keeps the old answer wherever the new one came back empty.
            // A survey happens once per track (see `spawn_probe`), so this is also the
            // one moment worth writing to the on-disk cache - not every phase re-read.
            let surveyed_now = outcome.surveyed.is_some();
            self.touch_facts(entry, move |facts| {
                facts.landed = true;
                if let Some(bpm) = outcome.surveyed {
                    facts.tempo = Some(bpm);
                }
                if let Some(frames) = outcome.frames {
                    facts.frames = Some(frames);
                }
                if let Some(url) = outcome.media {
                    facts.media_url = Some(url);
                }
                if let Some(url) = outcome.thumbnail {
                    facts.thumbnail = Some(url);
                }
                facts.measured = fold_probe(facts.measured, outcome.measured);
            });
            if surveyed_now
                && let Some(known) = self
                    .facts
                    .iter()
                    .find(|(cached, _)| *cached == entry)
                    .map(|(_, facts)| (facts.tempo, facts.measured.and_then(|m| m.downbeat)))
                && let Some(bpm) = known.0
            {
                let url = self.source.track_url(entry);
                if !url.is_empty() {
                    self.analysis.record(&url, bpm, known.1);
                    // A file on disk gets told what it is, so the next session - and any
                    // other DJ tool that reads `TBPM` - never pays for this decode again.
                    // The player's own cache would cover the next session too, but a tag
                    // travels with the file when it is copied, backed up or moved, and a
                    // cache does not.
                    let local = self
                        .downloads
                        .get(&url)
                        .map(|path| path.to_path_buf())
                        .or_else(|| {
                            let path = std::path::PathBuf::from(&url);
                            path.is_file().then_some(path)
                        });
                    if let Some(path) = local {
                        write_bpm_tag(path, bpm);
                    }
                }
            }
        }
        // Asked for at the cue, so this usually lands before the overlap starts - but if
        // the deck is already cued, apply it now rather than waiting for a transition that
        // has already begun without it.
        if let Some(next) = self.next_entry_index()
            && self.cross.is_some_and(|cross| cross.started.is_none())
        {
            self.tempo_ratio = self.tempo_ratio_for(next);
            if (self.tempo_ratio - 1.0).abs() > 0.001 {
                if let Some(deck) = self.spare.as_mut() {
                    let _ = deck.set_speed(self.tempo_ratio);
                }
                let percent = (self.tempo_ratio - 1.0) * 100.0;
                self.show_toast(format!("⇄ arriving track pulled {percent:+.1}% to match"));
            }
            self.cue_sync_seek();
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_reread_never_erases_a_good_answer() {
        let good = Measured {
            bpm: 128.0,
            downbeat: Some(4.5),
        };
        // Nothing came back: the old answer stands.
        assert_eq!(
            fold_probe(Some(good), None).expect("kept").downbeat,
            Some(4.5)
        );
        // A fresh tempo with no bar line keeps the old bar line.
        let tempo_only = Measured {
            bpm: 127.5,
            downbeat: None,
        };
        let folded = fold_probe(Some(good), Some(tempo_only)).expect("folded");
        assert_eq!(folded.downbeat, Some(4.5), "the bar line was thrown away");
        assert!(
            (folded.bpm - 127.5).abs() < 1e-9,
            "the fresh tempo should win"
        );
        // A full fresh answer replaces outright, and nothing from nothing is nothing.
        let fresh = Measured {
            bpm: 127.5,
            downbeat: Some(61.2),
        };
        assert_eq!(
            fold_probe(Some(good), Some(fresh)).expect("new").downbeat,
            Some(61.2)
        );
        assert!(fold_probe(None, None).is_none());
    }

    #[test]
    fn a_tempo_without_a_bar_line_is_a_match_not_a_grid() {
        // The phase windows can all land on breakdowns; what is left is a tempo. That
        // still pulls the arriving deck to speed - but it must not become a grid anchored
        // at zero, because the release and the correction loop would then hold the mix
        // faithfully to a bar line that was never measured.
        let tempo_only = Measured {
            bpm: 128.0,
            downbeat: None,
        };
        assert!(
            Grid::of(tempo_only).is_none(),
            "a grid was invented from no bar line"
        );
        let seen = Measured {
            bpm: 128.0,
            downbeat: Some(2.1),
        };
        assert!(Grid::of(seen).is_some());
    }

    #[test]
    fn played_music_that_drifts_still_gets_a_tempo() {
        let look = |bpm: f64| Look {
            bpm,
            confidence: 0.9,
        };
        // A live drummer: four honest windows, none within the machine width of another.
        // Refusing this was the second biggest source of "no steady beat found".
        let drifting = [look(91.0), look(94.0), look(96.0), look(93.0)];
        let got = consensus(&drifting).expect("a drifting groove still has a tempo");
        assert!((92.0..95.5).contains(&got), "settled on {got}");
        // But genuine disagreement is still refused at any width.
        assert_eq!(consensus(&[look(128.0), look(97.0)]), None);
        // And one look alone still is not agreement, however wide the net.
        assert_eq!(consensus(&[look(91.0)]), None);
    }
}

#[cfg(test)]
mod survey_tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A track with drums, a bass line and a held pad, opening with `intro` seconds of the
    /// pad alone - which is what almost every record does and what a single look at the
    /// top of a file lands in.
    fn track(path: &str, bpm: f64, intro: f64, seconds: u32) {
        let beat = 60.0 / bpm;
        let bar = 2.0 * beat;
        let half = 0.5 * beat;
        let expr = format!(
            "aevalsrc='gt(t,{intro})*1.0*sin(2*PI*(55-25*mod(t-{intro},{beat})/{beat})*t)\
             *exp(-9*mod(t-{intro},{beat}))\
             +gt(t,{intro})*0.45*random(1)*exp(-30*mod(t-{intro}+{bar}-{beat},{bar}))\
             +gt(t,{intro})*0.10*random(2)*exp(-60*mod(t-{intro}+{half},{beat}))\
             +gt(t,{intro})*0.30*sin(2*PI*110*t)*exp(-3*mod(t-{intro},{bar}))\
             +0.12*sin(2*PI*220*t)+0.08*sin(2*PI*330*t)':d={seconds}:s=48000"
        );
        let ok = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", &expr])
            .args(["-c:a", "libmp3lame", "-b:a", "192k", path])
            .status()
            .expect("run ffmpeg");
        assert!(ok.success(), "could not build {path}");
    }

    /// A held chord and nothing else: no beat to find, at any confidence.
    fn drone(path: &str, seconds: u32) {
        let expr = format!(
            "aevalsrc='0.3*sin(2*PI*220*t)+0.2*sin(2*PI*277*t)+0.15*sin(2*PI*330*t)'\
             :d={seconds}:s=48000"
        );
        let ok = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", &expr])
            .args(["-c:a", "libmp3lame", path])
            .status()
            .expect("run ffmpeg");
        assert!(ok.success(), "could not build {path}");
    }

    /// The same, with the drums pulled out between `hole_from` and `hole_to` - the shape
    /// of a breakdown, which is where a phase window lands often enough to matter.
    fn track_with_hole(path: &str, bpm: f64, hole_from: f64, hole_to: f64, seconds: u32) {
        let beat = 60.0 / bpm;
        let bar = 2.0 * beat;
        let half = 0.5 * beat;
        let expr = format!(
            "aevalsrc='(lt(t,{hole_from})+gt(t,{hole_to}))*(1.0*sin(2*PI*(55-25*mod(t,{beat})/{beat})*t)\
             *exp(-9*mod(t,{beat}))\
             +0.45*random(1)*exp(-30*mod(t+{bar}-{beat},{bar}))\
             +0.10*random(2)*exp(-60*mod(t+{half},{beat})))\
             +0.12*sin(2*PI*220*t)+0.08*sin(2*PI*330*t)':d={seconds}:s=48000"
        );
        let ok = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", &expr])
            .args(["-c:a", "libmp3lame", "-b:a", "192k", path])
            .status()
            .expect("run ffmpeg");
        assert!(ok.success(), "could not build {path}");
    }

    #[test]
    fn a_breakdown_where_the_phase_window_lands_costs_the_bar_line_not_the_tempo() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ytmhole_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("hole.mp3").to_string_lossy().into_owned();
        // Beats either side of a forty-second hole; the phase is asked for in the middle
        // of it, far enough in that every retry window is still inside the hole.
        track_with_hole(&path, 128.0, 20.0, 60.0, 90);
        let outcome = measure_track(&path, 36.0, None, None, None);
        assert!(
            outcome.frames.is_some(),
            "the whole-song read failed on a local file, so every probe would pay for \
             seeked windows it did not need"
        );
        let (surveyed, measured) = (outcome.surveyed, outcome.measured);
        let bpm = surveyed.expect("the survey looks either side of the hole");
        assert!(
            (bpm - 128.0).abs() / 128.0 < 0.01,
            "surveyed {bpm:.2} for a 128 BPM track with a hole in it"
        );
        let measured = measured.expect(
            "a breakdown under the phase window turned a surveyed tempo into nothing - \
             this is the exact path that said `no steady beat found` about music with an \
             obvious one",
        );
        assert!(
            measured.downbeat.is_none(),
            "no window here contains a beat, so claiming a bar line would be invention"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_surveyed_tempo_outvotes_the_window_that_landed_on_the_intro() {
        if !have_ffmpeg() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ytmsurvey_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let at = |name: &str| dir.join(name).to_string_lossy().into_owned();

        // Twelve seconds of pad before anything starts. One look at the top of this file
        // reports a confident tempo for the pad; the survey has to outvote it.
        let slow = at("slow.mp3");
        track(&slow, 92.0, 12.0, 70);
        let got = survey_tempo(&slow).expect("no tempo for a track with an obvious one");
        assert!(
            (got - 92.0).abs() / 92.0 < 0.01,
            "surveyed {got:.2} for a 92 BPM track - a single window here returns about 150"
        );

        // A short edit, where most of the places to look fall off the end.
        let short = at("short.mp3");
        track(&short, 128.0, 0.0, 26);
        let got = survey_tempo(&short).expect("a short track still has a tempo");
        assert!(
            (got - 128.0).abs() / 128.0 < 0.01,
            "surveyed {got:.2} for a 128 BPM edit"
        );

        // And nothing with a beat in it is refused rather than guessed at. The tracker on
        // its own names a tempo for this quite happily, which is worse than saying no:
        // a mix beat-matched to a number that means nothing sounds like a fault.
        let held = at("drone.mp3");
        drone(&held, 70);
        assert_eq!(survey_tempo(&held), None, "named a tempo for a held chord");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agreement_is_what_makes_an_answer_and_one_look_is_not_agreement() {
        let look = |bpm: f64, confidence: f32| Look { bpm, confidence };
        // Two that agree carry it, and the answer is their mean rather than either.
        assert!(
            (consensus(&[look(128.0, 0.9), look(128.4, 0.9)]).expect("agreed") - 128.2).abs()
                < 0.01
        );
        // Half time is the same answer about the same music.
        let folded = consensus(&[look(174.0, 0.9), look(87.0, 0.9)]).expect("octaves agree");
        assert!((folded - 174.0).abs() < 0.5, "folded to {folded}");
        // A lone window is not evidence, however sure it sounds.
        assert_eq!(consensus(&[look(150.0, 0.99)]), None);
        // Nor are two that disagree.
        assert_eq!(consensus(&[look(128.0, 0.9), look(97.0, 0.9)]), None);
        // The odd one out is outvoted, not averaged in.
        let out = consensus(&[look(92.0, 0.9), look(150.0, 0.98), look(92.3, 0.9)]);
        assert!((out.expect("majority") - 92.15).abs() < 0.1, "{out:?}");
        // And an unsure look does not get a vote at all.
        assert_eq!(consensus(&[look(128.0, 0.9), look(128.0, 0.2)]), None);
    }
}
