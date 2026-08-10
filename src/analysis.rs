//! Beat tracking over the visualiser's band levels, so an automatic transition can land
//! on a bar line rather than wherever the wall clock happened to stop.
//!
//! No PCM reaches this side of the player: [`crate::audio_tap::Tap`] already measures
//! what mpv is decoding and prints octave-spaced band levels, and those levels are all
//! there is to work with. The chain is the usual one - spectral flux for onsets,
//! autocorrelation over an onset history for tempo, a comb over the same history for
//! phase - fitted to two awkward facts about where the numbers come from.
//!
//! The first is that frames arrive whenever the UI redraws: 30 Hz, 10 Hz or 4 Hz
//! depending on what is on screen, and not at all while paused. Nothing may assume a
//! sample interval, so every measurement is binned onto a fixed grid indexed by the
//! *track's* clock, and each frame's rise is spread across the interval it covers in
//! proportion to how much of each cell that interval touches. Both halves of that
//! matter. Dropping a rise in as a spike would leave a pulse train at the redraw rate -
//! 30 Hz frames into 20 ms cells land one, two, one, two cells apart, which is a 0.1 s
//! pulse whose subharmonics are 120 and 60 BPM - and that artefact does not care what
//! the music is doing, which is exactly what makes it dangerous. Spreading a rise
//! evenly rather than by overlap does the same thing more quietly. What remains is that
//! a slow stretch deposits far less per second than a fast one, which is honest: it is
//! worth less.
//!
//! The second is that this runs on the UI thread. [`BeatTracker::feed`] is sixteen
//! subtractions and a couple of stores, tens of nanoseconds; the search runs four times
//! a second at most, over fixed-size arrays owned by the tracker, so the steady state
//! allocates nothing at all.
//!
//! Confidence is meant to be believed. A player that cuts a song on an invented tempo is
//! worse than one that never cuts on the beat at all, so the number is a product of
//! several independent reasons to doubt rather than an average of them: silence, noise,
//! a held tone and a UI too slow to resolve the beat all report near zero instead of
//! something plausible, and [`BeatTracker::grid`] stays `None` until five seconds of
//! history have earned an answer.
//!
//! What it does not do: name the metrical level. Half or double the tempo scores almost
//! as well as the truth on plenty of material, and the penalties here only tilt the
//! decision. That is much less serious than it sounds for this use - a bar line drawn at
//! half tempo is still one of the real ones - and it is reflected in the confidence
//! rather than hidden.

/// Bands per frame, matching [`crate::audio_tap::BAND_COUNT`].
const BANDS: usize = 16;

/// Onset-grid resolution. The tap measures roughly every 21 ms, so a 20 ms cell is about
/// as fine as the data can honestly support; the tempo search interpolates for the rest.
const CELL: f64 = 0.02;

/// Onset history, in cells: 16 seconds. Long enough for ~16 beats at the slowest tempo
/// in range, short enough that a tempo change is followed within a few seconds.
const CELLS: usize = 800;

/// Slowest tempo entertained.
const MIN_BPM: f32 = 60.0;
/// Fastest tempo entertained.
const MAX_BPM: f32 = 180.0;

/// Shortest beat period searched, in whole cells. One cell below the 180 BPM period of
/// 16.67 cells, so that the fractional refinement can actually reach the top of the
/// range instead of being clamped to 176 BPM.
const MIN_LAG: usize = 16;
/// Longest beat period searched, in cells (60 BPM).
const MAX_LAG: usize = 50;
/// Shortest period the refinement may settle on: exactly [`MAX_BPM`].
const MIN_PERIOD: f64 = 60.0 / (MAX_BPM as f64 * CELL);
/// Longest, exactly [`MIN_BPM`].
const MAX_PERIOD: f64 = 60.0 / (MIN_BPM as f64 * CELL);
/// Autocorrelation is computed this far out so a candidate can be scored on its second
/// and third harmonic, which is what separates a beat from an off-beat.
const AC_LAGS: usize = MAX_LAG * 3 + 2;

/// Step of the tempo scan, in cells. A quarter of a cell is 1.5 BPM at the fast end and
/// 0.15 at the slow end, and the winner is refined from there.
const CANDIDATE_STEP: f64 = 0.25;
/// Candidates in the scan.
const CANDIDATES: usize = ((MAX_LAG - MIN_LAG) * 4) + 1;

/// History needed before any estimate is offered.
const MIN_HISTORY: f64 = 5.0;

/// Track seconds between tempo searches. Four a second is far more than the estimate
/// moves and keeps the cost per frame down where it belongs.
const SEARCH_EVERY: f64 = 0.25;

/// An interval longer than this tells nothing about onsets - a kick can have come and
/// gone between the two frames - so its flux is discarded rather than smeared.
const MAX_GAP: f64 = 1.0;

/// Frames closer together than this are treated as a repeat: a paused UI redrawing the
/// same position, or two redraws inside one tap window.
const MIN_STEP: f64 = 0.001;

/// A backwards jump beyond this is a seek, not clock jitter.
const SEEK_BACK: f64 = 0.25;

/// Below this normalised RMS there is nothing playing worth analysing. The tap maps
/// -60 dB to 0.0, so this sits near -50 dB.
const SILENCE: f32 = 0.06;

/// Confidence at or above which the grid is worth acting on. Mirrors the threshold the
/// caller is told about, so [`BeatTracker::next_downbeat_in`] cannot hand back a bar
/// line the caller would have rejected anyway.
const TRUST: f32 = 0.5;

/// Band weights for the bar-line vote. A bar announces itself at the bottom of the
/// spectrum - the kick, and the bass note that comes in with the new harmony - so the
/// vote is a low-passed flux; the negative weights up top mean a beat carrying nothing
/// but hats and snare cannot win the vote by being busy.
const BAR_WEIGHT: [f32; BANDS] = [
    1.00, 1.00, 1.00, 1.00, 0.90, 0.70, 0.50, 0.00, -0.30, -0.40, -0.40, -0.40, -0.40, -0.40,
    -0.40, -0.40,
];

/// Half-width of the moving average taken out of the onset envelope, in cells. Two
/// seconds across: longer than the slowest beat in range, shorter than the stretches of
/// one redraw rate the UI produces.
const HIGH_PASS: usize = 50;

/// Cells either side of a beat that count towards its bar vote. Wide enough to survive
/// the drift of an imperfect period across the history, narrow enough not to swallow the
/// neighbouring beat at 180 BPM.
const VOTE_SPREAD: i64 = 2;

/// Bar lengths beyond this are not music this player will meet, and the vote array is
/// fixed size.
const MAX_BEATS_PER_BAR: u32 = 16;

/// Tempo prior centre. Octave errors are the usual failure of every autocorrelation
/// tracker; a gentle pull towards the middle of the range breaks ties without
/// overriding evidence.
const PRIOR_BPM: f32 = 112.0;
/// Width of that prior, in octaves. Wide - the whole search range is 1.6 octaves.
const PRIOR_OCTAVES: f32 = 2.4;

