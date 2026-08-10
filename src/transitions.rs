//! Transitions, written as scores rather than as code.
//!
//! A transition used to be a `match` arm with two gain curves in it. That was enough while
//! there were five of them and each was one idea; it stops being enough the moment anyone wants
//! to write down what a DJ actually does, which is a sequence of moves on named controls at
//! counted bars: kill the bass here, sweep the top away there, wait four bars, drop.
//!
//! So a transition here is data. A [`Recipe`] is a list of [`Lane`]s; a lane automates one
//! [`Param`] on one [`Deck`] through a list of [`Key`]s; a key says what the value is at a
//! moment, and how it travels there from the one before. Adding a transition is adding a
//! `Recipe` to [`RECIPES`] - no new match arms, no new maths, and nothing else in the player
//! needs to know it exists. Combining one with the user's own effects already works, because
//! what comes out is an `af` chain and chains concatenate.
//!
//! Three things the design has to respect, all learned the hard way.
//!
//! Overlapping tracks are uncorrelated, so their powers add: `out² + in²` is the quantity that
//! has to stay near 1 for a *blend*. [`Ease::PowerDown`] and [`Ease::PowerUp`] are cos and sin
//! of the eased angle for exactly that reason - bending the angle changes how fast the swap
//! happens while the Pythagorean identity holds the sum at unity, where bending the gains
//! directly puts a hole in the middle. Not every transition wants that (a long blend
//! deliberately empties out before the drop), so a recipe declares whether it is one.
//!
//! A filter string costs a filter-graph rebuild on the deck it lands on, mid-playback, so
//! everything continuous is quantised before it is formatted: a sweep moves in a handful of
//! steps that nobody hears as steps, and the automix re-applies it a handful of times instead
//! of thirty times a second.
//!
//! And mpv accepts an `af` string it cannot parse, over IPC, with `"error":"success"` - no
//! error, no log, no effect. Every filter this module can emit is therefore built from a small
//! fixed vocabulary that is checked against a real mpv, rather than formatted freely.

// ---------------------------------------------------------------------------
// The vocabulary a score is written in
// ---------------------------------------------------------------------------

/// Which deck a lane is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Deck {
    /// The track that is leaving.
    Outgoing,
    /// The track that is arriving.
    Incoming,
}

/// What a lane automates.
///
/// Deliberately a small set of things a hand reaches for on a mixer, not a general filter
/// interface. Anything expressible here is expressible safely; anything not is a change to this
/// enum and its one formatting function, which is where the mpv syntax is checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Param {
    /// Channel fader, 0.0..=1.0, multiplying the user's own volume.
    Gain,
    /// Low shelf, in dB. `0` is flat and `-40` is gone.
    Bass,
    /// Midrange bell, in dB.
    Mid,
    /// High shelf, in dB.
    High,
    /// Lowpass corner in Hz. `0` means no lowpass at all.
    Lowpass,
    /// Highpass corner in Hz. `0` means none. The other half of a filter move, and the
    /// one that lets a track arrive with its bottom trimmed or leave as nothing but hats.
    Highpass,
    /// Absolute playback rate, `1.0` being the file's own, applied on top of any tempo
    /// match. Below `1.0` the pitch falls with it, which is the entire point: this is
    /// what a brake or a tape stop is, and correcting the pitch would remove the effect.
    Speed,
    /// How much of its own tempo the deck is playing at: `0.0` is fully matched to the other
    /// deck, `1.0` is the track's own speed. Relative because the ratio between two tracks is
    /// not known until they are chosen, and a score has to be written before that.
    Tempo,
    /// A tape-delay echo's feedback, `0.0`..=`1.0`. `0` is no echo at all - the filter is
    /// left out rather than run at a decay of zero - and the delay time is fixed at a
    /// deliberately unmusical 350 ms, so the repeats read as an effect rather than as
    /// this track's own tempo lying about itself. What rides is the *feedback*, which is
    /// the difference between an echo and the top-end lift this used to be: the lift
    /// could only ever switch on or off, and an echo whose tail cannot be ridden down is
    /// worse than none.
    Echo,
}

impl Param {
    /// The value a deck has when no lane touches this parameter.
    fn neutral(self) -> f64 {
        match self {
            Param::Gain => 1.0,
            Param::Bass | Param::Mid | Param::High | Param::Lowpass | Param::Highpass => 0.0,
            Param::Tempo | Param::Speed => 1.0,
            Param::Echo => 0.0,
        }
    }
}

/// When a key happens.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum At {
    /// A bar of the transition, counted from its start. Bars are what the move is actually
    /// written in - "wait four, then drop" is four bars, not 0.25 of anything - and the automix
    /// supplies them from the beat grid. Without a grid they degrade to equal fractions of the
    /// transition, which is the same shape at the wrong tempo rather than a broken one.
    Bar(f64),
    /// A fraction of the whole transition, for moves that genuinely do not count.
    Frac(f64),
}

/// How a value travels from this key to the next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ease {
    /// Straight line.
    Linear,
    /// Slow at both ends. The default for anything a hand would do.
    Smooth,
    /// Hold this value, then jump at the next key. A drop is a hold and a jump.
    Hold,
    /// `cos` of the eased angle: a gain leaving, half of a constant-power pair.
    PowerDown,
    /// `sin` of the eased angle: a gain arriving, the other half.
    PowerUp,
}

/// The shape of the transition's own clock, applied on top of whatever score is running.
///
/// Not a sixth [`Ease`], and not a per-score choice: a score says *what* happens and in
/// what order, and this says *when* along the way it happens - so one setting restyles
/// every transition at once, the plain fade included, without a single `.mix` file
/// knowing it exists.
///
/// It works by warping the clock rather than the values, which is the whole reason it is
/// safe to apply to everything. A score's `power-down`/`power-up` pair is `cos`/`sin` of
/// one angle, so warping the angle moves both together and `out² + in²` stays pinned at
/// one whatever the curve; a `hold` still holds, because holding ignores the clock; and a
/// drop still drops on the same key it was written on. Warping the *values* instead would
/// break all three, which is why nothing here touches them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Curve {
    /// No warp at all: every score plays exactly as it is written.
    #[default]
    Linear,
    /// Slow at both ends, quickest through the middle - what a hand on a fader does
    /// when it is paying attention.
    Smooth,
    /// A real cubic Bézier, the `(0.25, 0.1) (0.25, 1.0)` one every UI toolkit calls
    /// "ease": a longer hang at the start than [`Curve::Smooth`], and a decisive finish.
    Bezier,
    /// Behind the score throughout: the old track is held on to, then handed over
    /// quickly at the end. The mix commits late.
    Late,
    /// Ahead of the score throughout: the handover happens early and the new track is
    /// left to play out. The mix commits early.
    Early,
}

impl Curve {
    /// In the order the setting cycles.
    pub const ALL: [Curve; 5] = [
        Curve::Linear,
        Curve::Smooth,
        Curve::Bezier,
        Curve::Late,
        Curve::Early,
    ];

