//! Reading a transition out of a file.
//!
//! Transitions are data, and data belongs in files rather than in a match arm nobody can
//! edit without a compiler. This is the reader for the small language they are written in:
//! a handful of settings, then lanes of keyframes, then optional hooks.
//!
//! ```text
//! name    Long blend
//! bars    16
//! align   beat
//!
//! lane in bass
//!     bar 0     -40    hold
//!     bar 16      0    hold
//!
//! hook bar 12   effect Echo on
//! ```
//!
//! Every error carries a file name and a line number, because the person reading it is the
//! person who typed the mistake, and a parser that says "invalid input" to someone editing
//! their own mix at two in the morning is not worth having. Nothing here ever panics on bad
//! input and nothing is guessed at: an unknown word is an error, not a default, so a typo
//! cannot silently produce a transition that does almost what was meant.

use crate::transitions::{At, Deck, Ease, Hook, HookAction, Key, Lane, Param, Recipe};
use std::path::Path;

/// Where a problem is, in terms the author can act on.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub source: String,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.source, self.line, self.message)
    }
}

/// Parse one transition file. `source` names it in any error - a path, usually.
///
/// `key` is the recipe's stable identity and comes from the file name rather than the
/// contents, so that a user file called `long-blend.mix` overrides the built-in of that
/// name whatever the author wrote in the `name` line.
pub fn parse(source: &str, key: &str, text: &str) -> Result<Recipe, Problem> {
    let mut recipe = Recipe {
        key: key.to_string(),
        label: key.to_string(),
        note: String::new(),
        bars: None,
        beat_aligned: false,
        tempo_match: false,
        constant_power: true,
        lanes: Vec::new(),
        hooks: Vec::new(),
        origin: String::new(),
    };
    let mut lane: Option<Lane> = None;
    // What the header claims about tempo, checked against what the lanes actually do.
    let mut declared_tempo: Option<bool> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let fault = |message: String| Problem {
            source: source.to_string(),
            line,
            message,
        };
        // Everything after a `#` is a comment, and a blank line is nothing at all.
        let content = raw.split('#').next().unwrap_or_default().trim();
        if content.is_empty() {
            continue;
        }
        let mut words = content.split_whitespace();
        let Some(head) = words.next() else { continue };

        match head {
            "name" | "note" => {
                let rest = content[head.len()..].trim().to_string();
                if rest.is_empty() {
                    return Err(fault(format!("{head} needs something after it")));
                }
                if head == "name" {
                    recipe.label = rest;
                } else {
                    recipe.note = rest;
                }
            }
            "bars" => {
                let value = words
                    .next()
                    .ok_or_else(|| fault("bars needs a count".to_string()))?;
                let bars: f64 = value
                    .parse()
                    .map_err(|_| fault(format!("bars wants a number, not {value:?}")))?;
                if !(bars.is_finite() && bars > 0.0) {
                    return Err(fault(format!("bars must be positive, not {bars}")));
                }
                recipe.bars = Some(bars);
            }
            "align" => {
                recipe.beat_aligned = match words.next() {
                    Some("beat") => true,
                    Some("free") => false,
                    other => {
                        return Err(fault(format!(
                            "align is beat or free, not {:?}",
                            other.unwrap_or("nothing")
                        )));
                    }
                }
            }
            "tempo" => {
                // Kept as documentation and as a claim to check, not as the switch. What
                // decides whether the arriving track is pulled to the playing one's speed
                // is whether the score has a `tempo` lane - see `validate`. Two ways of
                // saying one thing is two ways of saying different things by accident.
                declared_tempo = match words.next() {
                    Some("match") => Some(true),
                    Some("free") => Some(false),
                    other => {
                        return Err(fault(format!(
                            "tempo is match or free, not {:?}",
                            other.unwrap_or("nothing")
                        )));
                    }
                }
            }
            "power" => {
                recipe.constant_power = match words.next() {
                    Some("constant") => true,
                    Some("free") => false,
                    other => {
                        return Err(fault(format!(
                            "power is constant or free, not {:?}",
                            other.unwrap_or("nothing")
                        )));
                    }
                }
            }
            "lane" => {
                // A new lane ends the one before it.
                if let Some(done) = lane.take() {
                    recipe.lanes.push(done);
                }
                let deck = parse_deck(words.next()).map_err(&fault)?;
                let param = parse_param(words.next()).map_err(&fault)?;
                if recipe
                    .lanes
                    .iter()
                    .any(|other| other.deck == deck && other.param == param)
                {
                    return Err(fault(format!(
                        "{deck:?} {param:?} already has a lane - one lane per control per deck"
                    )));
                }
                lane = Some(Lane {
                    deck,
                    param,
                    keys: Vec::new(),
                });
            }
            "hook" => {
                let at = parse_at(words.next(), words.next()).map_err(&fault)?;
                let action = parse_action(&mut words, content).map_err(&fault)?;
                recipe.hooks.push(Hook { at, action });
            }
            "bar" | "at" => {
                let Some(current) = lane.as_mut() else {
                    return Err(fault(
                        "a keyframe before any lane - say `lane out gain` first".to_string(),
                    ));
                };
                let at = parse_at(Some(head), words.next()).map_err(&fault)?;
                let raw_value = words
                    .next()
                    .ok_or_else(|| fault("a keyframe needs a value".to_string()))?;
                let value: f64 = raw_value
                    .parse()
                    .map_err(|_| fault(format!("{raw_value:?} is not a number")))?;
                if !value.is_finite() {
                    return Err(fault(format!("{raw_value:?} is not a usable number")));
                }
                let ease = parse_ease(words.next()).map_err(&fault)?;
                push_key(current, Key { at, value, ease }, &fault)?;
            }
            "every" => {
                let Some(current) = lane.as_mut() else {
                    return Err(fault(
                        "a keyframe before any lane - say `lane out gain` first".to_string(),
                    ));
                };
                let step = parse_bar_fraction(words.next()).map_err(&fault)?;
                if words.next() != Some("from") {
                    return Err(fault(
                        "every wants `every <fraction> from bar <n> to bar <n> <v0> <v1> \
                         <ease>`"
                            .to_string(),
                    ));
                }
                let from = parse_bar_only(words.next(), words.next()).map_err(&fault)?;
                if words.next() != Some("to") {
                    return Err(fault(
                        "every needs `to bar <n>` after its start".to_string(),
                    ));
                }
                let to = parse_bar_only(words.next(), words.next()).map_err(&fault)?;
                let values: Vec<f64> = (0..2)
                    .map(|_| {
                        let raw = words.next().ok_or_else(|| {
                            "every needs two values, the pulse toggles between them".to_string()
                        })?;
                        raw.parse::<f64>()
                            .map_err(|_| format!("{raw:?} is not a number"))
                            .and_then(|v| {
                                v.is_finite()
                                    .then_some(v)
                                    .ok_or_else(|| format!("{raw:?} is not a usable number"))
                            })
                    })
                    .collect::<Result<_, String>>()
                    .map_err(&fault)?;
                let ease = parse_ease(words.next()).map_err(&fault)?;
                if to <= from {
                    return Err(fault(format!(
                        "every runs from bar {from} to bar {to} - that never starts"
                    )));
                }
                // Rounded rather than floored: a span written as a clean number of steps
                // (2 bars at 1/16) should not lose its last pulse to float error.
                let steps = ((to - from) / step).round().max(1.0) as usize;
                for i in 0..steps {
                    let at = At::Bar(from + i as f64 * step);
                    let value = values[i % 2];
                    push_key(current, Key { at, value, ease }, &fault)?;
                }
            }
            other => {
                return Err(fault(format!(
                    "{other:?} means nothing here - expected name, note, bars, align, tempo, \
                     power, lane, hook, bar, at or every"
                )));
            }
        }
    }
    if let Some(done) = lane.take() {
        recipe.lanes.push(done);
    }
    // A score wants the arriving track pulled to the playing one's tempo exactly when it
    // has a lane to release it again. Inferred rather than declared, so the two halves of
    // "sync" cannot drift apart.
    recipe.tempo_match = recipe.lanes.iter().any(|lane| lane.param == Param::Tempo);
    if let Some(declared) = declared_tempo
        && declared != recipe.tempo_match
    {
        return Err(Problem {
            source: source.to_string(),
            line: 0,
            message: if declared {
                "the header says `tempo match` but no lane automates tempo - the arriving \
                 track would be pulled to this one's speed and never let back to its own"
                    .to_string()
            } else {
                "the header says `tempo free` but a lane automates tempo - one of the two \
                 is wrong, and a silent disagreement here is a transition that plays at \
                 the wrong speed"
                    .to_string()
            },
        });
    }
    validate(source, &recipe)?;
    Ok(recipe)
}

