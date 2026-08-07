//! User-audible audio shaping, all through mpv's `af` chain: a registry of toggleable
//! ffmpeg filters (the `e` menu) plus the crossfade envelope math.
//!
//! An effect is one entry in [`ALL`]: a name, a menu note, and an ffmpeg filter string.
//! Adding an effect is adding a line. Everything enabled joins into a single
//! `lavfi=[...]` node placed *before* the visualizer tap, so the scopes measure what the
//! ears actually get. Filters shape playback only - downloads and the stream recorder
//! read the demuxer, upstream of every filter.

pub struct Effect {
    pub name: &'static str,
    /// One-line description shown in the menu.
    pub note: &'static str,
    /// ffmpeg filter syntax, as it appears inside `lavfi=[...]`.
    pub filter: &'static str,
}

pub const ALL: &[Effect] = &[
    Effect {
        name: "Echo",
        note: "half-second slapback",
        filter: "aecho=0.8:0.9:500:0.3",
    },
    Effect {
        name: "Reverb",
        note: "small-room reflections",
        filter: "aecho=0.8:0.88:40|80|120|160:0.5|0.35|0.25|0.18",
    },
    Effect {
        name: "Bass boost",
        note: "+8 dB low shelf",
        filter: "bass=g=8:f=110",
    },
    Effect {
        name: "Nightcore",
        note: "125% rate — faster and higher",
        // Normalized to 48 kHz first so the speed-up is +25% whatever the source rate.
        filter: "aresample=48000,asetrate=60000",
    },
    Effect {
        name: "Karaoke",
        note: "cuts centre-panned vocals",
        filter: "pan=stereo|c0=0.5*c0-0.5*c1|c1=0.5*c1-0.5*c0",
    },
    Effect {
        name: "8D",
        note: "slow left-right orbit",
        filter: "apulsator=hz=0.125",
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

/// Crossfade fade-out multiplier: `Some(0..=1)` while the track is inside its closing
/// fade window, `None` outside it. `half` is the fade length per side - the configured
/// crossfade time splits across the outgoing and incoming track. Tracks shorter than
/// twice the whole transition never fade; neither does the last track of the queue
/// (`has_next` false), which plays out clean.
pub fn fade_out(
    position: Option<f64>,
    duration: Option<f64>,
    half: f64,
    has_next: bool,
) -> Option<f64> {
    if !has_next || half <= 0.0 {
        return None;
    }
    let (position, duration) = (position?, duration?);
    if duration < half * 4.0 {
        return None;
    }
    let remaining = duration - position;
    (remaining < half).then(|| (remaining / half).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_joins_enabled_filters_in_registry_order() {
        assert_eq!(chain(&[false; 6]), None);
        assert_eq!(chain(&[]), None);
        let echo_bass = chain(&[true, false, true, false, false, false]).unwrap();
        assert_eq!(
            echo_bass, "lavfi=[aecho=0.8:0.9:500:0.3,bass=g=8:f=110]",
            "one lavfi node, registry order"
        );
    }

    #[test]
    fn fade_out_only_inside_the_tail_window_with_a_next_track() {
        // Mid-track: nothing.
        assert_eq!(fade_out(Some(10.0), Some(120.0), 2.5, true), None);
        // Inside the window: proportional.
        let m = fade_out(Some(118.75), Some(120.0), 2.5, true).unwrap();
        assert!((m - 0.5).abs() < 1e-9, "half-way through the fade: {m}");
        // Last track plays out clean.
        assert_eq!(fade_out(Some(118.75), Some(120.0), 2.5, false), None);
        // Jingles shorter than twice the transition never fade.
        assert_eq!(fade_out(Some(7.0), Some(8.0), 2.5, true), None);
        // Unknown position/duration: nothing to compute.
        assert_eq!(fade_out(None, Some(120.0), 2.5, true), None);
        assert_eq!(fade_out(Some(1.0), None, 2.5, true), None);
        // Past the end (mpv can report position > duration briefly): clamped, not negative.
        assert_eq!(fade_out(Some(121.0), Some(120.0), 2.5, true), Some(0.0));
    }
}