    /// Stable config token. Never change one: it is in config files.
    pub fn key(self) -> &'static str {
        match self {
            Curve::Linear => "linear",
            Curve::Smooth => "smooth",
            Curve::Bezier => "bezier",
            Curve::Late => "late",
            Curve::Early => "early",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Curve::Linear => "Linear",
            Curve::Smooth => "Smooth",
            Curve::Bezier => "Bezier",
            Curve::Late => "Late",
            Curve::Early => "Early",
        }
    }

    /// One line for the settings row.
    pub fn note(self) -> &'static str {
        match self {
            Curve::Linear => "every score exactly as written",
            Curve::Smooth => "eased at both ends, quickest in the middle",
            Curve::Bezier => "a long hang, then a decisive finish",
            Curve::Late => "hold the old track, then hand over quickly",
            Curve::Early => "hand over early, let the new track play out",
        }
    }

    pub fn parse(word: &str) -> Option<Curve> {
        Curve::ALL
            .iter()
            .copied()
            .find(|curve| curve.key().eq_ignore_ascii_case(word.trim()))
    }

    pub fn cycle(self, forward: bool) -> Curve {
        let at = Curve::ALL.iter().position(|c| *c == self).unwrap_or(0);
        let len = Curve::ALL.len();
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        Curve::ALL[next]
    }

    /// Where in the score to read, at `p` of the way through the transition.
    ///
    /// Every curve is pinned at both ends (`w(0) = 0`, `w(1) = 1`) and rises the whole
    /// way between them. Both of those matter: a curve that did not reach `1` would
    /// leave the score unfinished with the old track still audible, and one that fell
    /// anywhere would run a fade backwards.
    pub fn warp(self, p: f64) -> f64 {
        let p = if p.is_finite() {
            p.clamp(0.0, 1.0)
        } else {
            0.0
        };
        match self {
            Curve::Linear => p,
            Curve::Smooth => p * p * (3.0 - 2.0 * p),
            Curve::Bezier => bezier_ease(p),
            Curve::Late => p * p,
            Curve::Early => 1.0 - (1.0 - p) * (1.0 - p),
        }
    }

    /// The other direction: how far through the transition to be, to be reading `y` of
    /// the score. Used wherever a *score* position has to become a *real* one - which
    /// bar the decks trade places on, and how long before the join that lands.
    ///
    /// By bisection rather than algebra: every curve here rises the whole way, so
    /// halving the interval always converges, and one implementation covers a cubic
    /// Bézier as readily as a square. Fifty steps put it well inside a millisecond of
    /// even the longest fade.
    pub fn unwarp(self, y: f64) -> f64 {
        let y = if y.is_finite() {
            y.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self == Curve::Linear {
            return y;
        }
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for _ in 0..50 {
            let mid = 0.5 * (lo + hi);
            if self.warp(mid) < y {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }
}

/// The `(0.25, 0.1) (0.25, 1.0)` cubic Bézier, solved properly rather than approximated
/// with something that merely looks like it.
///
/// A Bézier is parametric: `x` and `y` are both functions of a parameter that is neither
/// of them, so reading it at a given `x` means solving for that parameter first. Newton
/// converges in a handful of steps from a sensible guess; the bisection fallback is for
/// the flat stretches where the derivative is small enough for Newton to step badly.
fn bezier_ease(x: f64) -> f64 {
    const X1: f64 = 0.25;
    const Y1: f64 = 0.1;
    const X2: f64 = 0.25;
    const Y2: f64 = 1.0;
    // B(t) for the one-dimensional cubic with endpoints pinned at 0 and 1.
    let curve = |t: f64, a: f64, b: f64| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    let slope = |t: f64, a: f64, b: f64| {
        let u = 1.0 - t;
        3.0 * u * u * a + 6.0 * u * t * (b - a) + 3.0 * t * t * (1.0 - b)
    };
    if x <= 0.0 || x >= 1.0 {
        return x.clamp(0.0, 1.0);
    }
    let mut t = x;
    for _ in 0..8 {
        let error = curve(t, X1, X2) - x;
        if error.abs() < 1e-7 {
            return curve(t, Y1, Y2);
        }
        let d = slope(t, X1, X2);
        if d.abs() < 1e-9 {
            break;
        }
        t = (t - error / d).clamp(0.0, 1.0);
    }
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..40 {
        t = 0.5 * (lo + hi);
        if curve(t, X1, X2) < x {
            lo = t;
        } else {
            hi = t;
        }
    }
    curve(0.5 * (lo + hi), Y1, Y2)
}

/// One point on a lane.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Key {
    pub at: At,
    pub value: f64,
    /// How the value travels from here to the next key. The last key's ease is unused.
    pub ease: Ease,
}

/// One parameter of one deck, automated.
#[derive(Clone, PartialEq, Debug)]
pub struct Lane {
    pub deck: Deck,
    pub param: Param,
    /// In time order. Out-of-order keys are a bug in the score, and the reader refuses one
    /// rather than letting the evaluator read the wrong pair at runtime.
    pub keys: Vec<Key>,
}

/// Something a transition does at a moment that is not automation.
///
/// The lanes cover what a mixer's controls do; a hook covers everything else - reaching
/// into the effects rack, saying something on screen - so that a score can express a whole
/// move rather than only the part of it that is a fader.
#[derive(Clone, PartialEq, Debug)]
pub struct Hook {
    pub at: At,
    pub action: HookAction,
}

/// What a hook does when it fires. Each fires once per transition, on the way past.
#[derive(Clone, PartialEq, Debug)]
pub enum HookAction {
    /// Switch one of the effects rack's entries on or off, by name.
    Effect { name: String, on: bool },
    /// Say something in the status bar.
    Toast(String),
    /// A-B loop `deck` over the `bars` before the moment this fires, or cancel a loop
    /// when `bars` is `None`. `deck` is only `None` alongside it, for `loop off` - which
    /// deck was rolling is exactly the thing a score should not have to keep track of.
    /// Playback jumping backwards rather than a control moving is why this is a hook and
    /// not a lane: a roll into a drop is usually two or three of these in a row, each a
    /// smaller fraction than the last, ending in the one that turns it off.
    Loop {
        deck: Option<Deck>,
        bars: Option<f64>,
    },
}

/// A transition, written down.
///
/// Owned rather than borrowed because these are read from files at startup, not compiled
/// in: the built-ins ship as text too, so there is exactly one format and one reader, and
/// the examples a user copies are the real thing rather than a rewrite of it.
#[derive(Clone, PartialEq, Debug)]
pub struct Recipe {
    /// Stable token, taken from the file name. Never change one: it is in config files.
    pub key: String,
    pub label: String,
    /// One line for the settings menu.
    pub note: String,
    /// Length in bars, when the move is counted rather than timed. The automix stretches the
    /// configured overlap to this many bars of the outgoing track when it has a grid.
    pub bars: Option<f64>,
    /// The swap wants to land on a bar line.
    pub beat_aligned: bool,
    /// The incoming deck should be pulled to the outgoing deck's tempo before it starts.
    pub tempo_match: bool,
    /// Both decks are audible throughout and the pair should hold constant power. False for
    /// moves that deliberately empty out - a long blend is quiet in the middle on purpose.
    ///
    /// Read only by the test that enforces it. That is the point of writing it down: the
    /// invariant is a property of the score, so the score is where it is declared, and a
    /// new recipe that forgets to think about loudness fails the test rather than the ear.
    #[cfg_attr(not(test), allow(dead_code))]
    pub constant_power: bool,
    pub lanes: Vec<Lane>,
    pub hooks: Vec<Hook>,
    /// Where it was read from, or empty for one that shipped with the player. Shown in the
    /// settings menu so it is obvious which transitions are yours.
    pub origin: String,
}

// ---------------------------------------------------------------------------
// Evaluating a score
// ---------------------------------------------------------------------------

/// Where the transition is, in both of the units a score can be written in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Clock {
    /// Progress through the whole transition, 0.0..=1.0.
    pub t: f64,
    /// Bars elapsed since it started. Supplied by the automix from the beat grid; when there is
    /// no grid it passes `t * bars`, which puts the moves in the right order at the wrong tempo.
    pub bar: f64,
}

