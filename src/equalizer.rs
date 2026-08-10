//! The equalizer: a fixed five-band shape, chosen as a preset rather than dragged.
//!
//! Deliberately presets and not sliders. A graphic EQ with five faders in a terminal is
//! five more things to hold, and the answer a listener actually wants is "make this sound
//! right on these speakers", not "set 3.5 kHz to +4". So the bands are fixed, the shapes
//! are named for the situation they are for, and picking one is a single keypress.
//!
//! Everything here emits the same filter vocabulary [`crate::transitions`] already checks
//! against a real mpv - `bass`, `treble`, `equalizer` - so there is one set of forms in
//! the binary and one place they are known to be accepted. It joins the `af` chain ahead
//! of the effects rack and the visualizer tap: tone shaping first, effects on top of it,
//! and the scopes measuring what the ears actually get.

/// The bands, low to high. Fixed: a preset is five numbers against this list, and the
/// view draws that list, so adding a band is one line here and nothing anywhere else.
///
/// The ends are shelves and the middle three are peaks, which is what a graphic EQ is:
/// shelving the extremes means "everything below/above this", where peaking the middle
/// means "this, and not its neighbours".
pub const BANDS: [Band; 5] = [
    Band {
        hz: 100,
        label: "100",
        shape: Shape::LowShelf,
    },
    Band {
        hz: 300,
        label: "300",
        shape: Shape::Peak { width: 200 },
    },
    Band {
        hz: 1000,
        label: "1k",
        shape: Shape::Peak { width: 700 },
    },
    Band {
        hz: 3500,
        label: "3.5k",
        shape: Shape::Peak { width: 2500 },
    },
    Band {
        hz: 10000,
        label: "10k",
        shape: Shape::HighShelf,
    },
];

/// The most any band may be moved, either way. Wider than this and the preamp below
/// would be taking back more than it is worth keeping.
pub const MAX_GAIN: i8 = 12;

pub struct Band {
    pub hz: u32,
    /// Short enough to sit under a bar in the view.
    pub label: &'static str,
    shape: Shape,
}

enum Shape {
    LowShelf,
    HighShelf,
    /// Peaking, with its width in Hz - the same form the transition language uses.
    Peak {
        width: u32,
    },
}

/// One named shape: five gains in dB, low band first.
pub struct Preset {
    pub name: &'static str,
    /// One line for the menu, saying what it is *for* rather than what it does.
    pub note: &'static str,
    pub gains: [i8; BANDS.len()],
}

impl Preset {
    /// Stable config token, derived from the name so the two cannot drift apart.
    pub fn key(&self) -> String {
        self.name.to_lowercase().replace(' ', "_")
    }

    /// How much to pull the whole chain down before boosting anything.
    ///
    /// A `+8 dB` shelf on a master already mixed to within an inch of full scale does not
    /// produce a louder bass, it produces a clipped one - the boost has nowhere to go.
    /// Every EQ worth using takes some of it back first, and this is the cheap version of
    /// that: a little over half the largest boost, which keeps the peaks in range without
    /// gutting the level. Cuts need none of it, so a preset that only cuts gets none.
    pub fn preamp_db(&self) -> i8 {
        let boost = self.gains.iter().copied().max().unwrap_or(0).max(0);
        -((f32::from(boost) * 0.6).round() as i8)
    }