fn position(at: At) -> f64 {
    match at {
        At::Bar(bar) => bar,
        At::Frac(frac) => frac,
    }
}

/// Push a keyframe onto the lane being built, refusing one that reads out of order - the
/// same check whether it came from a written `bar`/`at` line or one `every` expanded.
fn push_key(
    current: &mut Lane,
    key: Key,
    fault: &impl Fn(String) -> Problem,
) -> Result<(), Problem> {
    if let Some(previous) = current.keys.last()
        && position(previous.at) > position(key.at)
    {
        return Err(fault(format!(
            "this keyframe is at {} but the one before it is at {}",
            position(key.at),
            position(previous.at)
        )));
    }
    current.keys.push(key);
    Ok(())
}

/// A moment that must be in bars - `every`'s step is a fraction of one, and a fraction of
/// the whole transition is not a length `every` can repeat against.
fn parse_bar_only(unit: Option<&str>, value: Option<&str>) -> Result<f64, String> {
    match parse_at(unit, value)? {
        At::Bar(bar) => Ok(bar),
        At::Frac(_) => Err(
            "every is written in bars - a step size means nothing against a fraction of \
                 the whole transition"
                .to_string(),
        ),
    }
}

/// The rules a transition has to obey to be playable at all.
///
/// Checked here rather than at playback: a file with a mistake in it should be refused when
/// it is read, with a line number, not halfway through a transition at a party.
fn validate(source: &str, recipe: &Recipe) -> Result<(), Problem> {
    let fault = |message: String| Problem {
        source: source.to_string(),
        line: 0,
        message,
    };
    for deck in [Deck::Outgoing, Deck::Incoming] {
        let Some(gain) = recipe
            .lanes
            .iter()
            .find(|lane| lane.deck == deck && lane.param == Param::Gain)
        else {
            return Err(fault(format!(
                "no gain lane for the {} deck - without one it would play at full volume \
                 throughout",
                deck_word(deck)
            )));
        };
        if gain.keys.len() < 2 {
            return Err(fault(format!(
                "the {} deck's gain lane needs at least a start and an end",
                deck_word(deck)
            )));
        }
        // The endpoints are the one thing a transition may not get wrong: the wrong value at
        // either end leaves a deck stranded for the rest of the night.
        let (first, last) = (gain.keys[0].value, gain.keys[gain.keys.len() - 1].value);
        let (want_first, want_last) = match deck {
            Deck::Outgoing => (1.0, 0.0),
            Deck::Incoming => (0.0, 1.0),
        };
        if (first - want_first).abs() > 1e-9 || (last - want_last).abs() > 1e-9 {
            return Err(fault(format!(
                "the {} deck's gain must start at {want_first} and end at {want_last}, \
                 not {first} and {last}",
                deck_word(deck)
            )));
        }
    }
    // Every moment a score names has to be one the transition will actually reach. A key
    // past the end is not a late instruction, it is one that never runs - and the endpoint
    // check above passes on it happily, because the value written there is correct. That
    // is the worst kind of fault: a file that says the outgoing deck reaches silence, and
    // an outgoing deck that gets cut off at full volume every time.
    for lane in &recipe.lanes {
        for key in &lane.keys {
            // Only bars need checking here. A fraction past the end is caught the moment
            // it is read, because `at` is a fraction of the whole and 1.4 of a whole is
            // wrong on its face; `bar 20` is only wrong once you know the score is
            // sixteen bars long, which is not known until the whole file has been read.
            let past = match (key.at, recipe.bars) {
                (At::Bar(bar), Some(bars)) => bar > bars,
                _ => false,
            };
            if past {
                return Err(fault(format!(
                    "the {} deck's {} lane has a keyframe at {} - the transition ends \
                     before that, so it would never run",
                    deck_word(lane.deck),
                    param_word(lane.param),
                    moment_word(key.at)
                )));
            }
        }
    }
    for hook in &recipe.hooks {
        let past = match (hook.at, recipe.bars) {
            (At::Bar(bar), Some(bars)) => bar > bars,
            _ => false,
        };
        if past {
            return Err(fault(format!(
                "a hook at {} - the transition ends before that, so it would never fire",
                moment_word(hook.at)
            )));
        }
    }
    // An effect switched on has to be switched off by the same score. Nothing else will:
    // the effect rack is the player's, not the transition's, so one left on is left on for
    // the rest of the night.
    let mut left_on: Vec<&str> = Vec::new();
    for hook in &recipe.hooks {
        if let HookAction::Effect { name, on } = &hook.action {
            left_on.retain(|held| !held.eq_ignore_ascii_case(name));
            if *on {
                left_on.push(name);
            }
        }
    }
    if let Some(name) = left_on.first() {
        return Err(fault(format!(
            "`{name}` is switched on and never off - a transition has to leave the rack \
             as it found it"
        )));
    }
    if recipe.bars.is_some()
        && recipe
            .lanes
            .iter()
            .flat_map(|lane| lane.keys.iter())
            .all(|key| matches!(key.at, At::Frac(_)))
    {
        return Err(fault(
            "bars is set but every keyframe is written with `at` - use `bar` or drop the \
             bars line"
                .to_string(),
        ));
    }
    Ok(())
}