/// How much a candidate is punished for repeating half-way between its own beats.
/// Below about 0.5 the half-tempo reading wins on anything with a four-bar pattern;
/// above about 0.9 a track with strong off-beat hats loses its own tempo to the double.
const OFFBEAT: f32 = 0.65;

/// The same for a candidate that is really three beats long. Much the same weight as
/// [`OFFBEAT`], because it is the same argument.
const THIRD: f32 = 0.65;

/// And five, which is the only other sub-multiple an even test cannot see.
const FIFTH: f32 = 0.5;

/// How fast the phase estimate forgets, in seconds. Eight keeps enough beats to average
/// the grid's own quantisation away while still following a track that drifts.
const PHASE_TAU: f64 = 8.0;

/// What the tempo scan settled on. Half and double are kept apart from unrelated rivals
/// because they mean quite different things: a rival at 137 BPM says the evidence is
/// mush, while a rival at half the tempo says the beat is certain and only its name is
/// arguable - and a bar line drawn at half tempo still falls on a real bar line.
struct Winner {
    period: f64,
    score: f32,
    /// Best score from a period bearing no simple ratio to the winner.
    runner: f32,
    /// Best score from a half, double, third, triple or dotted relative of it.
    relative: f32,
}

/// How one candidate period stands to another.
enum Relation {
    /// The same tempo, within the width of the scan's own peak.
    Same,
    /// A half, double, third, triple, or three-to-two relative.
    Metrical,
    Unrelated,
}

/// Classify `lag` against `best`, in octaves.
fn relation(lag: f64, best: f64) -> Relation {
    let d = (lag / best).log2().abs();
    if d < 0.12 {
        Relation::Same
    } else if [0.585, 1.0, 1.585].iter().any(|m| (d - m).abs() < 0.09) {
        Relation::Metrical
    } else {
        Relation::Unrelated
    }
}

/// A tempo and where the bar lines fall.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatGrid {
    pub bpm: f32,
    /// 0.0..=1.0. Below ~0.5 the caller should not trust the grid and will fall back to
    /// transitioning on a plain timer.
    pub confidence: f32,
    /// Seconds per beat, so callers do not recompute it.
    pub beat_seconds: f64,
}

/// Onset history and the current estimate, fed one UI frame at a time.
///
/// `Default` is written out rather than derived because the history arrays are longer
/// than the 32 elements std derives `Default` for.
pub struct BeatTracker {
    /// Onset density per cell over the whole spectrum, indexed by absolute cell modulo
    /// [`CELLS`]. This is what the tempo search reads.
    flux: [f32; CELLS],
    /// 1.0 for a cell some frame actually covered, 0.0 for one skipped by a stall. A
    /// gap left in the history as ordinary zeroes would be a block of identical values
    /// once the mean is taken out, and blocks of identical values correlate beautifully
    /// with themselves at short lags - which is to say a stalled UI would push the
    /// answer towards double tempo.
    seen: [f32; CELLS],
    /// The same flux weighted towards the bottom of the spectrum, which is where a bar
    /// line announces itself; used only to pick which beat starts the bar.
    bass: [f32; CELLS],
    /// Chronological copy the search works over, so the ring's wrap does not have to be
    /// unpicked inside the inner loop. Owned by the tracker to keep the search
    /// allocation-free.
    work: [f32; CELLS],
    /// `work` with the onsets widened, which is what the autocorrelation runs over.
    blur: [f32; CELLS],
    /// `seen`, chronological, so the correlation loop indexes straight lines.
    mask: [f32; CELLS],
    /// Normalised autocorrelation of `blur`, indexed by lag in cells.
    ac: [f32; AC_LAGS],
    /// Previous frame's band levels, for the rise.
    prev: [f32; BANDS],
    /// Whether `prev` holds a usable frame.
    primed: bool,
    /// Track position of the last accepted frame.
    last: f64,
    /// Absolute index of the newest written cell.
    head: i64,
    /// Absolute index of the oldest cell written since the last reset.
    start: i64,
    /// Slow average of the frame RMS, for the silence gate.
    level: f32,
    /// Slow average of the frame rate, in frames per second of track. A beat sampled
    /// three times cannot be located, however loud it is.
    rate: f32,
    /// Next track position at which the search may run.
    due: f64,
    /// Current estimate, if any.
    grid: Option<BeatGrid>,
    /// Beat period in seconds, alongside `grid`.
    period: f64,
    /// A track position on which a beat falls; bar lines are derived from it.
    anchor: f64,
    /// Previous accepted tempo, for the agreement term in the confidence.
    settled: f32,
}

impl Default for BeatTracker {
    fn default() -> Self {
        BeatTracker {
            flux: [0.0; CELLS],
            seen: [0.0; CELLS],
            bass: [0.0; CELLS],
            work: [0.0; CELLS],
            blur: [0.0; CELLS],
            mask: [0.0; CELLS],
            ac: [0.0; AC_LAGS],
            prev: [0.0; BANDS],
            primed: false,
            last: 0.0,
            head: 0,
            start: 0,
            level: 0.0,
            rate: 0.0,
            due: 0.0,
            grid: None,
            period: 0.0,
            anchor: 0.0,
            settled: 0.0,
        }
    }
}

impl BeatTracker {
    /// An empty tracker.
    pub fn new() -> BeatTracker {
        BeatTracker::default()
    }

