//! User-audible audio shaping, all through mpv's `af` chain: a registry of toggleable
//! ffmpeg filters (the `e` menu) plus the crossfade envelope math.
//!
//! An effect is one entry in [`ALL`]: a name, a menu note, and an ffmpeg filter string.
//! Adding an effect is adding a line. Everything enabled joins into a single
//! `lavfi=[...]` node placed *before* the visualizer tap, so the scopes measure what the
//! ears actually get. Filters shape playback only - downloads and the stream recorder
//! read the demuxer, upstream of every filter.
//!
//! The crossfade curve used to live here too. It moved to [`crate::transitions`] when it
//! stopped being one curve and became a choice of five: two implementations of the same
//! equal-power maths, in two files, is one more than anybody can keep honest.

pub struct Effect {
    pub name: &'static str,
    /// One-line description shown in the menu.
    pub note: &'static str,
    /// ffmpeg filter syntax, as it appears inside `lavfi=[...]`.
    pub filter: &'static str,
}

/// The rack, in the order the menu lists it: delays and reverbs, then modulation,
/// then the gates, then the destructive ones, then tone, then dynamics, then rate.
///
/// Grouped by what a hand reaches for rather than alphabetically, because a rack this
/// long is scrolled and the thing next to what you wanted should be the thing you
/// might have wanted instead. Every filter string here is checked against a real
/// ffmpeg; mpv accepts an `af` it cannot parse and reports success, so a typo in one
/// of these is silent, and silence is the one failure this list cannot afford.
pub const ALL: &[Effect] = &[
    Effect {
        name: "Echo",
        note: "half-second slapback",
        filter: "aecho=0.8:0.9:500:0.3",
    },
    Effect {
        name: "Slapback",
        note: "one tight repeat, rockabilly short",
        filter: "aecho=0.8:0.7:120:0.35",
    },
    Effect {
        name: "Ping pong",
        note: "repeats bouncing across the stereo",
        filter: "aecho=0.8:0.9:250|500:0.4|0.25",
    },
    Effect {
        name: "Tape delay",
        note: "long feedback, darkening as it goes",
        filter: "aecho=0.8:0.88:600|1200:0.5|0.3,lowpass=f=6000",
    },
    Effect {
        name: "Reverb",
        note: "small-room reflections",
        filter: "aecho=0.8:0.88:40|80|120|160:0.5|0.35|0.25|0.18",
    },
    Effect {
        name: "Cathedral",
        note: "a very large room indeed",
        filter: "aecho=0.8:0.9:500|1000|1500|2000:0.5|0.4|0.3|0.2",
    },
    Effect {
        name: "Flanger",
        note: "jet-engine sweep",
        filter: "flanger=delay=5:depth=2:speed=0.5",
    },
    Effect {
        name: "Phaser",
        note: "slow notched sweep",
        filter: "aphaser=type=t:speed=0.5:decay=0.4",
    },
    Effect {
        name: "Chorus",
        note: "thickens it into two of itself",
        filter: "chorus=0.7:0.9:55:0.4:0.25:2",
    },
    Effect {
        name: "Doubler",
        note: "a tight double, not an effect",
        filter: "chorus=0.6:0.9:40:0.3:0.2:1",
    },
    Effect {
        name: "Tremolo",
        note: "volume pulsing on the beat",
        filter: "tremolo=f=6:d=0.7",
    },
    Effect {
        name: "Vibrato",
        note: "pitch wobble",
        filter: "vibrato=f=6:d=0.5",
    },
    Effect {
        name: "Trans",
        note: "hard square gate, four to the bar",
        filter: "apulsator=hz=4:mode=square",
    },
    Effect {
        name: "Stutter",
        note: "the same gate, twice as fast",
        filter: "apulsator=hz=8:mode=square",
    },
    Effect {
        name: "Auto pan",
        note: "swinging left to right",
        filter: "apulsator=hz=1.5:mode=sine",
    },
    Effect {
        name: "8D",
        note: "slow left-right orbit",
        filter: "apulsator=hz=0.125",
    },
    Effect {
        name: "Crush",
        note: "6-bit, for when it should hurt",
        filter: "acrusher=level_in=1:level_out=1:bits=6:mode=log:aa=1",
    },
    Effect {
        name: "Lo-fi",
        note: "4-bit and no top end",
        filter: "acrusher=bits=4:mode=lin:aa=1,lowpass=f=6000",
    },
    Effect {
        name: "Telephone",
        note: "band-limited to a phone line",
        filter: "highpass=f=800,lowpass=f=2500",
    },
    Effect {
        name: "Megaphone",
        note: "distorted and shouted through a cone",
        filter: "highpass=f=500,lowpass=f=4000,acrusher=bits=8:mode=log:aa=1",
    },
    Effect {
        name: "Radio",
        note: "AM band, squashed flat",
        filter: "highpass=f=300,lowpass=f=3400,acompressor=threshold=0.1:ratio=4",
    },
    Effect {
        name: "Low cut",
        note: "bottom taken out, DJ filter up",
        filter: "highpass=f=400",
    },
    Effect {
        name: "High cut",
        note: "top taken off, DJ filter down",
        filter: "lowpass=f=1200",
    },
    Effect {
        name: "Sub bass",
        note: "+12 dB under 60 Hz",
        filter: "bass=g=12:f=60",
    },
    Effect {
        name: "Bass boost",
        note: "+8 dB low shelf",
        filter: "bass=g=8:f=110",
    },
    Effect {
        name: "Air",
        note: "+8 dB of top, for dull masters",
        filter: "treble=g=8:f=12000",
    },
    Effect {
        name: "Wide",
        note: "stereo pushed outwards",
        filter: "extrastereo=m=2.5",
    },
    Effect {
        name: "Mono",
        note: "both channels folded together",
        filter: "pan=stereo|c0=0.5*c0+0.5*c1|c1=0.5*c0+0.5*c1",
    },
    Effect {
        name: "Karaoke",
        note: "cuts centre-panned vocals",
        filter: "pan=stereo|c0=0.5*c0-0.5*c1|c1=0.5*c1-0.5*c0",
    },
    Effect {
        name: "Pump",
        note: "hard compression, sidechain feel",
        filter: "acompressor=threshold=0.05:ratio=20:attack=1:release=120",
    },
    Effect {
        name: "Glue",
        note: "gentle bus compression",
        filter: "acompressor=threshold=0.1:ratio=4:attack=20:release=250",
    },
    Effect {
        name: "Level",
        note: "evens a quiet master out",
        filter: "dynaudnorm=f=200:g=5",
    },
    Effect {
        name: "Saturate",
        note: "pushed into soft clipping",
        filter: "acontrast=50",
    },
    Effect {
        name: "Gate",
        note: "silence between the loud parts",
        filter: "agate=threshold=0.02:ratio=4",
    },
    Effect {
        name: "De-ess",
        note: "takes the hiss off the s sounds",
        filter: "deesser=i=0.4",
    },
    Effect {
        name: "Nightcore",
        note: "125% rate - faster and higher",
        filter: "aresample=48000,asetrate=60000",
    },
    Effect {
        name: "Screwed",
        note: "75% rate - slower and lower",
        filter: "aresample=48000,asetrate=36000",
    },
    Effect {
        name: "Half speed",
        note: "an octave down, tape-style",
        filter: "aresample=48000,asetrate=24000",
    },
];