fn deck_word(deck: Deck) -> &'static str {
    match deck {
        Deck::Outgoing => "leaving",
        Deck::Incoming => "arriving",
    }
}

/// The word a control is written with, so a complaint names it the way the file does.
fn param_word(param: Param) -> &'static str {
    match param {
        Param::Gain => "gain",
        Param::Bass => "bass",
        Param::Mid => "mid",
        Param::High => "high",
        Param::Lowpass => "lowpass",
        Param::Highpass => "highpass",
        Param::Speed => "speed",
        Param::Tempo => "tempo",
        Param::Echo => "echo",
    }
}

/// Likewise for a moment, so `bar 20` is complained about as `bar 20`.
fn moment_word(at: At) -> String {
    match at {
        At::Bar(bar) => format!("bar {bar}"),
        At::Frac(frac) => format!("at {frac}"),
    }
}

fn parse_deck(word: Option<&str>) -> Result<Deck, String> {
    match word {
        Some("out") | Some("outgoing") | Some("leaving") => Ok(Deck::Outgoing),
        Some("in") | Some("incoming") | Some("arriving") => Ok(Deck::Incoming),
        other => Err(format!(
            "a deck is out or in, not {:?}",
            other.unwrap_or("nothing")
        )),
    }
}

fn parse_param(word: Option<&str>) -> Result<Param, String> {
    match word {
        Some("gain") => Ok(Param::Gain),
        Some("bass") => Ok(Param::Bass),
        Some("mid") => Ok(Param::Mid),
        Some("high") => Ok(Param::High),
        Some("lowpass") => Ok(Param::Lowpass),
        Some("highpass") => Ok(Param::Highpass),
        Some("speed") => Ok(Param::Speed),
        Some("tempo") => Ok(Param::Tempo),
        Some("echo") => Ok(Param::Echo),
        other => Err(format!(
            "{:?} is not a control - try gain, bass, mid, high, lowpass, highpass, speed, \
             tempo or echo",
            other.unwrap_or("nothing")
        )),
    }
}