/// What the two decks should be doing at one instant.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Shape {
    /// 0.0..=1.0 multipliers on each deck's user volume.
    pub outgoing_gain: f64,
    pub incoming_gain: f64,
    /// An mpv `af` chain for each deck, or `None` to leave that deck's chain alone. Stable
    /// across a transition: re-apply only when the string changes.
    pub outgoing_filter: Option<String>,
    pub incoming_filter: Option<String>,
    /// How much of its own tempo each deck should play at: `0.0` fully matched to the other,
    /// `1.0` its own. The automix converts these using the ratio it measured.
    pub outgoing_tempo: f64,
    pub incoming_tempo: f64,
    /// Absolute rate for each deck, `1.0` being the track's own, on top of the tempo
    /// match. Anything else means the pitch moves with it.
    pub outgoing_speed: f64,
    pub incoming_speed: f64,
}

/// Read one lane at `clock`.
fn value_at(lane: &Lane, clock: Clock) -> f64 {
    let Some(first) = lane.keys.first() else {
        return lane.param.neutral();
    };
    let position = |at: At| match at {
        At::Bar(bar) => bar,
        At::Frac(frac) => frac,
    };
    let now = |at: At| match at {
        At::Bar(_) => clock.bar,
        At::Frac(_) => clock.t,
    };
    if now(first.at) <= position(first.at) {
        return first.value;
    }
    for pair in lane.keys.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (from, to) = (position(a.at), position(b.at));
        let here = now(b.at);
        if here >= to {
            continue;
        }
        let span = to - from;
        let progress = if span <= 0.0 {
            1.0
        } else {
            ((here - from) / span).clamp(0.0, 1.0)
        };
        return travel(a.value, b.value, progress, a.ease);
    }
    lane.keys
        .last()
        .map_or(lane.param.neutral(), |key| key.value)
}

/// One value's journey to the next, under an easing.
fn travel(from: f64, to: f64, progress: f64, ease: Ease) -> f64 {
    let p = progress.clamp(0.0, 1.0);
    match ease {
        // Endpoints are pinned rather than computed: `cos(PI/2)` is 6e-17, and a deck left at
        // 0.99999999 of the user's volume at the end of a transition is a bug nobody can see.
        _ if p <= 0.0 => from,
        _ if p >= 1.0 => to,
        Ease::Hold => from,
        Ease::Linear => from + (to - from) * p,
        Ease::Smooth => from + (to - from) * (p * p * (3.0 - 2.0 * p)),
        Ease::PowerDown => {
            let angle = p * std::f64::consts::FRAC_PI_2;
            from + (to - from) * (1.0 - angle.cos())
        }
        Ease::PowerUp => {
            let angle = p * std::f64::consts::FRAC_PI_2;
            from + (to - from) * angle.sin()
        }
    }
}

/// Everything one deck is doing, before it is turned into a filter chain.
#[derive(Clone, Copy)]
struct DeckState {
    gain: f64,
    bass: f64,
    mid: f64,
    high: f64,
    lowpass: f64,
    highpass: f64,
    tempo: f64,
    speed: f64,
    echo: f64,
}

impl DeckState {
    fn read(recipe: &Recipe, deck: Deck, clock: Clock) -> DeckState {
        let read = |param: Param| {
            recipe
                .lanes
                .iter()
                .find(|lane| lane.deck == deck && lane.param == param)
                .map_or(param.neutral(), |lane| value_at(lane, clock))
        };
        DeckState {
            gain: read(Param::Gain).clamp(0.0, 1.0),
            bass: read(Param::Bass),
            mid: read(Param::Mid),
            high: read(Param::High),
            lowpass: read(Param::Lowpass),
            highpass: read(Param::Highpass),
            tempo: read(Param::Tempo).clamp(0.0, 1.0),
            // mpv stalls below about 0.05 rather than playing very slowly, so a brake
            // stops there. It is inaudible as pitch by then anyway.
            speed: read(Param::Speed).clamp(0.05, 2.0),
            echo: read(Param::Echo).clamp(0.0, 1.0),
        }
    }

    /// The deck's `af` chain, or `None` when nothing is being done to it.
    ///
    /// Every number is quantised before it is formatted, so the string this returns changes a
    /// handful of times across a transition rather than on every frame - which matters because
    /// each change rebuilds mpv's filter graph mid-playback.
    fn filter(self) -> Option<String> {
        let bass = quantise_db(self.bass);
        let mid = quantise_db(self.mid);
        let high = quantise_db(self.high);
        let lowpass = quantise_hz(self.lowpass);
        let highpass = quantise_hz(self.highpass);
        let echo = quantise_percent(self.echo);
        if bass == 0 && mid == 0 && high == 0 && lowpass == 0 && highpass == 0 && echo == 0 {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        if bass != 0 {
            parts.push(format!("bass=g={bass}:f=200"));
        }
        if mid != 0 {
            parts.push(format!("equalizer=f=1200:t=h:w=1600:g={mid}"));
        }
        if high != 0 {
            parts.push(format!("treble=g={high}:f=4000"));
        }
        if lowpass != 0 {
            parts.push(format!("lowpass=f={lowpass}"));
        }
        if highpass != 0 {
            parts.push(format!("highpass=f={highpass}"));
        }
        if echo != 0 {
            // Fixed in-gain, out-gain and delay: what a score rides is the feedback, the
            // one number that turns a single slapback into a tail that goes on repeating
            // after the track itself has faded under it. Capped under 1.0 - `aecho` builds
            // up without bound at or past it, which is a runaway, not an effect.
            let decay = f64::from(echo.min(95)) / 100.0;
            parts.push(format!("aecho=0.8:0.7:350:{decay:.2}"));
        }
        let mut chain = String::from("lavfi=[");
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                chain.push(',');
            }
            chain.push_str(part);
        }
        chain.push(']');
        Some(chain)
    }
}