    /// Feed one measurement. `position` is the track's own clock in seconds; frames may
    /// be irregular, may repeat a position (paused), and may jump backwards (a seek).
    pub fn feed(&mut self, bands: &[f32; BANDS], rms_mono: f32, position: f64) {
        if !position.is_finite() || position < 0.0 {
            return;
        }
        let step = position - self.last;
        if self.primed && !(-SEEK_BACK..=MAX_GAP * 8.0).contains(&step) {
            // A seek: the history describes music that is no longer adjacent to the
            // playhead, and its phase is meaningless against the new clock.
            self.reset();
        }
        if self.primed && step < MIN_STEP {
            return;
        }

        let mut now = [0.0f32; BANDS];
        for (slot, &raw) in now.iter_mut().zip(bands.iter()) {
            *slot = if raw.is_finite() {
                raw.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
        let rms = if rms_mono.is_finite() {
            rms_mono.clamp(0.0, 1.0)
        } else {
            0.0
        };

        let cell = (position / CELL).floor() as i64;
        if !self.primed {
            self.prev = now;
            self.primed = true;
            self.last = position;
            self.head = cell;
            self.start = cell;
            self.due = position + MIN_HISTORY;
            return;
        }

        // Rectified rise, whole spectrum and bass separately. Levels are already
        // logarithmic - the tap normalises dB against a fixed floor - so a plain
        // difference is a ratio, and a quiet passage counts as much as a loud one.
        let mut rise = 0.0f32;
        let mut low = 0.0f32;
        for ((&new, &old), &w) in now.iter().zip(self.prev.iter()).zip(BAR_WEIGHT.iter()) {
            let d = new - old;
            if d > 0.0 {
                rise += d;
                low += d * w;
            }
        }
        self.prev = now;

        // Everything below is in track time, so the smoothing constants mean the same
        // thing whether the UI is redrawing 30 times a second or four.
        let span = step.clamp(MIN_STEP, MAX_GAP);
        let a = (span / 2.0).min(1.0) as f32;
        self.level += (rms - self.level) * a;
        // Weighted by the interval it covers, so this is the mean rate over the last
        // few seconds of track rather than the mean of the intervals - a stall counts
        // for as long as it lasted.
        let b = (span / 6.0).min(1.0) as f32;
        self.rate += ((1.0 / span as f32) - self.rate) * b;

        self.advance(cell);
        if step <= MAX_GAP && rms > SILENCE {
            // As a density, and split across cells by how much of each the interval
            // actually covers. Depositing whole cells instead would leave a comb at the
            // beat frequency between the redraw rate and the grid - 30 Hz frames into
            // 20 ms cells land 1, 2, 1, 2 cells apart, which is a 0.1 s pulse train and
            // its subharmonics are 120 and 60 BPM. That artefact does not care what the
            // music is doing, which is precisely what makes it dangerous.
            let density = 1.0 / step;
            let from = self.head.max(cell - CELLS as i64 + 1);
            for c in from..=cell {
                let lo = (c as f64 * CELL).max(self.last);
                let hi = ((c + 1) as f64 * CELL).min(position);
                if hi <= lo {
                    continue;
                }
                let share = (density * (hi - lo)) as f32;
                let i = ring(c);
                self.flux[i] += rise * share;
                self.bass[i] += low * share;
                self.seen[i] = 1.0;
            }
        }
        self.head = self.head.max(cell);
        self.last = position;

        if position >= self.due {
            self.due = position + SEARCH_EVERY;
            self.search();
        }
    }

    /// The current estimate, or `None` before there is enough evidence.
    pub fn grid(&self) -> Option<BeatGrid> {
        self.grid
    }

    /// Seconds from `position` until the next downbeat (start of a bar of
    /// `beats_per_bar`), or `None` without a trustworthy grid. Always >= 0.
    pub fn next_downbeat_in(&self, position: f64, beats_per_bar: u32) -> Option<f64> {
        let grid = self.grid?;
        if grid.confidence < TRUST || !position.is_finite() || self.period <= 0.0 {
            return None;
        }
        let per_bar = beats_per_bar.clamp(1, MAX_BEATS_PER_BAR);
        let bar = self.period * f64::from(per_bar);
        let downbeat = self.anchor + self.period * f64::from(self.bar_offset(per_bar));
        let phase = (position - downbeat).rem_euclid(bar);
        // `rem_euclid` puts `phase` in [0, bar), so this is in (0, bar]; the full-bar
        // case is exactly on a downbeat, which is no wait at all.
        let wait = bar - phase;
        Some(if wait >= bar { 0.0 } else { wait.max(0.0) })
    }

    /// A new track: forget everything. Must be cheap and leave the tracker usable.
    pub fn reset(&mut self) {
        self.flux = [0.0; CELLS];
        self.seen = [0.0; CELLS];
        self.bass = [0.0; CELLS];
        self.prev = [0.0; BANDS];
        self.primed = false;
        self.last = 0.0;
        self.head = 0;
        self.start = 0;
        self.level = 0.0;
        self.rate = 0.0;
        self.due = 0.0;
        self.grid = None;
        self.period = 0.0;
        self.anchor = 0.0;
        self.settled = 0.0;
    }

    /// Clear the cells between the old head and `cell` so stale audio from a lap of the
    /// ring cannot be read as recent.
    fn advance(&mut self, cell: i64) {
        if cell <= self.head {
            return;
        }
        let first = (self.head + 1).max(cell - CELLS as i64 + 1);
        for c in first..=cell {
            let i = ring(c);
            self.flux[i] = 0.0;
            self.bass[i] = 0.0;
            self.seen[i] = 0.0;
        }
        self.start = self.start.max(cell - CELLS as i64 + 1);
    }

    /// Cells of history currently held.
    fn filled(&self) -> usize {
        (self.head - self.start + 1).clamp(0, CELLS as i64) as usize
    }

    /// Tempo, phase and confidence from the onset history. The one expensive thing in
    /// the module, and the reason [`SEARCH_EVERY`] exists.
    fn search(&mut self) {
        let n = self.filled();
        if (n as f64) * CELL < MIN_HISTORY || self.level < SILENCE {
            self.grid = None;
            return;
        }

        // Chronological, so neither the ring's wrap nor the mask has to be unpicked
        // inside the correlation loop.
        let base = self.head - n as i64 + 1;
        let mut kept = 0.0f32;
        for (i, (slot, m)) in self.work[..n]
            .iter_mut()
            .zip(self.mask[..n].iter_mut())
            .enumerate()
        {
            let c = ring(base + i as i64);
            let v = self.flux[c];
            *slot = v;
            *m = self.seen[c];
            kept += self.seen[c];
        }
        if kept < (MIN_HISTORY / CELL) as f32 {
            self.grid = None;
            return;
        }
        // Take out anything slower than a couple of seconds before looking for a beat.
        // The level of the envelope follows the redraw rate as much as the music: a
        // stretch of 30 Hz frames deposits several times the flux per second that a
        // stretch of 4 Hz frames does, purely because a rise measured over 33 ms and one
        // measured over 250 ms are the same number. Left in, those blocks lift the whole
        // autocorrelation - every lag correlates with every other - and noise starts
        // looking like a beat. A moving average two seconds wide removes them, and being
        // a whole number of beats wide at the slow end of the range it leaves the beat
        // itself alone.
        let mut window = 0.0f32;
        let mut count = 0.0f32;
        for &v in &self.work[..(HIGH_PASS + 1).min(n)] {
            window += v;
            count += 1.0;
        }
        let mut rectified = 0.0f32;
        for i in 0..n {
            if i > HIGH_PASS {
                window -= self.work[i - HIGH_PASS - 1];
                count -= 1.0;
            }
            if i + HIGH_PASS < n {
                window += self.work[i + HIGH_PASS];
                count += 1.0;
            }
            let v = (self.work[i] - window / count.max(1.0)).max(0.0);
            self.blur[i] = v;
            rectified += v;
        }
        let mean = rectified / kept;
        let mut energy = 0.0f32;
        for (i, slot) in self.work[..n].iter_mut().enumerate() {
            *slot = (self.blur[i] - mean) * self.mask[i];
            energy += *slot * *slot;
        }
        if energy <= 1e-9 {
            // Digital silence or a steady tone: nothing rose, so there are no onsets and
            // no honest answer.
            self.grid = None;
            return;
        }

        // Widen every onset to a few cells before correlating. An onset lands in one or
        // two cells, which makes the autocorrelation peaks narrower than the grid: a
        // beat period of 17.6 cells is then measured half way down its own peak while
        // its double at 35.2 sits nearly on top of one, and the tracker reports half the
        // tempo for no better reason than arithmetic. Smoothing costs resolution the
        // comb in `lock` gets back anyway.
        self.blur[0] = self.work[0];
        self.blur[n - 1] = self.work[n - 1];
        let mut blurred = 0.0f32;
        for i in 1..n - 1 {
            let v = 0.25 * self.work[i - 1] + 0.5 * self.work[i] + 0.25 * self.work[i + 1];
            self.blur[i] = v;
            blurred += v * v;
        }
        let scale = blurred.max(1e-12) / kept;

        let lags = AC_LAGS.min(n.saturating_sub(MIN_LAG));
        self.ac.fill(0.0);
        let whole = kept >= n as f32;
        for lag in 1..lags {
            let mut acc = 0.0f32;
            for (i, &v) in self.blur[lag..n].iter().enumerate() {
                acc += v * self.blur[i];
            }
            // Divided by the pairs that carried data, not by the window, so a lag of
            // three seconds is comparable with one of a third of a second and a stall
            // costs every lag the same. Counting them is only worth it when there is
            // actually a gap, which there usually is not.
            let pairs = if whole {
                (n - lag) as f32
            } else {
                let mut p = 0.0f32;
                for (i, &m) in self.mask[lag..n].iter().enumerate() {
                    p += m * self.mask[i];
                }
                p
            };
            self.ac[lag] = acc / (pairs.max(1.0) * scale);
        }

        let Some(win) = self.best_lag(lags) else {
            self.grid = None;
            return;
        };
        let coarse = self.refine(win.period, lags);
        let (period, anchor) = self.lock(n, base, coarse);
        let bpm = (60.0 / (period * CELL)) as f32;
        if !(MIN_BPM..=MAX_BPM).contains(&bpm) {
            self.grid = None;
            return;
        }

        let peak = self.ac_at(period);
        let mut floor = 0.0f32;
        for lag in MIN_LAG..=MAX_LAG.min(lags - 1) {
            floor += self.ac[lag];
        }
        floor /= (MAX_LAG.min(lags - 1) + 1 - MIN_LAG) as f32;

        // How far the winning period stands above the rest of the curve, and how far
        // its score stands above the best unrelated rival. Both are in units of
        // correlation, and both are needed: white noise throws up a tall peak often
        // enough, and a lead over the runner-up often enough, but hardly ever both.
        let strength = ((peak - floor - 0.12) / 0.38).clamp(0.0, 1.0);
        let margin = ((win.score - win.runner - 0.06) / 0.34).clamp(0.0, 1.0);
        // Losing narrowly to half or double costs a third of the confidence, no more.
        // The grid is still right; only the size of the bar is in doubt, and a bar line
        // at half tempo is one of the real ones.
        let level = 0.65 + 0.35 * ((win.score - win.relative) / 0.25).clamp(0.0, 1.0);
        // Frames per beat. Three is the floor below which an onset cannot be placed at
        // all, six is comfortable; 4 Hz redraws never get there and say so.
        let per_beat = self.rate * (period * CELL) as f32;
        let sampled = ((per_beat - 3.0) / 3.0).clamp(0.0, 1.0);
        // Material whose bands barely move has no onsets to track, whatever the
        // autocorrelation of the residue happens to look like.
        let moving = ((mean * (1.0 / CELL as f32) - 0.6) / 1.5).clamp(0.0, 1.0);
        let agree = if self.settled > 0.0 {
            (1.0 - (bpm / self.settled).log2().abs() * 12.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.settled = if self.settled > 0.0 {
            self.settled + (bpm - self.settled) * 0.35
        } else {
            bpm
        };
        // A product, not an average: every one of these is a reason to disbelieve the
        // answer, and one of them being emphatic should not be rescued by the others.
        let confidence =
            strength * margin * level * sampled * moving * (0.4 + 0.6 * agree).min(1.0);

        self.period = period * CELL;
        self.anchor = anchor;
        self.grid = Some(BeatGrid {
            bpm,
            confidence: confidence.clamp(0.0, 1.0),
            beat_seconds: self.period,
        });
    }

    /// Best period in range and what came second, told apart by whether the runner-up is
    /// a metrical relative of the winner or a rival tempo altogether.
    ///
    /// Scoring a candidate on its own correlation plus its harmonics is what stops the
    /// off-beat winning: hats halfway between the beats correlate at half the period,
    /// but only the true period also correlates at twice and three times itself. The
    /// scan is in quarter cells because a beat period is rarely a whole number of them
    /// and the penalty terms are read at fractions of it either way.
    fn best_lag(&self, lags: usize) -> Option<Winner> {
        let top = (MAX_LAG.min(lags.saturating_sub(1)) as f64).min(MAX_PERIOD);
        if top < MIN_PERIOD {
            return None;
        }
        let mut best = (0.0f64, f32::NEG_INFINITY);
        let mut runner = 0.0f32;
        let mut relative = 0.0f32;
        let mut scores = [0.0f32; CANDIDATES];
        for (i, slot) in scores.iter_mut().enumerate() {
            let l = MIN_PERIOD + i as f64 * CANDIDATE_STEP;
            if l > top {
                break;
            }
            let on = self.ac_at(l) + 0.5 * self.ac_at(l * 2.0) + 0.25 * self.ac_at(l * 3.0);
            // Half-way between the beats. If the history repeats there as strongly as it
            // does on the beat, the candidate is two beats long and this is really the
            // bar: exactly the half-tempo error, and the only reliable way to see it.
            //
            // One-sided, and that is the whole of it. Between the beats of an evenly
            // spaced pulse train the correlation goes *negative*, and a penalty allowed
            // to go negative pays a bonus for it - which hands the answer to whichever
            // candidate happens to straddle the gaps, be that double tempo or two and a
            // half times it. Repeating between the beats is evidence against a
            // candidate; not repeating there is not evidence for it.
            let off =
                (self.ac_at(l * 0.5) + 0.5 * self.ac_at(l * 1.5) + 0.25 * self.ac_at(l * 2.5))
                    .max(0.0);
            // Thirds, for the same reason and more weakly, since a triplet subdivision is
            // real music and a candidate three times the beat is not.
            let third = (self.ac_at(l / 3.0) + self.ac_at(l * 2.0 / 3.0)).max(0.0);
            // And fifths. An even sub-multiple is caught by the half-beat term and an
            // odd one is not, so a candidate five pulses long slips through both: every
            // one of its own halves and thirds lands in a gap, and it scores as if it
            // were the fundamental. Nothing in this repertoire is genuinely in five, so
            // this costs real music nothing and closes the hole.
            let fifth = (self.ac_at(l / 5.0) + self.ac_at(l * 2.0 / 5.0)).max(0.0);
            let s = (on - OFFBEAT * off - THIRD * third - FIFTH * fifth) * prior(l);
            *slot = s;
            if s > best.1 {
                best = (l, s);
            }
        }
        if best.1 <= 0.0 {
            return None;
        }
        for (i, &s) in scores.iter().enumerate() {
            let l = MIN_PERIOD + i as f64 * CANDIDATE_STEP;
            match relation(l, best.0) {
                Relation::Same => {}
                Relation::Metrical if s > relative => relative = s,
                Relation::Unrelated if s > runner => runner = s,
                _ => {}
            }
        }
        Some(Winner {
            period: best.0,
            score: best.1,
            runner,
            relative,
        })
    }

    /// Sharpen the scan's winner by combing the autocorrelation itself over four
    /// metrical levels, which locates the peak to a twentieth of a cell where the scan
    /// stops at a quarter.
    fn refine(&self, lag: f64, lags: usize) -> f64 {
        let mut best = (lag, f32::NEG_INFINITY);
        for step in -12i32..=12 {
            let p = lag + f64::from(step) * 0.05;
            if !(MIN_PERIOD..=MAX_PERIOD).contains(&p) {
                continue;
            }
            let mut s = 0.0f32;
            for k in 1..=4 {
                let l = p * f64::from(k);
                if l as usize + 1 >= lags {
                    break;
                }
                s += self.ac_at(l) / k as f32;
            }
            if s > best.1 {
                best = (p, s);
            }
        }
        best.0
    }

    /// Autocorrelation at a fractional lag.
    fn ac_at(&self, lag: f64) -> f32 {
        let i = lag.floor() as usize;
        if i + 1 >= AC_LAGS {
            return 0.0;
        }
        let t = (lag - i as f64) as f32;
        self.ac[i] * (1.0 - t) + self.ac[i + 1] * t
    }

    /// Where the beats fall, and a better period than the autocorrelation alone can
    /// give. Returns the period in cells and the track position of the most recent beat.
    ///
    /// One bin of a discrete transform at the beat frequency gives the coarse offset:
    /// the phase of that bin is where the onset envelope's own periodic component peaks,
    /// which is the beat. A short joint search then slides both the offset and the
    /// period against the envelope itself. The period matters as much as the offset -
    /// an error of half a percent is a whole beat of drift over three minutes, and it is
    /// the comb over sixteen seconds of beats, not the autocorrelation peak, that pins
    /// it down. The anchor is the newest beat rather than the oldest so that
    /// extrapolating forwards has no lever arm to speak of.
    fn lock(&self, n: usize, base: i64, p0: f64) -> (f64, f64) {
        let decay = (-CELL / PHASE_TAU).exp() as f32;
        let turn = std::f64::consts::TAU / p0;
        let (rot_s, rot_c) = turn.sin_cos();
        // Walking the phasor round by one cell each time costs two multiplies instead of
        // a sine and a cosine, which matters at eight hundred cells a search.
        let mut cos = 1.0f64;
        let mut sin = 0.0f64;
        let mut w = 1.0f32;
        let mut re = 0.0f32;
        let mut im = 0.0f32;
        for &v in self.work[..n].iter().rev() {
            let x = v * w;
            re += x * cos as f32;
            im -= x * sin as f32;
            let next_c = cos * rot_c - sin * rot_s;
            sin = sin * rot_c + cos * rot_s;
            cos = next_c;
            w *= decay;
        }
        // The walk went backwards from cell n-1, so the angle is measured from there.
        let last = (n - 1) as f64;
        let offset = if re == 0.0 && im == 0.0 {
            0.0
        } else {
            last + (f64::from(im)).atan2(f64::from(re)) / std::f64::consts::TAU * p0
        };
        let mut beat = offset - (offset - last).div_euclid(p0).max(0.0) * p0;
        while beat > last {
            beat -= p0;
        }

        // Coordinate descent rather than the full grid: the comb score is smooth in both
        // directions and separable enough that two passes land on the same answer for a
        // tenth of the arithmetic.
        let mut period = p0;
        for _ in 0..2 {
            let mut best = (beat, f32::NEG_INFINITY);
            for k in -10i32..=10 {
                let a = beat + f64::from(k) * 0.05;
                if a > last {
                    continue;
                }
                let s = self.comb(n, a, period);
                if s > best.1 {
                    best = (a, s);
                }
            }
            beat = best.0;
            let mut best = (period, f32::NEG_INFINITY);
            for k in -12i32..=12 {
                let p = period + f64::from(k) * 0.04;
                if !(MIN_PERIOD..=MAX_PERIOD).contains(&p) {
                    continue;
                }
                let s = self.comb(n, beat, p);
                if s > best.1 {
                    best = (p, s);
                }
            }
            period = best.0;
        }
        (period, (base as f64 + beat) * CELL)
    }

    /// How well a comb of beats ending at cell `at` and spaced `period` apart lines up
    /// with the onset history, weighted towards the recent end.
    fn comb(&self, n: usize, at: f64, period: f64) -> f32 {
        let per_beat = (-period * CELL / PHASE_TAU).exp() as f32;
        let mut s = 0.0f32;
        let mut w = 1.0f32;
        let mut t = at;
        while t >= 0.0 {
            s += sample(&self.work[..n], t) * w;
            w *= per_beat;
            t -= period;
        }
        s
    }

    /// Which beat after the anchor starts the bar, by voting with the bass onsets: a bar
    /// line is where the kick or the downbeat chord lands, and that lives at the bottom
    /// of the spectrum. With no clear winner this returns 0 and the caller gets a bar
    /// line that is at least a real beat.
    fn bar_offset(&self, per_bar: u32) -> u32 {
        if per_bar <= 1 || self.period <= 0.0 {
            return 0;
        }
        let n = self.filled();
        if n < MIN_LAG {
            return 0;
        }
        let base = self.head - n as i64 + 1;
        let t0 = base as f64 * CELL;
        let t1 = self.head as f64 * CELL;
        let first = ((t0 - self.anchor) / self.period).ceil() as i64;
        let last = ((t1 - self.anchor) / self.period).floor() as i64;
        if last <= first {
            return 0;
        }
        let mut vote = [0.0f32; MAX_BEATS_PER_BAR as usize];
        for k in first..=last {
            let cell = ((self.anchor + k as f64 * self.period) / CELL).round() as i64;
            let mut e = 0.0f32;
            for c in cell - VOTE_SPREAD..=cell + VOTE_SPREAD {
                if c >= base && c <= self.head {
                    e += self.bass[ring(c)];
                }
            }
            let slot = k.rem_euclid(i64::from(per_bar)) as usize;
            if let Some(v) = vote.get_mut(slot) {
                *v += e;
            }
        }
        let mut best = (0u32, 0.0f32);
        for (i, &v) in vote.iter().take(per_bar as usize).enumerate() {
            if v > best.1 {
                best = (i as u32, v);
            }
        }
        best.0
    }
}

/// Ring slot for an absolute cell index.
fn ring(cell: i64) -> usize {
    cell.rem_euclid(CELLS as i64) as usize
}

/// Log-normal pull towards the middle of the tempo range, used only to break ties
/// between metrical levels.
fn prior(lag: f64) -> f32 {
    let bpm = 60.0 / (lag * CELL) as f32;
    let d = (bpm / PRIOR_BPM).log2() / PRIOR_OCTAVES;
    (-0.5 * d * d).exp()
}

/// Linear read of the onset envelope at a fractional cell.
fn sample(env: &[f32], at: f64) -> f32 {
    if at < 0.0 {
        return 0.0;
    }
    let i = at.floor() as usize;
    if i + 1 >= env.len() {
        return env.get(i).copied().unwrap_or(0.0);
    }
    let t = (at - i as f64) as f32;
    env[i] * (1.0 - t) + env[i + 1] * t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift, so every run of the suite sees exactly the same "random" material.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// 0.0..1.0
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Roughly normal, for timing jitter and measurement noise.
        fn wobble(&mut self) -> f64 {
            (0..6).map(|_| self.unit()).sum::<f64>() - 3.0
        }
    }

    /// One measurement, as [`crate::audio_tap::Tap`] would report it.
    #[derive(Clone, Copy)]
    struct Take {
        bands: [f32; BANDS],
        rms: f32,
    }

    /// The tap measures 1024 samples at a time, so about every 21 ms.
    const TAP: f64 = 1024.0 / 48000.0;

    /// dB against a fixed floor, 0.0..=1.0, the way the tap normalises.
    fn level(db: f32, floor: f32) -> f32 {
        if db.is_finite() {
            ((db + floor) / floor).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// A Gaussian bump across the bands: one drum's rough spectrum.
    fn shape(centre: f32, spread: f32, peak: f32) -> [f32; BANDS] {
        let mut g = [0.0f32; BANDS];
        for (i, slot) in g.iter_mut().enumerate() {
            let d = (i as f32 - centre) / spread;
            *slot = peak * (-0.5 * d * d).exp();
        }
        g
    }

    /// Kick on one and three, snare on two and four, eighth-note hats with a little
    /// swing, and a bass note on the downbeat so the bar has something to be found by.
    /// Rendered as band levels one tap window at a time, with timing jitter and the
    /// measurement noise a 21 ms RMS window really has.
    fn pattern(bpm: f64, seconds: f64, seed: u64) -> Vec<Take> {
        let mut rng = Rng::new(seed);
        let beat = 60.0 / bpm;
        let kick = shape(1.5, 1.6, 1.0);
        let snare = shape(7.0, 3.0, 0.55);
        let hat = shape(13.5, 2.0, 0.2);
        let note = shape(4.0, 2.0, 0.35);

        let mut hits: Vec<(f64, [f32; BANDS], f64)> = Vec::new();
        let bars = (seconds / (beat * 4.0)) as i64 + 2;
        for bar in 0..bars {
            for b in 0..4 {
                let at = (bar * 4 + b) as f64 * beat + rng.wobble() * 0.003;
                if b == 0 || b == 2 {
                    let g = if b == 0 { 1.0 } else { 0.72 };
                    let mut k = kick;
                    for v in k.iter_mut() {
                        *v *= g;
                    }
                    hits.push((at, k, 0.07));
                } else {
                    hits.push((at, snare, 0.12));
                }
                if b == 0 {
                    hits.push((at, note, 0.35));
                }
                for eighth in 0..2 {
                    let swing = if eighth == 1 { 0.018 } else { 0.0 };
                    let mut h = hat;
                    for v in h.iter_mut() {
                        *v *= if eighth == 1 { 1.0 } else { 0.7 };
                    }
                    hits.push((at + f64::from(eighth) * beat / 2.0 + swing, h, 0.035));
                }
            }
        }

        let mut out = Vec::new();
        let mut t = 0.0;
        while t < seconds {
            let mut power = [0.0f32; BANDS];
            for (i, p) in power.iter_mut().enumerate() {
                // A quiet pad underneath, so nothing is ever digitally silent.
                *p += match i {
                    1..=4 => 0.004,
                    5..=9 => 0.0015,
                    _ => 0.0004,
                };
            }
            for &(at, gain, decay) in &hits {
                let age = t - at;
                if !(0.0..1.0).contains(&age) {
                    continue;
                }
                let env = (-age / decay).exp() as f32;
                for (p, g) in power.iter_mut().zip(gain.iter()) {
                    *p += g * g * env * env;
                }
            }
            let mut bands = [0.0f32; BANDS];
            let mut total = 0.0f32;
            for (slot, p) in bands.iter_mut().zip(power.iter()) {
                let noise = (1.0 + 0.12 * rng.wobble() as f32).max(0.05);
                let amp = (p.max(1e-9) * noise).sqrt();
                total += amp * amp;
                *slot = level(20.0 * amp.log10(), 64.0);
            }
            out.push(Take {
                bands,
                rms: level(10.0 * total.max(1e-12).log10(), 60.0),
            });
            t += TAP;
        }
        out
    }

    /// Steady white noise at a plausible programme level: plenty of flux, no rhythm.
    fn noise(seconds: f64, seed: u64) -> Vec<Take> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::new();
        let mut t = 0.0;
        while t < seconds {
            let mut bands = [0.0f32; BANDS];
            for slot in bands.iter_mut() {
                let amp = 0.08 * (1.0 + 0.5 * rng.wobble() as f32).abs();
                *slot = level(20.0 * amp.max(1e-9).log10(), 64.0);
            }
            out.push(Take { bands, rms: 0.62 });
            t += TAP;
        }
        out
    }

    /// A held sine: one band lit, nothing rising.
    fn tone(seconds: f64, seed: u64) -> Vec<Take> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::new();
        let mut t = 0.0;
        while t < seconds {
            let mut bands = [0.0f32; BANDS];
            for (i, slot) in bands.iter_mut().enumerate() {
                let amp: f32 = if i == 7 { 0.5 } else { 0.0008 };
                let jitter = (1.0 + 0.02 * rng.wobble() as f32).max(0.01);
                *slot = level(20.0 * (amp * jitter).log10(), 64.0);
            }
            out.push(Take { bands, rms: 0.7 });
            t += TAP;
        }
        out
    }

    /// The UI redrawing at a fixed rate.
    fn steady(seconds: f64, hz: f64) -> Vec<f64> {
        let n = (seconds * hz) as usize;
        (0..n).map(|i| i as f64 / hz).collect()
    }

    /// The realistic case: the redraw rate follows what is on screen, the player is
    /// paused now and then (the same position arriving repeatedly), and sometimes the UI
    /// does not get round to redrawing for a second or two.
    fn ragged(seconds: f64, seed: u64) -> Vec<f64> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::new();
        let mut t = 0.0;
        while t < seconds {
            let hz = [30.0, 30.0, 10.0, 4.0][(rng.next() % 4) as usize];
            let until = (t + 1.0 + rng.unit() * 2.5).min(seconds);
            while t < until {
                out.push(t);
                t += 1.0 / hz;
            }
            match rng.next() % 10 {
                0 => {
                    for _ in 0..6 {
                        out.push(t);
                    }
                }
                1 => t += 0.5 + rng.unit() * 1.5,
                _ => {}
            }
        }
        out.retain(|&x| x < seconds);
        out
    }

    /// Feed a whole take list along a frame schedule.
    fn play(tracker: &mut BeatTracker, takes: &[Take], times: &[f64]) {
        for &t in times {
            let i = ((t / TAP) as usize).min(takes.len() - 1);
            tracker.feed(&takes[i].bands, takes[i].rms, t);
        }
    }

    #[test]
    fn nothing_is_claimed_before_there_is_evidence() {
        let mut tracker = BeatTracker::new();
        assert!(tracker.grid().is_none());
        assert!(tracker.next_downbeat_in(0.0, 4).is_none());

        let takes = pattern(120.0, 4.0, 1);
        play(&mut tracker, &takes, &steady(4.0, 30.0));
        assert!(
            tracker.grid().is_none(),
            "four seconds is not enough history"
        );
    }

    #[test]
    fn steady_frames_find_the_tempo() {
        for &bpm in &[84.0, 97.5, 128.0, 140.0, 174.0] {
            let takes = pattern(bpm, 40.0, 7);
            let mut tracker = BeatTracker::new();
            play(&mut tracker, &takes, &steady(40.0, 30.0));
            let grid = tracker.grid().expect("a grid after forty seconds");
            assert!(
                (f64::from(grid.bpm) - bpm).abs() < 2.0,
                "{bpm} BPM read as {}",
                grid.bpm
            );
            assert!(grid.confidence >= TRUST, "{bpm} BPM: {}", grid.confidence);
            assert!((grid.beat_seconds - 60.0 / f64::from(grid.bpm)).abs() < 1e-6);
        }
    }

    #[test]
    fn ragged_frames_find_the_tempo() {
        for &bpm in &[97.5, 128.0, 140.0] {
            let takes = pattern(bpm, 45.0, 11);
            let mut tracker = BeatTracker::new();
            play(&mut tracker, &takes, &ragged(45.0, 3));
            let grid = tracker.grid().expect("a grid after forty-five seconds");
            assert!(
                (f64::from(grid.bpm) - bpm).abs() < 3.0,
                "{bpm} BPM read as {} from ragged frames",
                grid.bpm
            );
        }
    }

    #[test]
    fn the_downbeat_is_ahead_and_within_one_bar() {
        let takes = pattern(128.0, 40.0, 13);
        let mut tracker = BeatTracker::new();
        let times = steady(40.0, 30.0);
        let mut answered = 0;
        for &t in &times {
            let i = ((t / TAP) as usize).min(takes.len() - 1);
            tracker.feed(&takes[i].bands, takes[i].rms, t);
            for bar in [0u32, 1, 3, 4, 7, u32::MAX] {
                if let Some(wait) = tracker.next_downbeat_in(t, bar) {
                    // Measured against the tracker's own bar, which is the only one it
                    // ever promised: 128 BPM is what the material is, not what it read.
                    let beats = f64::from(bar.clamp(1, MAX_BEATS_PER_BAR));
                    let own = tracker.grid().map_or(0.0, |g| g.beat_seconds) * beats;
                    assert!(wait >= 0.0, "negative wait {wait}");
                    assert!(wait <= own + 1e-9, "wait {wait} past a bar of {own}");
                    answered += 1;
                }
            }
            // Asking about a moment other than the last frame is allowed.
            if let Some(wait) = tracker.next_downbeat_in(t + 0.37, 4) {
                assert!(wait >= 0.0);
            }
        }
        assert!(answered > 0, "never answered at all");
    }

    #[test]
    fn the_downbeat_lands_on_a_bar_line() {
        let bpm = 128.0;
        let bar = 60.0 / bpm * 4.0;
        let takes = pattern(bpm, 60.0, 17);
        let mut tracker = BeatTracker::new();
        let mut worst: f64 = 0.0;
        let mut asked = 0;
        for &t in &steady(60.0, 30.0) {
            let i = ((t / TAP) as usize).min(takes.len() - 1);
            tracker.feed(&takes[i].bands, takes[i].rms, t);
            if t < 20.0 {
                continue;
            }
            if let Some(wait) = tracker.next_downbeat_in(t, 4) {
                let landing = t + wait;
                worst = worst.max((landing - (landing / bar).round() * bar).abs());
                asked += 1;
            }
        }
        assert!(asked > 500, "only answered {asked} times");
        assert!(worst < 0.08, "worst bar-line error {worst:.3}s");
    }

    #[test]
    fn silence_noise_and_a_held_tone_are_not_beats() {
        let quiet: Vec<Take> = (0..2000)
            .map(|_| Take {
                bands: [0.0; BANDS],
                rms: 0.0,
            })
            .collect();
        for (name, takes) in [
            ("silence", quiet),
            ("noise", noise(45.0, 23)),
            ("tone", tone(45.0, 29)),
        ] {
            for times in [steady(45.0, 30.0), ragged(45.0, 5)] {
                let mut tracker = BeatTracker::new();
                let mut worst = 0.0f32;
                for &t in &times {
                    let i = ((t / TAP) as usize).min(takes.len() - 1);
                    tracker.feed(&takes[i].bands, takes[i].rms, t);
                    if let Some(grid) = tracker.grid() {
                        worst = worst.max(grid.confidence);
                    }
                    assert!(
                        tracker.next_downbeat_in(t, 4).is_none(),
                        "{name} produced a downbeat"
                    );
                }
                assert!(worst < TRUST, "{name} reached confidence {worst}");
            }
        }
    }

    #[test]
    fn a_seek_forgets_the_old_phase() {
        let takes = pattern(128.0, 40.0, 31);
        let mut tracker = BeatTracker::new();
        play(&mut tracker, &takes, &steady(40.0, 30.0));
        assert!(tracker.grid().is_some());

        // Back to the top of the track: nothing measured before is adjacent to the
        // playhead any more.
        tracker.feed(&takes[0].bands, takes[0].rms, 2.0);
        assert!(tracker.grid().is_none());
        assert!(tracker.next_downbeat_in(2.0, 4).is_none());

        let times: Vec<f64> = steady(40.0, 30.0).iter().map(|t| t + 2.0).collect();
        play(&mut tracker, &takes, &times);
        assert!(tracker.grid().is_some(), "did not re-lock after a seek");
    }

    #[test]
    fn a_pause_changes_nothing() {
        let takes = pattern(140.0, 40.0, 37);
        let mut tracker = BeatTracker::new();
        play(&mut tracker, &takes, &steady(40.0, 30.0));
        let before = tracker.grid().expect("a grid");

        // Paused: the same position arrives over and over.
        for _ in 0..200 {
            tracker.feed(&takes[0].bands, takes[0].rms, 40.0 - 1.0 / 30.0);
        }
        assert_eq!(tracker.grid(), Some(before));
    }

    #[test]
    fn reset_leaves_it_usable() {
        let takes = pattern(128.0, 40.0, 41);
        let mut tracker = BeatTracker::new();
        play(&mut tracker, &takes, &steady(40.0, 30.0));
        assert!(tracker.grid().is_some());

        tracker.reset();
        assert!(tracker.grid().is_none());
        assert!(tracker.next_downbeat_in(0.0, 4).is_none());

        play(&mut tracker, &takes, &steady(40.0, 30.0));
        let grid = tracker.grid().expect("a grid after being reused");
        assert!((grid.bpm - 128.0).abs() < 2.0);
    }

    #[test]
    fn nonsense_input_is_survivable() {
        let mut tracker = BeatTracker::new();
        let bad = [f32::NAN; BANDS];
        let inf = [f32::INFINITY; BANDS];
        let huge = [1e30f32; BANDS];
        for (i, position) in [
            0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -5.0,
            1e12,
            0.5,
            0.5,
            0.5,
            0.4,
            1e9,
            0.0,
        ]
        .iter()
        .enumerate()
        {
            let bands = match i % 3 {
                0 => &bad,
                1 => &inf,
                _ => &huge,
            };
            tracker.feed(bands, f32::NAN, *position);
            let _ = tracker.grid();
            let _ = tracker.next_downbeat_in(*position, 0);
            let _ = tracker.next_downbeat_in(f64::NAN, u32::MAX);
        }
        assert!(tracker.grid().is_none());
    }
}

#[cfg(test)]
mod lead_verification {
    //! Written by the caller rather than the module's author, and deliberately not
    //! sharing its fixtures: the question here is not "does it agree with itself" but
    //! "would I cut a song on this".
    use super::*;

    /// A crude but honest stand-in for a track: a kick on every beat and hats between,
    /// both with a decay rather than an instant, a little noise, and the frames arriving
    /// at whatever rate the UI happened to redraw.
    ///
    /// The decay matters. A kick modelled as a single 30 ms spike is aliased to nonsense
    /// by a 30 Hz sampler and would be testing the fixture rather than the tracker; real
    /// percussion rings for a hundred milliseconds or so, which is what the band levels
    /// this module reads actually see.
    fn play(bpm: f64, seconds: f64, ragged: bool, tracker: &mut BeatTracker) {
        let beat = 60.0 / bpm;
        let mut position = 0.0;
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / 16_777_216.0
        };
        while position < seconds {
            let bar = beat * 4.0;
            let since_beat = (position % beat) as f32;
            let since_off = ((position + beat / 2.0) % beat) as f32;
            // A snare on two and four, because a kick-and-hat pattern alone is a weaker
            // rhythm than any real record has and makes this an unfairly hard fixture.
            let since_snare = ((position + bar / 2.0 - beat) % (bar / 2.0)) as f32;
            let kick = (-since_beat / 0.12).exp();
            let hat = (-since_off / 0.05).exp();
            let snare = (-since_snare / 0.09).exp();
            let mut bands = [0.0f32; 16];
            for (i, band) in bands.iter_mut().enumerate() {
                let noise = rand() * 0.04;
                *band = match i {
                    0..=3 => 0.10 + 0.80 * kick + noise,
                    6..=9 => 0.10 + 0.55 * snare + noise,
                    11..=15 => 0.08 + 0.45 * hat + 0.30 * snare + noise,
                    _ => 0.12 + 0.10 * kick + noise,
                };
            }
            let rms = bands.iter().sum::<f32>() / 16.0;
            tracker.feed(&bands, rms, position);
            // 30 Hz, 10 Hz or 4 Hz, as the real player redraws.
            let step = if ragged {
                match (rand() * 3.0) as u32 {
                    0 => 1.0 / 30.0,
                    1 => 0.1,
                    _ => 0.25,
                }
            } else {
                1.0 / 30.0
            };
            position += step;
        }
    }

    /// The property the player actually depends on: whatever the tracker reports, either
    /// it is a real metrical level of the tune, or it is not believed.
    ///
    /// Deliberately not "it always gets the tempo right". This fixture is a crude
    /// stand-in and the module's own report is candid that heavily limited masters defeat
    /// it - it names the half, or refuses. That is fine, because the automix falls back
    /// to a plain timer. What is not fine is a confident wrong answer, because that cuts
    /// a song in the middle of a phrase, so that is what is asserted here.
    #[test]
    fn it_is_either_right_or_unconvinced_but_never_confidently_wrong() {
        let believable = |grid: BeatGrid, bpm: f64| {
            let ratio = f64::from(grid.bpm) / bpm;
            // Half, double, and the triplet levels a 3/4 feel produces are all real
            // answers to "where are the bar lines".
            [0.25, 1.0 / 3.0, 0.5, 1.0, 2.0, 3.0, 4.0]
                .iter()
                .any(|m| (ratio - m).abs() < 0.05)
        };
        let mut confident_and_right = 0;
        for &bpm in &[84.0, 97.5, 128.0, 140.0, 174.0] {
            for ragged in [false, true] {
                let mut tracker = BeatTracker::new();
                play(bpm, 40.0, ragged, &mut tracker);
                let Some(grid) = tracker.grid() else { continue };
                if grid.confidence <= 0.5 {
                    continue; // shrugging is always allowed
                }
                assert!(
                    believable(grid, bpm),
                    "{bpm} BPM (ragged: {ragged}) read as {:.1} and believed at {:.2} - \
                     a confident wrong answer is the one failure that matters here",
                    grid.bpm,
                    grid.confidence
                );
                if (f64::from(grid.bpm) / bpm - 1.0).abs() < 0.05 {
                    confident_and_right += 1;
                }
            }
        }
        // ...and it must not be uselessly shy either, or beat alignment would never fire.
        //
        // Four of ten is a deliberately low bar and this fixture is why: it is a drum
        // machine, not a record. Measured against real audio put through the player's own
        // filter chain the tracker does far better (five tempos inside 0.15%), and
        // against heavily limited commercial masters far worse - it names the half, or
        // refuses. Both are fine, because the automix falls back to its timer. The number
        // here exists to catch a regression that makes it refuse *everything*.
        //
        // It was three when this was written. An earlier version of the tracker let its
        // octave-breaking penalties go negative, which paid a bonus to whichever wrong
        // candidate straddled the gaps between beats - this fixture is what caught it,
        // and 174 BPM read as 69.5 is what it looked like.
        assert!(
            confident_and_right >= 4,
            "only {confident_and_right} of ten runs found the tempo outright - beat \
             alignment would almost never engage"
        );
    }

    #[test]
    fn it_refuses_to_invent_one() {
        // The failure that matters: a confident wrong answer makes the player cut a song
        // in the middle of a phrase. Silence must not be 128 BPM.
        let mut seed = 12_345u64;
        let mut rand = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as f32 / 2_147_483_648.0
        };
        for (name, make) in [("silence", 0usize), ("white noise", 1), ("a held tone", 2)] {
            let mut tracker = BeatTracker::new();
            let mut position = 0.0;
            while position < 40.0 {
                let bands = match make {
                    0 => [0.0f32; 16],
                    1 => std::array::from_fn(|_| rand()),
                    _ => std::array::from_fn(|i| if i == 5 { 0.7 } else { 0.02 }),
                };
                let rms = bands.iter().sum::<f32>() / 16.0;
                tracker.feed(&bands, rms, position);
                position += 1.0 / 30.0;
            }
            let confidence = tracker.grid().map_or(0.0, |g| g.confidence);
            assert!(
                confidence < 0.5,
                "{name} was read as a beat with confidence {confidence:.2} - the player \
                 would cut a song on that"
            );
        }
    }

    #[test]
    fn the_next_downbeat_is_always_ahead_and_inside_a_bar() {
        let mut tracker = BeatTracker::new();
        play(128.0, 40.0, false, &mut tracker);
        let grid = tracker
            .grid()
            .expect("a grid after forty seconds of four-to-the-floor");
        let bar = grid.beat_seconds * 4.0;
        for step in 0..200 {
            let position = 40.0 + f64::from(step) * 0.05;
            let Some(ahead) = tracker.next_downbeat_in(position, 4) else {
                panic!("no downbeat from a confident grid");
            };
            assert!(ahead >= 0.0, "a downbeat in the past: {ahead}");
            assert!(
                ahead <= bar + 1e-6,
                "the next downbeat is more than a bar away: {ahead} > {bar}"
            );
        }
    }
}