fn parse_at(unit: Option<&str>, value: Option<&str>) -> Result<At, String> {
    let raw = value.ok_or_else(|| "a moment needs a number after it".to_string())?;
    let number: f64 = raw
        .parse()
        .map_err(|_| format!("{raw:?} is not a number"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("{raw:?} is not a moment"));
    }
    match unit {
        Some("bar") => Ok(At::Bar(number)),
        Some("at") => {
            if number > 1.0 {
                return Err(format!(
                    "`at` is a fraction of the transition, so {number} is past the end - \
                     did you mean `bar {number}`?"
                ));
            }
            Ok(At::Frac(number))
        }
        other => Err(format!(
            "a moment is `bar <n>` or `at <0..1>`, not {:?}",
            other.unwrap_or("nothing")
        )),
    }
}

fn parse_ease(word: Option<&str>) -> Result<Ease, String> {
    match word {
        Some("hold") => Ok(Ease::Hold),
        Some("linear") => Ok(Ease::Linear),
        Some("smooth") => Ok(Ease::Smooth),
        Some("power-down") => Ok(Ease::PowerDown),
        Some("power-up") => Ok(Ease::PowerUp),
        // Deliberately not defaulted. An ease is the difference between a fade and a drop,
        // and guessing it would make a typo sound like a decision.
        other => Err(format!(
            "{:?} is not an ease - try hold, linear, smooth, power-down or power-up",
            other.unwrap_or("nothing")
        )),
    }
}