/// dB to a 4 dB step, floored at -40 where everything is inaudible anyway.
fn quantise_db(db: f64) -> i32 {
    if !db.is_finite() {
        return 0;
    }
    let clamped = db.clamp(-40.0, 12.0);
    let stepped = (clamped / 4.0).round() as i32 * 4;
    stepped.clamp(-40, 12)
}

/// A filter corner to one of a handful of values a sweep passes through. `0` is off.
fn quantise_hz(hz: f64) -> u32 {
    // Spread wider than a lowpass alone needs, because a highpass lives at the bottom:
    // trimming an arriving track at 120 Hz is a common move and 250 would be a different one.
    const STEPS: [u32; 10] = [60, 120, 250, 500, 1000, 2000, 4000, 8000, 12000, 16000];
    if !hz.is_finite() || hz <= 0.0 || hz >= 20000.0 {
        return 0;
    }
    let mut best = STEPS[0];
    let mut distance = f64::INFINITY;
    for step in STEPS {
        // Nearest in octaves, not in Hz: 250 and 500 are as far apart to an ear as 8k and 16k.
        let apart = (f64::from(step).log2() - hz.log2()).abs();
        if apart < distance {
            distance = apart;
            best = step;
        }
    }
    best
}

/// A 0.0..=1.0 amount in steps of five per cent - fine enough that a ramp of it sounds
/// continuous, coarse enough that it does not rebuild the filter graph on every frame.
fn quantise_percent(amount: f64) -> u32 {
    if !amount.is_finite() || amount <= 0.0 {
        return 0;
    }
    (amount.clamp(0.0, 1.0) * 20.0).round() as u32 * 5
}

impl Recipe {
    /// Where in the transition the leaving track falls silent, as a fraction of the whole.
    ///
    /// This is what positions a transition against the music: that moment has to land on
    /// the outgoing track's natural end, because a deck cut off while its fader is still
    /// up is a hard stop that no curve can hide. Everything after it is the arriving track
    /// alone - still being worked on, but alone - which is why a sixteen-bar blend that
    /// empties out at bar twelve spends its last four bars past the end of the old track.
    ///
    /// So a fade length is the span of the whole move, and the score decides how much of it
    /// falls either side of the join.
    pub fn out_end(&self) -> f64 {
        let span = self.bars.unwrap_or(1.0);
        let normalise = |at: At| match at {
            At::Bar(bar) if span > 0.0 => bar / span,
            At::Bar(bar) => bar,
            At::Frac(frac) => frac,
        };
        self.lanes
            .iter()
            .find(|lane| lane.deck == Deck::Outgoing && lane.param == Param::Gain)
            .and_then(|lane| {
                // Where it falls silent *and stays there*, not the first time it touches
                // zero. A gate is at zero every other eighth and is very much still the
                // track playing; taking the first zero would trade the decks on the first
                // chop and leave the rest of the gate running on a deck nobody is
                // listening to. So: find the last moment it is audible, and the silence
                // begins at the key after it.
                let last_up = lane.keys.iter().rposition(|key| key.value.abs() > 1e-9)?;
                lane.keys.get(last_up + 1).or(lane.keys.last())
            })
            .map(|key| normalise(key.at))
            // A score whose fader never reaches zero is refused at read time, so this is
            // only reached by a caller holding a recipe it built itself.
            .unwrap_or(1.0)
            .clamp(0.05, 1.0)
    }

    /// The same moment, as a fraction of the *real* transition rather than of the score.
    ///
    /// [`Self::out_end`] is a position in the score, and `curve` decides when the clock
    /// gets there - so anything reasoning about when the decks actually trade places has
    /// to ask here, not there. Two things do: the swap itself, and the beat alignment
    /// that decides how far ahead of a bar line to start the overlap so the swap lands on
    /// it. Under `Late`, a score that empties out half way is really doing it seven
    /// tenths of the way through; alignment reading `0.5` would put the join a fifth of
    /// the overlap off the beat, which on an eight second fade is most of a bar.
    pub fn out_end_at(&self, curve: Curve) -> f64 {
        curve.unwarp(self.out_end()).clamp(0.05, 1.0)
    }
}

/// Where in `recipe` to read, at the real `clock`, under `curve`.
///
/// Both axes are warped, because a score writes its fades on whichever one suits it and
/// the curve is about the fade either way. Hooks are read on the *unwarped* clock (see
/// `Player::fire_hooks`): a lane is a fader move and moving it is the whole point, but a
/// hook is an event pinned to a musical moment - a beat roll that starts a third of a bar
/// late is not a beat roll - so the curve shapes what slides and leaves alone what lands.
fn shaped(recipe: &Recipe, clock: Clock, curve: Curve) -> Clock {
    let t = clock.t.clamp(0.0, 1.0);
    let bar = clock.bar.max(0.0);
    Clock {
        t: curve.warp(t),
        bar: match recipe.bars {
            // Normalise, warp, and put it back in bars: the curve is a shape, not a
            // length, and it must not change how many bars the move takes.
            Some(bars) if bars > 0.0 => curve.warp((bar / bars).clamp(0.0, 1.0)) * bars,
            // Without a declared length the caller passes `t` for both, so this is `t`.
            _ => curve.warp(bar.clamp(0.0, 1.0)),
        },
    }
}