/// The enabled effects as one mpv `af` node, or `None` when everything is off.
pub fn chain(on: &[bool]) -> Option<String> {
    let filters: Vec<&str> = ALL
        .iter()
        .zip(on)
        .filter(|(_, on)| **on)
        .map(|(effect, _)| effect.filter)
        .collect();
    if filters.is_empty() {
        None
    } else {
        Some(format!("lavfi=[{}]", filters.join(",")))
    }
}

/// Whether a track is long enough to be worth overlapping at all.
///
/// A transition that eats most of a track is not a crossfade, it is a mess: a jingle
/// shorter than twice the overlap is played straight through.
pub fn long_enough_to_cross(duration: Option<f64>, overlap: f64) -> bool {
    duration.is_some_and(|d| d >= overlap * 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_joins_enabled_filters_in_registry_order() {
        assert_eq!(chain(&vec![false; ALL.len()]), None);
        assert_eq!(chain(&[]), None);
        // The first two entries, whatever they are: the contract under test is "one
        // lavfi node, registry order", not which effects happen to be at the top.
        let mut on = vec![false; ALL.len()];
        on[0] = true;
        on[1] = true;
        assert_eq!(
            chain(&on).unwrap(),
            format!("lavfi=[{},{}]", ALL[0].filter, ALL[1].filter),
            "one lavfi node, registry order"
        );
    }

    #[test]
    fn the_rack_is_distinct_and_says_what_each_one_is() {
        let mut names: Vec<&str> = ALL.iter().map(|e| e.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two effects share a name");
        for effect in ALL {
            assert!(!effect.note.is_empty(), "{} has no note", effect.name);
            assert!(!effect.filter.is_empty(), "{} has no filter", effect.name);
            // Nothing here may carry the node wrapper: `chain` adds exactly one, and a
            // stray `lavfi=[` inside it would nest and be silently ignored by mpv.
            assert!(
                !effect.filter.contains("lavfi="),
                "{} brings its own node wrapper",
                effect.name
            );
        }
    }

    #[test]
    fn short_tracks_are_played_straight_through() {
        assert!(long_enough_to_cross(Some(120.0), 8.0));
        assert!(
            long_enough_to_cross(Some(16.0), 8.0),
            "exactly twice still counts"
        );
        assert!(!long_enough_to_cross(Some(15.0), 8.0));
        // A stream with no known length is never overlapped: there is no end to aim at.
        assert!(!long_enough_to_cross(None, 8.0));
    }
}