fn parse_action<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    whole: &str,
) -> Result<HookAction, String> {
    match words.next() {
        Some("effect") => {
            let name = words
                .next()
                .ok_or_else(|| "effect needs a name".to_string())?
                .to_string();
            let on = match words.next() {
                Some("on") => true,
                Some("off") => false,
                other => {
                    return Err(format!(
                        "an effect is switched on or off, not {:?}",
                        other.unwrap_or("nothing")
                    ));
                }
            };
            Ok(HookAction::Effect { name, on })
        }
        Some("toast") => {
            // Everything after the word, so the message may contain spaces.
            let text = whole
                .split_once("toast")
                .map(|(_, rest)| rest.trim().to_string())
                .unwrap_or_default();
            if text.is_empty() {
                return Err("toast needs something to say".to_string());
            }
            Ok(HookAction::Toast(text))
        }
        Some("loop") => match words.next() {
            Some("off") => Ok(HookAction::Loop {
                deck: None,
                bars: None,
            }),
            other => {
                let deck = parse_deck(other)?;
                let bars = parse_bar_fraction(words.next())?;
                Ok(HookAction::Loop {
                    deck: Some(deck),
                    bars: Some(bars),
                })
            }
        },
        other => Err(format!(
            "{:?} is not something a hook can do - try `effect <name> on|off`, \
             `loop <deck> <fraction>`, `loop off` or `toast <text>`",
            other.unwrap_or("nothing")
        )),
    }
}

/// A loop length as a fraction of a bar: `1/2`, `1/4`, or a bare decimal like `0.5`.
fn parse_bar_fraction(word: Option<&str>) -> Result<f64, String> {
    let word = word.ok_or_else(|| "loop needs a length, like `1/2`".to_string())?;
    let bad = || format!("{word:?} is not a length like `1/2`");
    let value = match word.split_once('/') {
        Some((num, den)) => {
            let num: f64 = num.parse().map_err(|_| bad())?;
            let den: f64 = den.parse().map_err(|_| bad())?;
            if den == 0.0 {
                return Err(format!("{word:?} divides by zero"));
            }
            num / den
        }
        None => word.parse().map_err(|_| bad())?,
    };
    if !(value.is_finite() && value > 0.0) {
        return Err(format!("{word:?} is not a positive length"));
    }
    Ok(value)
}

/// Read every `.mix` file in `dir`. Missing directory is no files, not an error.
///
/// A file that will not parse is reported and skipped rather than taken down with it: one
/// bad mix of your own should cost you that mix, not the player.
pub fn read_dir(dir: &Path) -> (Vec<Recipe>, Vec<Problem>) {
    let mut recipes = Vec::new();
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (recipes, problems);
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "mix"))
        .collect();
    // Deterministic order, so two files that both define a key resolve the same way twice.
    paths.sort();
    for path in paths {
        let source = path.display().to_string();
        let Some(key) = key_from_path(&path) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse(&source, &key, &text) {
                Ok(mut recipe) => {
                    recipe.origin = source;
                    recipes.push(recipe);
                }
                Err(problem) => problems.push(problem),
            },
            Err(e) => problems.push(Problem {
                source,
                line: 0,
                message: format!("cannot read it: {e}"),
            }),
        }
    }
    (recipes, problems)
}