/// The shape of `recipe` at `clock`, read through `curve`.
pub fn shape_of(recipe: &Recipe, clock: Clock, curve: Curve) -> Shape {
    let clock = shaped(recipe, clock, curve);
    let out = DeckState::read(recipe, Deck::Outgoing, clock);
    let incoming = DeckState::read(recipe, Deck::Incoming, clock);
    Shape {
        outgoing_gain: out.gain,
        incoming_gain: incoming.gain,
        outgoing_filter: out.filter(),
        incoming_filter: incoming.filter(),
        outgoing_tempo: out.tempo,
        incoming_tempo: incoming.tempo,
        outgoing_speed: out.speed,
        incoming_speed: incoming.speed,
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// The transitions that ship with the player, as the same text a user would write.
///
/// Embedded rather than installed so a fresh copy works with no files anywhere, and read
/// through the same parser as everything else so there is one format, one reader, and no
/// second implementation to drift. They are also the examples: what is copied out of
/// `transitions/` is exactly what runs.
/// The built-ins, for anything that wants to check them - the parser's own tests do.
#[cfg_attr(not(test), allow(dead_code))]
pub fn built_in() -> &'static [(&'static str, &'static str)] {
    BUILT_IN
}

static BUILT_IN: &[(&str, &str)] = &[
    ("crossfade", include_str!("../transitions/crossfade.mix")),
    ("bass_swap", include_str!("../transitions/bass-swap.mix")),
    (
        "filter_sweep",
        include_str!("../transitions/filter-sweep.mix"),
    ),
    ("echo_out", include_str!("../transitions/echo-out.mix")),
    (
        "cut_on_beat",
        include_str!("../transitions/cut-on-beat.mix"),
    ),
    ("long_blend", include_str!("../transitions/long-blend.mix")),
    ("brake", include_str!("../transitions/brake.mix")),
    ("drop_out", include_str!("../transitions/drop-out.mix")),
    ("drop_swap", include_str!("../transitions/drop-swap.mix")),
    ("filter_in", include_str!("../transitions/filter-in.mix")),
    (
        "highpass_out",
        include_str!("../transitions/highpass-out.mix"),
    ),
    ("radio_fade", include_str!("../transitions/radio-fade.mix")),
    ("slam", include_str!("../transitions/slam.mix")),
    ("vocal_swap", include_str!("../transitions/vocal-swap.mix")),
    (
        "double_drop",
        include_str!("../transitions/double-drop.mix"),
    ),
    ("sync_blend", include_str!("../transitions/sync-blend.mix")),
    ("sync_cut", include_str!("../transitions/sync-cut.mix")),
    ("tempo_ride", include_str!("../transitions/tempo-ride.mix")),
    ("beat_roll", include_str!("../transitions/beat-roll.mix")),
    ("gate_out", include_str!("../transitions/gate-out.mix")),
    ("half_time", include_str!("../transitions/half-time.mix")),
];

static REGISTRY: std::sync::OnceLock<Vec<Recipe>> = std::sync::OnceLock::new();

/// What loading found, so the player can say something when a file will not read.
#[derive(Default, Debug)]
pub struct Loaded {
    pub built_in: usize,
    /// Files of the user's own that parsed.
    pub user: usize,
    /// Files that did not, each already naming itself and its line.
    pub problems: Vec<String>,
}

/// Read the built-ins, then anything in `dir`, and settle the registry.
///
/// A user file overrides a built-in of the same name, which is what makes "edit the
/// standalone one" work: copy `long-blend.mix` next to your config, change it, and yours
/// is the one that runs. Loading happens once - a music player that re-read its
/// transitions mid-transition would be a different kind of instrument.
pub fn load(dir: Option<&std::path::Path>) -> Loaded {
    let mut report = Loaded::default();
    let mut recipes: Vec<Recipe> = Vec::new();
    for (key, text) in BUILT_IN {
        match crate::score::parse(&format!("built-in {key}"), key, text) {
            Ok(recipe) => recipes.push(recipe),
            // A built-in that will not parse is a bug in the player, not in anyone's
            // config, but it must still not stop the music.
            Err(problem) => report.problems.push(problem.to_string()),
        }
    }
    report.built_in = recipes.len();
    if let Some(dir) = dir {
        let (mine, problems) = crate::score::read_dir(dir);
        report.user = mine.len();
        report
            .problems
            .extend(problems.iter().map(ToString::to_string));
        for recipe in mine {
            match recipes.iter().position(|other| other.key == recipe.key) {
                Some(at) => recipes[at] = recipe,
                None => recipes.push(recipe),
            }
        }
    }
    let _ = REGISTRY.set(recipes);
    report
}

/// Every transition, in the order the settings menu cycles through them.
pub fn recipes() -> &'static [Recipe] {
    // A caller before `load` gets the built-ins rather than nothing: the alternative is a
    // player with no transitions at all because of an ordering mistake.
    REGISTRY.get_or_init(|| {
        BUILT_IN
            .iter()
            .filter_map(|(key, text)| {
                crate::score::parse(&format!("built-in {key}"), key, text).ok()
            })
            .collect()
    })
}

/// How one track is handed to the next: an index into [`recipes`], so the rest of the
/// player keeps passing a small `Copy` value around while the score lives in the registry.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style(usize);

impl Style {
    /// Every transition there is.
    #[cfg_attr(not(test), allow(dead_code))]
    /// The plain fade: an equal-power crossfade and nothing else.
    ///
    /// What the player runs when the beat-aware machinery is switched off, so it is looked
    /// up by name rather than assumed to be first in the list - the registry's order is a
    /// user's directory listing, and a built-in can be replaced by a file of theirs.
    pub fn plain() -> Style {
        Style::all()
            .into_iter()
            .find(|style| style.key() == "crossfade")
            .unwrap_or(Style(0))
    }

    pub fn all() -> Vec<Style> {
        (0..recipes().len()).map(Style).collect()
    }

    fn recipe(self) -> &'static Recipe {
        let all = recipes();
        // An index that cannot name a transition is a bug, but not one worth a panic in a
        // music player: fall back to the first, which is always right enough.
        all.get(self.0).unwrap_or(&all[0])
    }

    pub fn label(self) -> &'static str {
        &self.recipe().label
    }

    pub fn note(self) -> &'static str {
        &self.recipe().note
    }

    pub fn key(self) -> &'static str {
        &self.recipe().key
    }

    /// Empty for one that shipped with the player, otherwise where it was read from.
    pub fn origin(self) -> &'static str {
        &self.recipe().origin
    }

    /// Bars this transition is counted in, when it is counted rather than timed.
    pub fn bars(self) -> Option<f64> {
        self.recipe().bars
    }

    pub fn wants_beat_alignment(self) -> bool {
        self.recipe().beat_aligned
    }

    pub fn wants_tempo_match(self) -> bool {
        self.recipe().tempo_match
    }

    /// The hooks this transition fires, in the order it fires them.
    pub fn hooks(self) -> &'static [Hook] {
        &self.recipe().hooks
    }

    /// Where in the transition the leaving track really falls silent under `curve` - see
    /// [`Recipe::out_end_at`]. What the swap, the beat alignment and the settings row
    /// that explains the split all read; nothing outside this module has any use for the
    /// unshaped figure, so nothing outside it is offered one.
    pub fn out_end_at(self, curve: Curve) -> f64 {
        self.recipe().out_end_at(curve)
    }

    pub fn cycle(self, forward: bool) -> Style {
        let len = recipes().len().max(1);
        Style(if forward {
            (self.0 + 1) % len
        } else {
            (self.0 + len - 1) % len
        })
    }

    /// Forgiving of case, spaces and hyphens, so a label round-trips as well as a key.
    pub fn parse(text: &str) -> Option<Style> {
        let wanted = text.trim().to_lowercase().replace([' ', '-'], "_");
        recipes()
            .iter()
            .position(|recipe| {
                recipe.key == wanted || recipe.label.to_lowercase().replace(' ', "_") == wanted
            })
            .map(Style)
    }
}