    /// This preset as an mpv `af` node, or `None` when it does nothing at all.
    ///
    /// Bands at zero are left out rather than emitted at `g=0`: a filter that does
    /// nothing still costs a biquad per sample per channel, and a chain that says what it
    /// is doing is one that can be read in `af` when something looks wrong.
    pub fn filter(&self) -> Option<String> {
        if self.gains.iter().all(|g| *g == 0) {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        let preamp = self.preamp_db();
        if preamp != 0 {
            parts.push(format!("volume={preamp}dB"));
        }
        for (band, gain) in BANDS.iter().zip(self.gains) {
            let gain = gain.clamp(-MAX_GAIN, MAX_GAIN);
            if gain == 0 {
                continue;
            }
            parts.push(match band.shape {
                Shape::LowShelf => format!("bass=g={gain}:f={}", band.hz),
                Shape::HighShelf => format!("treble=g={gain}:f={}", band.hz),
                Shape::Peak { width } => {
                    format!("equalizer=f={}:t=h:w={width}:g={gain}", band.hz)
                }
            });
        }
        Some(format!("lavfi=[{}]", parts.join(",")))
    }
}

/// Every preset, in the order the view lists and the key cycles them.
///
/// `Flat` is first and is a true no-op - not a flat curve run through five filters, but
/// no filters at all - so the default costs exactly nothing and "off" is unambiguous.
pub const ALL: &[Preset] = &[
    Preset {
        name: "Flat",
        note: "no filter at all — the track as it was mastered",
        gains: [0, 0, 0, 0, 0],
    },
    Preset {
        name: "Club",
        note: "big low end, scooped mids, hard top — a room with a system in it",
        gains: [6, -2, -3, 2, 4],
    },
    Preset {
        name: "Deep",
        note: "sub and body forward, everything above it out of the way",
        gains: [8, 2, -2, -1, 0],
    },
    Preset {
        name: "Warm",
        note: "body up and the edge off — long listening, harsh masters",
        gains: [4, 2, 0, -2, -3],
    },
    Preset {
        name: "Bright",
        note: "air and presence, low end thinned — dull recordings, dark rooms",
        gains: [-2, -1, 0, 3, 6],
    },
    Preset {
        name: "Vocal",
        note: "mud out, words forward — podcasts, live sets, anything spoken",
        gains: [-3, -3, 3, 4, 1],
    },
    Preset {
        name: "Loudness",
        note: "the smile curve — for listening quietly, where the ear loses the ends",
        gains: [7, 0, -3, 0, 6],
    },
    Preset {
        name: "Laptop",
        note: "small speakers have no bottom, so imply one in the low mids instead",
        gains: [-6, 5, 3, 2, 0],
    },
    Preset {
        name: "Late night",
        note: "keeps the detail, loses the neighbours",
        gains: [-8, -2, 1, 2, -1],
    },
];

/// The preset at `index`, wrapping - the index is what the settings file and the view
/// both hold, and it is always in range by construction.
pub fn at(index: usize) -> &'static Preset {
    &ALL[index % ALL.len()]
}

/// Which preset `key` names, for reading the config back.
pub fn parse(key: &str) -> Option<usize> {
    let wanted = key.trim().to_lowercase().replace(' ', "_");
    ALL.iter().position(|preset| preset.key() == wanted)
}

/// The chain for the preset at `index`, or `None` for one that does nothing.
pub fn chain(index: usize) -> Option<String> {
    at(index).filter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_is_nothing_at_all_rather_than_five_filters_doing_nothing() {
        assert_eq!(ALL[0].name, "Flat");
        assert_eq!(chain(0), None);
        assert_eq!(ALL[0].preamp_db(), 0);
    }

    #[test]
    fn every_preset_emits_only_the_checked_vocabulary() {
        // mpv accepts an `af` string it cannot parse and reports success, so a typo here
        // is silent. Every form emitted must be one already run through a real mpv.
        for (i, preset) in ALL.iter().enumerate() {
            let Some(chain) = chain(i) else { continue };
            assert!(
                chain.starts_with("lavfi=[") && chain.ends_with(']'),
                "{} is not an mpv filter node: {chain}",
                preset.name
            );
            for part in chain
                .trim_start_matches("lavfi=[")
                .trim_end_matches(']')
                .split(',')
            {
                let known = part.starts_with("volume=")
                    || part.starts_with("bass=g=")
                    || part.starts_with("treble=g=")
                    || part.starts_with("equalizer=f=");
                assert!(known, "{} emits an unchecked form: {part}", preset.name);
            }
        }
    }

    #[test]
    fn a_boost_is_paid_for_and_a_cut_is_not() {
        for preset in ALL {
            let boost = preset.gains.iter().copied().max().unwrap_or(0);
            let preamp = preset.preamp_db();
            if boost <= 0 {
                assert_eq!(
                    preamp, 0,
                    "{} only cuts, so it has nothing to make room for",
                    preset.name
                );
            } else {
                assert!(
                    preamp < 0 && i16::from(preamp.abs()) <= i16::from(boost),
                    "{}: a {boost} dB boost took back {preamp} dB",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn presets_are_distinct_and_within_range() {
        let mut keys: Vec<String> = ALL.iter().map(Preset::key).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "two presets share a config key");
        for preset in ALL {
            for gain in preset.gains {
                assert!(
                    gain.abs() <= MAX_GAIN,
                    "{} moves a band {gain} dB, past the {MAX_GAIN} dB the preamp is \
                     sized for",
                    preset.name
                );
            }
        }
        // Round-trips through the config, by the key the name derives from.
        for (i, preset) in ALL.iter().enumerate() {
            assert_eq!(parse(&preset.key()), Some(i));
            assert_eq!(parse(preset.name), Some(i));
        }
        assert_eq!(parse("not a preset"), None);
    }
}