/// `long-blend.mix` names the recipe `long_blend`, so a file can be swapped for one of the
/// built-ins simply by giving it the same name.
pub fn key_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(stem.trim().to_lowercase().replace([' ', '-'], "_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "\
name  Test
lane out gain
    at 0.0  1.0  power-down
    at 1.0  0.0  hold
lane in gain
    at 0.0  0.0  power-up
    at 1.0  1.0  hold
";

    fn parse_ok(text: &str) -> Recipe {
        parse("test", "test", text).expect("should parse")
    }

    fn parse_err(text: &str) -> Problem {
        parse("test", "test", text).expect_err("should not parse")
    }

    #[test]
    fn a_minimal_score_is_two_faders() {
        let recipe = parse_ok(MINIMAL);
        assert_eq!(recipe.key, "test");
        assert_eq!(recipe.label, "Test");
        assert_eq!(recipe.lanes.len(), 2);
        assert!(
            recipe.constant_power,
            "a score is a blend unless it says otherwise"
        );
        assert_eq!(recipe.bars, None);
    }

    #[test]
    fn every_built_in_reads() {
        // The shipped transitions go through this parser like anyone else's, so a mistake
        // in one is caught here rather than at a party.
        for (key, text) in crate::transitions::built_in() {
            let recipe = parse(&format!("built-in {key}"), key, text)
                .unwrap_or_else(|problem| panic!("{problem}"));
            assert_eq!(&recipe.key, key);
            assert!(!recipe.label.is_empty(), "{key} has no name");
            assert!(!recipe.note.is_empty(), "{key} has no note");
        }
    }

    #[test]
    fn a_mistake_names_its_line_and_says_what_it_wanted() {
        // The person reading the error is the person who typed it.
        // MINIMAL is seven lines, so the bad lane is the eighth.
        let problem = parse_err(&format!("{MINIMAL}lane out wobble\n    at 0.0 1.0 hold\n"));
        assert_eq!(problem.line, 8, "the line number must point at the mistake");
        assert!(problem.message.contains("wobble"), "{}", problem.message);
        assert!(
            problem.message.contains("gain"),
            "does not say what is allowed"
        );

        let problem = parse_err(&format!("{MINIMAL}lane out bass\n    at 0.0 1.0 wobble\n"));
        assert!(problem.message.contains("ease"), "{}", problem.message);

        // An ease is the difference between a fade and a drop, so it is never guessed.
        let problem = parse_err(&format!("{MINIMAL}lane out bass\n    at 0.0 1.0\n"));
        assert!(problem.message.contains("ease"), "{}", problem.message);
    }

    #[test]
    fn the_rules_that_matter_are_refused_at_read_time() {
        // No fader at all: the deck would play at full volume throughout.
        let problem = parse_err("name X\nlane out gain\n  at 0.0 1.0 hold\n  at 1.0 0.0 hold\n");
        assert!(problem.message.contains("arriving"), "{}", problem.message);

        // Endpoints the wrong way round strand a deck for the rest of the night.
        let problem = parse_err(
            "name X\nlane out gain\n  at 0.0 1.0 hold\n  at 1.0 0.5 hold\n\
             lane in gain\n  at 0.0 0.0 hold\n  at 1.0 1.0 hold\n",
        );
        assert!(problem.message.contains("end at 0"), "{}", problem.message);

        // Keys out of order read the wrong pair, silently.
        let problem = parse_err(&format!(
            "{MINIMAL}lane out bass\n  at 0.5 0 hold\n  at 0.2 -40 hold\n"
        ));
        assert!(problem.message.contains("before it"), "{}", problem.message);

        // Counted in bars but written in fractions is a contradiction, not a default.
        let problem = parse_err(&format!("bars 16\n{MINIMAL}"));
        assert!(
            problem.message.contains("bars is set"),
            "{}",
            problem.message
        );

        // Two lanes for one control would silently ignore one of them.
        let problem = parse_err(&format!(
            "{MINIMAL}lane out bass\n  at 0.0 0 hold\n\
             lane out bass\n  at 0.0 0 hold\n"
        ));
        assert!(
            problem.message.contains("already has a lane"),
            "{}",
            problem.message
        );
    }

    #[test]
    fn comments_and_blank_lines_are_not_content() {
        let recipe = parse_ok(&format!("# a comment\n\n{MINIMAL}\n   # indented\n"));
        assert_eq!(recipe.lanes.len(), 2);
        // ...and a comment at the end of a real line is still a comment.
        let recipe = parse_ok(
            "name Test # not part of the name\nlane out gain\n\
             at 0.0 1.0 hold\n at 1.0 0.0 hold\nlane in gain\n at 0.0 0.0 hold\n\
             at 1.0 1.0 hold\n",
        );
        assert_eq!(recipe.label, "Test");
    }

    #[test]
    fn hooks_are_read_with_their_moment_and_their_action() {
        let recipe = parse_ok(&format!(
            "{MINIMAL}hook at 0.5 effect Echo on\nhook at 1.0 effect Echo off\n\
             hook at 0.25 toast here it comes\n"
        ));
        assert_eq!(recipe.hooks.len(), 3);
        assert_eq!(
            recipe.hooks[0].action,
            HookAction::Effect {
                name: "Echo".to_string(),
                on: true
            }
        );
        // A toast keeps its spaces.
        assert_eq!(
            recipe.hooks[2].action,
            HookAction::Toast("here it comes".to_string())
        );
        let problem = parse_err(&format!("{MINIMAL}hook at 0.5 explode\n"));
        assert!(problem.message.contains("explode"), "{}", problem.message);
    }

    #[test]
    fn every_expands_a_gate_into_alternating_keys() {
        let recipe = parse_ok(
            "name X\nbars 16\nlane out gain\n  bar 0 1.0 hold\n\
             every 1/16 from bar 12 to bar 14   1.0 0.0   hold\n  bar 16 0.0 hold\n\
             lane in gain\n  bar 0 0.0 hold\n  bar 16 1.0 hold\n",
        );
        let gate: Vec<&Key> = recipe.lanes[0]
            .keys
            .iter()
            .filter(|key| matches!(key.at, At::Bar(bar) if (12.0..14.0).contains(&bar)))
            .collect();
        // Two bars at a sixteenth of a bar per pulse: exactly the "thirty-two hand-written
        // keyframes" `every` exists to replace.
        assert_eq!(gate.len(), 32, "{gate:?}");
        assert_eq!(gate[0].value, 1.0);
        assert_eq!(gate[1].value, 0.0);
        assert_eq!(gate[31].value, 0.0);
        assert!(gate.iter().all(|key| key.ease == Ease::Hold));
        // First and last bar are still the keys written by hand around it, in order.
        assert_eq!(recipe.lanes[0].keys.first().unwrap().at, At::Bar(0.0));
        assert_eq!(recipe.lanes[0].keys.last().unwrap().at, At::Bar(16.0));
    }

    #[test]
    fn every_is_refused_when_it_cannot_mean_anything() {
        let base = "name X\nbars 16\nlane out gain\n  bar 0 1.0 hold\n";
        let tail = "\n  bar 16 0.0 hold\nlane in gain\n  bar 0 0.0 hold\n  bar 16 1.0 hold\n";

        // A step in fractions has no bar to be a fraction of.
        let text = format!("{base}every 1/16 from at 0.5 to at 0.9   1.0 0.0   hold{tail}");
        assert!(parse_err(&text).message.contains("bars"));

        // A range that runs backwards, or not at all.
        let text = format!("{base}every 1/16 from bar 14 to bar 12   1.0 0.0   hold{tail}");
        assert!(parse_err(&text).message.contains("never starts"));

        // Only one value to alternate between is not a pulse - it reads the ease word
        // where the second value should be and rejects that instead, which still refuses
        // the file rather than accepting a gate with no low value.
        let text = format!("{base}every 1/16 from bar 12 to bar 14   1.0   hold{tail}");
        assert!(parse_err(&text).message.contains("not a number"));

        // Genuinely nothing after the first value.
        let text = format!("{base}every 1/16 from bar 12 to bar 14   1.0{tail}");
        assert!(parse_err(&text).message.contains("two values"));
    }

    #[test]
    fn a_loop_roll_names_its_deck_and_shrinks_towards_off() {
        let recipe = parse_ok(&format!(
            "{MINIMAL}hook at 0.5 loop out 1/2\nhook at 0.6 loop out 1/4\n\
             hook at 0.7 loop off\n"
        ));
        assert_eq!(recipe.hooks.len(), 3);
        assert_eq!(
            recipe.hooks[0].action,
            HookAction::Loop {
                deck: Some(Deck::Outgoing),
                bars: Some(0.5)
            }
        );
        assert_eq!(
            recipe.hooks[1].action,
            HookAction::Loop {
                deck: Some(Deck::Outgoing),
                bars: Some(0.25)
            }
        );
        // Off names no deck: a score should not have to remember which one was rolling.
        assert_eq!(
            recipe.hooks[2].action,
            HookAction::Loop {
                deck: None,
                bars: None
            }
        );
        let problem = parse_err(&format!("{MINIMAL}hook at 0.5 loop out sixteenth\n"));
        assert!(problem.message.contains("sixteenth"), "{}", problem.message);
        let problem = parse_err(&format!("{MINIMAL}hook at 0.5 loop sideways 1/2\n"));
        assert!(problem.message.contains("sideways"), "{}", problem.message);
    }

    #[test]
    fn a_moment_the_transition_never_reaches_is_refused() {
        // The nastiest of the lot, because everything about it looks right: the outgoing
        // gain does end at zero, and the endpoint check is satisfied. It just ends there
        // four bars after the transition has finished, so in practice the deck is cut off
        // at full volume every single time.
        let past = "name X\nbars 16\nlane out gain\n  bar 0 1.0 hold\n  bar 20 0.0 hold\n\
                    lane in gain\n  bar 0 0.0 hold\n  bar 16 1.0 hold\n";
        let problem = parse_err(past);
        assert!(problem.message.contains("bar 20"), "{}", problem.message);
        assert!(problem.message.contains("never run"), "{}", problem.message);

        // The same mistake in fractions is caught earlier and more helpfully, because
        // `at 1.4` is wrong on its face where `bar 20` needs the rest of the file to know.
        let past = format!("{MINIMAL}lane in lowpass\n  at 0.0 500 hold\n  at 1.4 0 hold\n");
        let problem = parse_err(&past);
        assert!(
            problem.message.contains("past the end"),
            "{}",
            problem.message
        );
        assert!(problem.message.contains("bar 1.4"), "{}", problem.message);

        // A hook nobody will ever reach is the same fault wearing a different hat.
        let past = "name X\nbars 8\nlane out gain\n  bar 0 1.0 hold\n  bar 8 0.0 hold\n\
                    lane in gain\n  bar 0 0.0 hold\n  bar 8 1.0 hold\n\
                    hook bar 12 toast too late\n";
        assert!(parse_err(past).message.contains("never fire"));

        // And the boundary itself is fine: a key exactly on the last bar runs.
        let edge = "name X\nbars 8\nlane out gain\n  bar 0 1.0 hold\n  bar 8 0.0 hold\n\
                    lane in gain\n  bar 0 0.0 hold\n  bar 8 1.0 hold\n\
                    hook bar 8 toast just in time\n";
        assert_eq!(parse_ok(edge).hooks.len(), 1);
    }

    #[test]
    fn an_effect_switched_on_and_never_off_is_refused() {
        // The effect rack belongs to the player, not to the transition. Something switched
        // on here and not switched off again stays on for every track after it.
        let stuck = format!("{MINIMAL}hook at 0.2 effect Echo on\n");
        let problem = parse_err(&stuck);
        assert!(problem.message.contains("Echo"), "{}", problem.message);
        assert!(problem.message.contains("never off"), "{}", problem.message);

        // Switched off again is fine, and so is off-then-on-then-off.
        let paired = format!("{MINIMAL}hook at 0.2 effect Echo on\nhook at 0.8 effect Echo off\n");
        assert_eq!(parse_ok(&paired).hooks.len(), 2);

        // Case is not the difference between on and off.
        let cased = format!("{MINIMAL}hook at 0.2 effect Echo on\nhook at 0.8 effect echo off\n");
        assert_eq!(parse_ok(&cased).hooks.len(), 2);
    }

    #[test]
    fn sync_is_one_intent_and_cannot_be_declared_two_ways() {
        // A `tempo` lane is what asks for the arriving track to be pulled to the playing
        // one's speed, because it is also the only thing that lets it back. Deriving the
        // flag from the lane means the two halves of a sync cannot drift apart - which
        // they had, in a shipped file that claimed a match and never released it.
        let with_lane = format!("{MINIMAL}lane in tempo\n  at 0.0 0.0 hold\n  at 1.0 1.0 hold\n");
        assert!(
            parse_ok(&with_lane).tempo_match,
            "a tempo lane means a tempo match"
        );
        assert!(!parse_ok(MINIMAL).tempo_match, "no lane means no match");

        // Saying it as well is allowed, as documentation...
        assert!(parse_ok(&format!("tempo match\n{with_lane}")).tempo_match);
        assert!(!parse_ok(&format!("tempo free\n{MINIMAL}")).tempo_match);

        // ...but disagreeing is refused, in both directions.
        let problem = parse_err(&format!("tempo match\n{MINIMAL}"));
        assert!(
            problem.message.contains("never let back"),
            "{}",
            problem.message
        );
        let problem = parse_err(&format!("tempo free\n{with_lane}"));
        assert!(
            problem.message.contains("one of the two is wrong"),
            "{}",
            problem.message
        );
    }

    #[test]
    fn a_moment_is_a_bar_or_a_fraction_and_never_both() {
        let recipe = parse_ok(&format!(
            "bars 8\n{MINIMAL}lane out bass\n  bar 0 0 smooth\n  bar 8 -40 hold\n"
        ));
        assert_eq!(recipe.bars, Some(8.0));
        assert_eq!(recipe.lanes[2].keys[0].at, At::Bar(0.0));
        // A fraction past the end is almost always a bar that forgot to say so.
        let problem = parse_err(&format!("{MINIMAL}lane out bass\n  at 4 0 hold\n"));
        assert!(problem.message.contains("bar 4"), "{}", problem.message);
    }

    #[test]
    fn a_file_name_is_the_key_however_it_is_written() {
        assert_eq!(
            key_from_path(Path::new("/x/Long-Blend.mix")).as_deref(),
            Some("long_blend")
        );
        assert_eq!(
            key_from_path(Path::new("my mix.mix")).as_deref(),
            Some("my_mix")
        );
    }
}