/// The shape of `style` at `clock`, read through `curve`.
pub fn shape(style: Style, clock: Clock, curve: Curve) -> Shape {
    shape_of(style.recipe(), clock, curve)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk a transition the way the automix does.
    fn walk(style: Style, samples: usize) -> Vec<(f64, Shape)> {
        walk_curved(style, samples, Curve::Linear)
    }

    /// The same, shaped by a curve - the automix's own `t`, read through the setting.
    fn walk_curved(style: Style, samples: usize, curve: Curve) -> Vec<(f64, Shape)> {
        let bars = style.bars().unwrap_or(1.0);
        (0..=samples)
            .map(|i| {
                let t = i as f64 / samples as f64;
                (t, shape(style, Clock { t, bar: t * bars }, curve))
            })
            .collect()
    }

    #[test]
    fn every_score_is_written_in_time_order() {
        // Out-of-order keys are a bug in the score, not something to sort at runtime: the
        // evaluator walks the list once and would silently read the wrong pair.
        for recipe in recipes() {
            for lane in &recipe.lanes {
                let mut previous = f64::NEG_INFINITY;
                for key in &lane.keys {
                    let at = match key.at {
                        At::Bar(bar) => bar,
                        At::Frac(frac) => frac,
                    };
                    assert!(
                        at >= previous,
                        "{}: {:?} {:?} has a key at {at} after one at {previous}",
                        recipe.key,
                        lane.deck,
                        lane.param
                    );
                    previous = at;
                }
            }
            // Both decks must have a fader written for them. Leaving one out would give it
            // the neutral gain of 1.0, which for the arriving deck means starting at full.
            for deck in [Deck::Outgoing, Deck::Incoming] {
                assert!(
                    recipe
                        .lanes
                        .iter()
                        .any(|lane| lane.deck == deck && lane.param == Param::Gain),
                    "{} has no gain lane for {deck:?}",
                    recipe.key
                );
            }
        }
    }

    #[test]
    fn every_transition_starts_on_the_leaving_track_and_ends_on_the_arriving_one() {
        for style in Style::all() {
            let start = shape(style, Clock { t: 0.0, bar: 0.0 }, Curve::Linear);
            let bars = style.bars().unwrap_or(1.0);
            let end = shape(style, Clock { t: 1.0, bar: bars }, Curve::Linear);
            assert_eq!(
                (start.outgoing_gain, start.incoming_gain),
                (1.0, 0.0),
                "{} does not start on the leaving track",
                style.key()
            );
            assert_eq!(
                (end.outgoing_gain, end.incoming_gain),
                (0.0, 1.0),
                "{} does not end on the arriving one",
                style.key()
            );
            // ...and the deck that survives is handed over clean, at its own tempo.
            assert_eq!(
                end.incoming_filter,
                None,
                "{} leaves a filter on",
                style.key()
            );
            assert!(
                (end.incoming_tempo - 1.0).abs() < 1e-9,
                "{} leaves the arriving deck off its own tempo",
                style.key()
            );
        }
    }

    #[test]
    fn a_blend_holds_its_loudness_and_a_score_that_does_not_says_so() {
        for style in Style::all() {
            let recipe = style.recipe();
            if !recipe.constant_power {
                continue;
            }
            for (t, shape) in walk(style, 200) {
                let power = shape.outgoing_gain.powi(2) + shape.incoming_gain.powi(2);
                let dip = 10.0 * power.log10();
                assert!(
                    dip.abs() < 1.5,
                    "{} dips {dip:.2} dB at t={t:.2} - uncorrelated tracks add in power, \
                     so that is a hole in the middle of the mix",
                    style.key()
                );
            }
        }
        // ...and the long blend is honest about being the exception: it empties out on
        // purpose, and a test that demanded constant power would be demanding it not work.
        let long = Style::parse("long_blend").expect("the long blend exists");
        assert!(!long.recipe().constant_power);
        let middle = shape(long, Clock { t: 0.8, bar: 13.0 }, Curve::Linear);
        assert!(
            middle.outgoing_gain + middle.incoming_gain <= 1.01,
            "the long blend is supposed to be empty at bar 13, not full"
        );
    }

    #[test]
    fn every_curve_is_pinned_at_both_ends_and_never_runs_backwards() {
        for curve in Curve::ALL {
            assert!(
                curve.warp(0.0).abs() < 1e-9,
                "{} does not start at the start",
                curve.key()
            );
            assert!(
                (curve.warp(1.0) - 1.0).abs() < 1e-9,
                "{} does not reach the end - the old track would still be audible",
                curve.key()
            );
            // Rising the whole way is what makes it a curve rather than a rewind, and
            // it is also what `unwarp`'s bisection needs to be allowed to assume.
            let mut previous = -1.0;
            for i in 0..=200 {
                let p = f64::from(i) / 200.0;
                let w = curve.warp(p);
                assert!(
                    (0.0..=1.0).contains(&w),
                    "{} leaves the unit interval at {p}: {w}",
                    curve.key()
                );
                assert!(
                    w >= previous - 1e-12,
                    "{} runs backwards at {p} - that is a fade played in reverse",
                    curve.key()
                );
                previous = w;
            }
            // Nonsense in is the start of the transition, not a panic or a NaN loose in
            // the gain of a live deck.
            assert_eq!(curve.warp(f64::NAN), 0.0);
            assert_eq!(curve.warp(-5.0), 0.0);
            assert_eq!(curve.warp(5.0), 1.0);
        }
    }

    #[test]
    fn unwarp_really_is_the_other_direction() {
        for curve in Curve::ALL {
            for i in 0..=40 {
                let p = f64::from(i) / 40.0;
                let round_trip = curve.warp(curve.unwarp(p));
                assert!(
                    (round_trip - p).abs() < 1e-6,
                    "{}: unwarp({p}) came back as {round_trip}",
                    curve.key()
                );
            }
        }
        // And it is the one thing it is used for: where the decks really trade places.
        // A `Late` curve is behind the score, so the written moment arrives later than
        // written; `Early` is ahead, so it arrives sooner. Getting this backwards would
        // cut the leaving deck off with its fader still up.
        let style = Style::parse("crossfade").expect("the plain fade exists");
        let written = style.out_end_at(Curve::Linear);
        assert!(style.out_end_at(Curve::Late) > written);
        assert!(style.out_end_at(Curve::Early) < written);
    }

    #[test]
    fn a_curve_restyles_every_score_without_breaking_one() {
        // The whole claim of the setting: it applies to everything, and everything it
        // applies to still obeys the rules it obeyed before.
        for curve in Curve::ALL {
            for style in Style::all() {
                let walked = walk_curved(style, 120, curve);
                let (first, last) = (
                    walked.first().expect("samples").1.clone(),
                    walked.last().expect("samples").1.clone(),
                );
                assert_eq!(
                    (first.outgoing_gain, first.incoming_gain),
                    (1.0, 0.0),
                    "{} under {} does not start on the leaving track",
                    style.key(),
                    curve.key()
                );
                assert_eq!(
                    (last.outgoing_gain, last.incoming_gain),
                    (0.0, 1.0),
                    "{} under {} does not end on the arriving one",
                    style.key(),
                    curve.key()
                );
                // Constant power is the invariant most at risk from a global reshape,
                // and the one the whole warp-the-clock design exists to protect: both
                // gains are read at the same warped moment, so `cos² + sin²` is still
                // one whatever the moment turns out to be.
                if style.recipe().constant_power {
                    for (t, shape) in &walked {
                        let power = shape.outgoing_gain.powi(2) + shape.incoming_gain.powi(2);
                        let dip = 10.0 * power.log10();
                        assert!(
                            dip.abs() < 1.5,
                            "{} under {} dips {dip:.2} dB at t={t:.2}",
                            style.key(),
                            curve.key()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_curve_moves_when_a_move_happens_and_not_what_the_move_is() {
        let style = Style::parse("crossfade").expect("the plain fade exists");
        let at = |t: f64, curve: Curve| shape(style, Clock { t, bar: t }, curve).outgoing_gain;

        // Sampled at a quarter, which is the middle of the *fade*: the plain crossfade's
        // fader reaches zero at `0.5` and spends the rest of the span on the arriving
        // track alone, so half way through the transition is already past the end of the
        // move this is about.
        //
        // There, `Late` still has more of the old track up than the score asks for and
        // `Early` has less. That is the entire audible difference.
        let written = at(0.25, Curve::Linear);
        assert!(
            at(0.25, Curve::Late) > written + 0.05,
            "Late is not holding on to the leaving track"
        );
        assert!(
            at(0.25, Curve::Early) < written - 0.05,
            "Early is not letting go of it"
        );

        // A hold is still a hold: `cut_on_beat` cuts, and no curve turns a cut into a
        // fade, because a curve moves the clock and a hold does not read it.
        let cut = Style::parse("cut_on_beat").expect("the cut exists");
        for curve in Curve::ALL {
            let gains: Vec<f64> = walk_curved(cut, 200, curve)
                .iter()
                .map(|(_, s)| s.outgoing_gain)
                .collect();
            let partial = gains.iter().filter(|g| **g > 0.01 && **g < 0.99).count();
            assert!(
                partial <= 2,
                "{} turned the cut into a fade: {partial} samples mid-way",
                curve.key()
            );
        }
    }

    #[test]
    fn a_gated_fader_falls_silent_where_it_stays_silent() {
        // The gate is at zero every other eighth and is very much still the track
        // playing. Taking the first zero - which is what this used to do - would trade
        // the decks on the first chop, three quarters of a bar in, and run the rest of
        // the gate on a deck nobody could hear.
        let gate = Style::parse("gate_out").expect("the gate exists");
        let out_end = gate.out_end_at(Curve::Linear);
        assert!(
            out_end > 0.6,
            "the gate hands over at {out_end:.3} of four bars - that is the first chop, \
             not the end of the gate"
        );
        // And it really does chop: the fader is up and down repeatedly before then,
        // which is the difference between a gate and a fade.
        let gains: Vec<f64> = walk(gate, 400)
            .iter()
            .map(|(_, s)| s.outgoing_gain)
            .collect();
        let flips = gains
            .windows(2)
            .filter(|w| (w[0] > 0.5) != (w[1] > 0.5))
            .count();
        assert!(
            flips >= 12,
            "only {flips} edges - `every` did not expand into a square wave"
        );
    }

    #[test]
    fn the_beat_roll_tightens_and_always_hands_the_deck_back() {
        let roll = Style::parse("beat_roll").expect("the roll exists");
        let loops: Vec<(f64, Option<f64>)> = roll
            .hooks()
            .iter()
            .filter_map(|hook| match (hook.at, &hook.action) {
                (At::Bar(bar), HookAction::Loop { bars, .. }) => Some((bar, *bars)),
                _ => None,
            })
            .collect();
        assert_eq!(
            loops,
            vec![
                (6.0, Some(0.5)),
                (6.5, Some(0.25)),
                (6.75, Some(0.125)),
                (7.0, None),
            ],
            "the roll should halve three times and then let go"
        );
        // The last one is the one that matters: a roll left running is a deck that
        // plays half a bar of the next track forever.
        assert_eq!(
            loops.last().map(|(_, bars)| *bars),
            Some(None),
            "the roll never turns itself off"
        );
        // ...and it lets go no later than the swap, so the hook still has a deck to
        // reach. (`Player::release_loops` is the belt to this file's braces, for the
        // transitions that get abandoned before their last hook.)
        let at = loops.last().expect("a last hook").0 / roll.bars().expect("counted in bars");
        assert!(
            at <= roll.out_end_at(Curve::Linear) + 1e-9,
            "the roll is turned off after the decks have already traded"
        );
    }

    #[test]
    fn filter_chains_are_rebuilt_a_handful_of_times_not_every_frame() {
        // Every change rebuilds mpv's filter graph mid-playback, so a sweep that moved
        // continuously would pay that on every frame.
        for style in Style::all() {
            for deck in [Deck::Outgoing, Deck::Incoming] {
                let mut distinct: Vec<Option<String>> = Vec::new();
                for (_, shape) in walk(style, 200) {
                    let chain = match deck {
                        Deck::Outgoing => shape.outgoing_filter,
                        Deck::Incoming => shape.incoming_filter,
                    };
                    if !distinct.contains(&chain) {
                        distinct.push(chain);
                    }
                }
                // The bound is per transition, not per second, and the long blend is a
                // thirty-second move with three sweeps in it - about one rebuild a second.
                // What this guards against is the two-hundred that a continuously computed
                // cutoff would produce, which is a filter-graph rebuild on every frame.
                assert!(
                    distinct.len() <= 32,
                    "{} rebuilds {:?}'s chain {} times across one transition",
                    style.key(),
                    deck,
                    distinct.len()
                );
            }
        }
    }

    #[test]
    fn the_long_blend_does_what_it_says() {
        let style = Style::parse("Long blend").expect("parses from its label too");
        assert_eq!(style.bars(), Some(16.0));
        assert!(style.wants_beat_alignment() && style.wants_tempo_match());
        let at = |bar: f64| shape(style, Clock { t: bar / 16.0, bar }, Curve::Linear);

        // Bar 0: both playing, the arriving one silent, bassless, mid pulled back, and
        // locked to the leaving track's tempo.
        let start = at(0.0);
        assert_eq!(start.incoming_gain, 0.0);
        let arriving = start.incoming_filter.clone().unwrap_or_default();
        assert!(
            arriving.contains("bass=g=-40"),
            "arriving track has its bass: {arriving}"
        );
        assert!(
            arriving.contains("equalizer"),
            "arriving mid is not pulled back: {arriving}"
        );
        assert_eq!(
            start.incoming_tempo, 0.0,
            "the arriving deck is not tempo-locked"
        );

        // Bar 4: the leaving track has lost its low end...
        let four = at(4.0).outgoing_filter.clone().unwrap_or_default();
        assert!(
            four.contains("bass=g=-40"),
            "leaving track keeps its bass at bar 4: {four}"
        );

        // Bar 8: ...and its top, while the arriving one is up and restored.
        let eight = at(8.0);
        let leaving = eight.outgoing_filter.clone().unwrap_or_default();
        assert!(
            leaving.contains("treble=g=-40"),
            "leaving track keeps its top: {leaving}"
        );
        assert_eq!(
            eight.incoming_gain, 1.0,
            "the arriving track is not up by bar 8"
        );
        let arriving = eight.incoming_filter.clone().unwrap_or_default();
        assert!(
            !arriving.contains("equalizer"),
            "the arriving track's mid should be restored by bar 8: {arriving}"
        );
        assert!(
            arriving.contains("bass=g=-40"),
            "...but its low end must not arrive until the drop: {arriving}"
        );

        // Bar 12: the leaving track is gone, swept out under a lowpass on its way.
        assert_eq!(
            at(12.0).outgoing_gain,
            0.0,
            "the leaving track is still audible at bar 12"
        );
        let swept = at(10.0).outgoing_filter.clone().unwrap_or_default();
        assert!(
            swept.contains("lowpass"),
            "nothing swept the leaving track away: {swept}"
        );

        // Bars 12-16: four bars of the arriving track alone, still with no low end...
        for bar in [12.5, 13.0, 14.0, 15.0, 15.9] {
            let waiting = at(bar).incoming_filter.clone().unwrap_or_default();
            assert!(
                waiting.contains("bass=g=-40"),
                "the bass arrived early, at bar {bar}: {waiting}"
            );
        }
        // ...and on the sixteenth it arrives in one step, not a fade.
        assert_eq!(at(16.0).incoming_filter, None, "the drop never happened");
    }

    #[test]
    fn a_score_without_a_beat_grid_is_the_same_shape_at_the_wrong_tempo() {
        // The automix passes `bar = t * bars` when it has no grid. The moves must still
        // happen in the right order, because a transition that has to be abandoned when
        // the tempo is unreadable is a transition nobody can rely on.
        let style = Style::parse("long_blend").expect("exists");
        let bars = style.bars().unwrap_or(16.0);
        let mut gains: Vec<f64> = Vec::new();
        for i in 0..=100 {
            let t = f64::from(i) / 100.0;
            gains.push(shape(style, Clock { t, bar: t * bars }, Curve::Linear).incoming_gain);
        }
        assert_eq!(gains.first(), Some(&0.0));
        assert_eq!(gains.last(), Some(&1.0));
        assert!(
            gains.windows(2).all(|pair| pair[1] >= pair[0] - 1e-9),
            "the arriving fader goes backwards somewhere"
        );
    }

    #[test]
    fn keys_and_labels_both_round_trip() {
        for style in Style::all() {
            assert_eq!(Style::parse(style.key()), Some(style));
            assert_eq!(Style::parse(style.label()), Some(style));
            assert_eq!(Style::parse(&style.label().to_uppercase()), Some(style));
        }
        assert_eq!(Style::parse("not a transition"), None);
        // Cycling visits every score and comes home.
        let mut style = Style::default();
        for _ in 0..recipes().len() {
            style = style.cycle(true);
        }
        assert_eq!(style, Style::default());
    }

    #[test]
    fn easings_are_pinned_at_both_ends() {
        // `cos(PI/2)` is 6e-17, and a deck left at 0.99999999 of the user's volume for the
        // rest of the night is a bug nobody can see and everybody can hear.
        for ease in [
            Ease::Linear,
            Ease::Smooth,
            Ease::Hold,
            Ease::PowerDown,
            Ease::PowerUp,
        ] {
            assert_eq!(travel(0.25, 0.75, 0.0, ease), 0.25, "{ease:?} at the start");
            assert_eq!(travel(0.25, 0.75, 1.0, ease), 0.75, "{ease:?} at the end");
            assert_eq!(
                travel(0.25, 0.75, -1.0, ease),
                0.25,
                "{ease:?} clamps below"
            );
            assert_eq!(travel(0.25, 0.75, 9.0, ease), 0.75, "{ease:?} clamps above");
        }
        // A hold really holds, right up to the jump.
        assert_eq!(travel(1.0, 0.0, 0.999, Ease::Hold), 1.0);
    }

    #[test]
    fn quantising_keeps_the_vocabulary_small_and_the_ear_happy() {
        assert_eq!(quantise_db(0.0), 0);
        assert_eq!(
            quantise_db(-1.0),
            0,
            "a dB of nothing is not worth a graph rebuild"
        );
        assert_eq!(quantise_db(-40.0), -40);
        assert_eq!(quantise_db(-999.0), -40, "silence has a floor");
        assert_eq!(quantise_db(f64::NAN), 0, "nonsense is flat, not a filter");
        // Nearest in octaves, not in Hz: 250 and 500 are as far apart to an ear as 8k and 16k.
        assert_eq!(quantise_hz(0.0), 0);
        assert_eq!(quantise_hz(260.0), 250);
        assert_eq!(quantise_hz(700.0), 500);
        assert_eq!(quantise_hz(760.0), 1000);
        assert_eq!(quantise_hz(19999.0), 16000);
        assert_eq!(
            quantise_hz(20000.0),
            0,
            "a lowpass above hearing is no lowpass"
        );
        assert_eq!(quantise_percent(0.0), 0);
        assert_eq!(
            quantise_percent(0.02),
            0,
            "a whisper of echo is not worth a graph rebuild"
        );
        assert_eq!(quantise_percent(0.5), 50);
        assert_eq!(quantise_percent(1.0), 100);
        assert_eq!(
            quantise_percent(f64::NAN),
            0,
            "nonsense is off, not a filter"
        );
    }

    #[test]
    fn echo_is_a_lane_now_not_a_switch() {
        let recipe = Recipe {
            key: "t".to_string(),
            label: "T".to_string(),
            note: String::new(),
            bars: None,
            beat_aligned: false,
            tempo_match: false,
            constant_power: false,
            lanes: vec![Lane {
                deck: Deck::Outgoing,
                param: Param::Echo,
                keys: vec![
                    Key {
                        at: At::Frac(0.0),
                        value: 0.0,
                        ease: Ease::Linear,
                    },
                    Key {
                        at: At::Frac(1.0),
                        value: 0.5,
                        ease: Ease::Hold,
                    },
                ],
            }],
            hooks: Vec::new(),
            origin: String::new(),
        };
        // No echo at the top: the filter is left out entirely, not run at a decay of zero.
        let start = DeckState::read(&recipe, Deck::Outgoing, Clock { t: 0.0, bar: 0.0 });
        assert_eq!(start.filter(), None);
        // Half way to the ceiling by the end - and the feedback is what rides, not the mix.
        let end = DeckState::read(&recipe, Deck::Outgoing, Clock { t: 1.0, bar: 0.0 });
        assert_eq!(
            end.filter(),
            Some("lavfi=[aecho=0.8:0.7:350:0.50]".to_string())
        );
    }

    #[test]
    fn every_filter_string_comes_from_the_checked_vocabulary() {
        // mpv accepts an `af` string it cannot parse and reports success, so a typo here is
        // silent. Everything emitted must be one of the forms checked against a real mpv.
        for style in Style::all() {
            for (_, shape) in walk(style, 400) {
                for chain in [shape.outgoing_filter, shape.incoming_filter]
                    .into_iter()
                    .flatten()
                {
                    assert!(
                        chain.starts_with("lavfi=[") && chain.ends_with(']'),
                        "not an mpv filter node: {chain}"
                    );
                    for part in chain
                        .trim_start_matches("lavfi=[")
                        .trim_end_matches(']')
                        .split(',')
                    {
                        let known = part.starts_with("bass=g=")
                            || part.starts_with("treble=g=")
                            || part.starts_with("equalizer=f=1200:t=h:w=1600:g=")
                            || part.starts_with("lowpass=f=")
                            || part.starts_with("highpass=f=")
                            || part.starts_with("aecho=0.8:0.7:350:");
                        assert!(known, "unchecked filter form: {part}");
                    }
                }
            }
        }
    }
}
