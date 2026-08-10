//! Rhythm measurement for a track that is not playing yet, so a transition can open the
//! next deck already tempo-matched instead of discovering its tempo once it is audible.
//!
//! [`crate::audio_tap::Tap`] only ever hears mpv, and mpv only ever decodes what is
//! audible, so the track after this one is silent and unmeasured right up to the moment
//! it matters. The only way to know its tempo in advance is to decode a slice of it
//! separately, which is what this does: one ffmpeg process, raw mono f32 to a pipe, no
//! second mpv and nothing audible.
//!
//! It has to be the *same* measurement or the beat tracker sees a different instrument
//! for each deck. The tap builds sixteen libavfilter `bandpass` branches and reads each
//! one's RMS; this runs the same sixteen filters over the samples in Rust - the RBJ
//! cookbook bandpass that libavfilter's `bandpass` is, at the same centres and the same
//! 0.6-octave width - and maps dB to 0..1 through the same
//! [`crate::audio_tap::BAND_FLOOR`]. Checked against libavfilter's own output on real
//! material: per-band RMS agrees to 0.002 dB, which is 3e-5 of one unit of that mapping.
//! Filtering here rather than in a second filter graph is what makes the frame interval
//! exactly regular, and it avoids demultiplexing seventeen `ametadata` streams out of
//! one pipe.
//!
//! Two differences from the tap survive that and cannot be removed, both to do with
//! what mpv hands its filter chain. mpv filters the decoder's own output, so the tap
//! runs at the source's sample rate - 23.2 ms windows for a 44.1 kHz file, 21.3 ms for
//! a 48 kHz one - while a regular interval is the entire point here, so the decode is
//! pinned to 48 kHz and the band shapes shift by a fraction of a dB on material that is
//! not already 48 kHz. And the decode is forced to two channels, which is exactly what
//! the tap sees for the stereo the music actually is, but 3.01 dB below it for a
//! genuinely mono source, because ffmpeg's upmix is equal-power and mpv's is not.
//! Neither matters to what reads these numbers: the beat tracker works on the *rise*
//! between frames, and an offset that holds for a whole track differences away.
//!
//! Forcing *one* channel instead, which is the obvious thing to do for a measurement
//! that ends up as one number per band, is the one option that is genuinely wrong.
//! ffmpeg's stereo downmix is equal-power too, so it reads 3 dB hot on a mono-ish
//! passage and level on a wide one - an error that moves with the music rather than
//! sitting still under it, which is the kind an onset detector cannot ignore.
//!
//! ffmpeg is optional in this project, the target may be a URL that has moved, and this
//! runs on a thread the user is waiting on. So every failure is [`None`], every wait has
//! a deadline, and a truncated decode is returned as the shorter [`Frames`] it is
//! rather than thrown away.

use crate::audio_tap::{BAND_COUNT, BAND_FLOOR};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Band centres, mirroring the private table in [`crate::audio_tap`]: `50 *
/// (12000/50)^(i/15)`. Duplicated rather than shared because the two must be identical
/// for the levels to be comparable, and a copy that is wrong is at least visible here.
const FREQS: [u32; BAND_COUNT] = [
    50, 72, 104, 150, 216, 311, 448, 645, 929, 1338, 1927, 2775, 3996, 5754, 8286, 11932,
];

/// Bandpass width in octaves, matching the tap's `width_type=o:w=0.6`.
const BANDWIDTH: f64 = 0.6;

/// Silence floor for the full-band level, matching the tap's own: -60 dB maps to 0.0.
/// Private there, so it is restated here.
const FULL_FLOOR: f32 = 60.0;

/// Decode rate. Pinned rather than taken from the file, because the filter coefficients
/// depend on it and a measurement that changes with the container it came from is not
/// comparable to anything. High enough that the top band centre of 11932 Hz keeps its
/// shape well below Nyquist.
const RATE: u32 = 48_000;

/// Channels decoded. The tap measures whatever mpv's decoder produced, which for music
/// is a stereo pair, and its `astats` reads the mean power of the two. Forcing two here
/// reproduces that exactly for a stereo source; see the module note for what it costs on
/// the sources that are not.
const CHANNELS: usize = 2;

/// Shortest frame interval entertained: below this the window is too short to hold a
/// cycle of the lowest band and the level reads as noise.
const MIN_INTERVAL: f64 = 0.005;
/// Longest: an interval this long can swallow a whole beat at the top of the tempo
/// range, at which point there is no rhythm left to measure.
const MAX_INTERVAL: f64 = 0.5;

/// Most audio that will ever be decoded in one call. Ten minutes is far past the point
/// where more helps a tempo estimate; the cap exists so a wrong argument cannot turn
/// into an unbounded read.
const MAX_SECONDS: f64 = 600.0;

/// Fixed part of [`estimated_cost`]: process spawn, demuxer probe and the pipe teardown.
/// Straight-line fit over local files on a warm cache put this at 27 ms; the figure used
/// is deliberately larger, because the estimate is worth having only if it is an upper
/// bound on a machine that is also playing music.
const SPAWN_COST: Duration = Duration::from_millis(90);

/// Seconds of audio decoded, filtered and measured per second of wall clock. The same
/// fit measured 580; a fifth of that leaves room for a slower disk, a codec that costs
/// more than MP3, and a busy box. Sampling 30 s of a local file really takes about 90 ms
/// and is estimated at 340 ms.
const DECODE_SPEED: f64 = 120.0;

/// How long [`sample`] waits before killing ffmpeg. The generous fixed part is for the
/// network: a remote read spends its time in TCP, not in the decoder, and it is the one
/// case where the estimate says nothing useful. For scale, 30 s of a public http URL
/// took 0.33 s, and an address that black-holes packets gave up inside ffmpeg after 5 s.
const DEADLINE_BASE: Duration = Duration::from_secs(20);

/// Multiple of [`estimated_cost`] added to [`DEADLINE_BASE`], so a genuinely long
/// request is not cut off by a limit tuned for a short one.
const DEADLINE_SLACK: u32 = 4;

/// Socket read timeout handed to ffmpeg, in microseconds, so a stalled connection fails
/// inside ffmpeg rather than waiting for the deadline to kill it.
const RW_TIMEOUT_US: u64 = 8_000_000;

/// Band levels sampled from a track that is not playing.
///
/// `bands` and `rms` are quantised to `u8` rather than kept as the `f32` [`level`]
/// computes them as: three cached tracks' worth of a whole song is the shape this is
/// held in the longest ([`crate::measure::TrackFacts::frames`]), and the ear cannot
/// hear past 8-bit resolution on a level meter, so a quarter of the bytes buys nothing
/// back. [`dequantize`] undoes it at the one place anything reads a level - beat
/// tracking never sees the difference.
pub struct Frames {
    /// One entry per frame, oldest first: 16 octave-spaced band levels, low first.
    /// Quantised - see [`dequantize`].
    pub bands: Vec<[u8; BAND_COUNT]>,
    /// Full-band RMS per frame, same length as `bands`. Quantised - see [`dequantize`].
    pub rms: Vec<u8>,
    /// Seconds between frames. Regular, unlike the live tap.
    pub interval: f64,
    /// Seconds into the track the first frame covers, so a caller can convert a frame
    /// index into a track position: frame `i` sits at `start + i * interval`.
    ///
    /// Frames are contiguous windows measured from the beginning of the track, and this
    /// is the *centre* of the first one. A window's energy is spread across the whole of
    /// it, so its centre is the unbiased answer to when that energy happened; timing
    /// frames at their edges would push every beat half an interval out.
    pub start: f64,
}

/// Sample `seconds` of `target` (a path or an http(s) URL) at `interval` seconds per frame.
///
/// Blocking: it runs ffmpeg and waits. `None` when ffmpeg is missing, the target cannot be
/// opened, or nothing decodable came back - never an error the caller has to handle.
///
/// A target that turns out to be shorter than `seconds`, or a decode that runs past the
/// internal deadline, returns however many whole frames did arrive. Callers should treat
/// a short `Frames` as the honest length of what could be read, not as a failure.
/// The same, from `start` seconds into the target rather than from the top.
///
/// A grid is a first bar line and a spacing, so reading one far from where it will be used
/// means extrapolating: multiply a small error in the spacing by the number of bars in
/// between and it stops being small. A track measured from its opening and then asked
/// where its bar falls fifty seconds later is twenty-seven bars of extrapolation, and at
/// the tenth of a per cent the tempo is quantised to that is sixty-seven milliseconds -
/// not a subtle error, and entirely an artefact of where the question was asked from.
pub fn sample_from(target: &str, start: f64, seconds: f64, interval: f64) -> Option<Frames> {
    if target.is_empty() || !seconds.is_finite() || !interval.is_finite() || seconds <= 0.0 {
        return None;
    }
    let seconds = seconds.min(MAX_SECONDS);
    let interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);

    let start = if start.is_finite() {
        start.max(0.0)
    } else {
        0.0
    };
    let pcm = decode(target, start, seconds)?;
    let frames = measure(&pcm, interval);
    // An empty result is indistinguishable from a target that never opened, and both
    // mean the same thing to a caller: there is nothing here to beat-match against.
    (!frames.bands.is_empty()).then_some(frames)
}

/// How long [`sample`] is likely to take for `seconds` of audio, so a caller can decide
/// whether it has time.
///
/// Based on measured local decoding: a fixed [`SPAWN_COST`] for starting ffmpeg and
/// probing the file, plus the audio itself at [`DECODE_SPEED`] times real time. It says
/// nothing about the network, so an http(s) target can take considerably longer - up to
/// the deadline [`sample`] enforces, which is [`DEADLINE_BASE`] plus
/// [`DEADLINE_SLACK`] times this estimate.
pub fn estimated_cost(seconds: f64) -> Duration {
    // `f64::clamp` passes NaN straight through, and `Duration::from_secs_f64` panics on
    // it. A request that is not a number is one `sample` will refuse anyway.
    let seconds = if seconds.is_finite() {
        seconds.clamp(0.0, MAX_SECONDS)
    } else {
        0.0
    };
    SPAWN_COST + Duration::from_secs_f64(seconds / DECODE_SPEED)
}

/// The ffmpeg argument list, kept separate so it can be read and tested without running
/// anything.
///
/// `-t` appears on both sides on purpose: as an input option it stops a remote read
/// early, which is the whole cost of a URL, and as an output option it holds even for a
/// demuxer that ignores the first one.
fn args(target: &str, start: f64, seconds: f64) -> Vec<String> {
    let duration = format!("{seconds:.3}");
    let from = format!("{start:.3}");
    [
        "-hide_banner",
        "-loglevel",
        "error",
        // A TUI shares its stdin with ffmpeg's, and ffmpeg reads keys.
        "-nostdin",
        "-nostats",
        "-rw_timeout",
        &RW_TIMEOUT_US.to_string(),
        // Before `-i`, so ffmpeg seeks the container rather than decoding and discarding
        // everything up to the offset - the difference between a tenth of a second and
        // several, on a track that is most of an hour long.
        "-ss",
        &from,
        "-t",
        &duration,
        "-i",
        target,
        "-t",
        &duration,
        "-vn",
        "-sn",
        "-dn",
        "-map_metadata",
        "-1",
        "-f",
        "f32le",
        "-acodec",
        "pcm_f32le",
        "-ac",
        &CHANNELS.to_string(),
        "-ar",
        &RATE.to_string(),
        "-",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Run ffmpeg and collect its raw samples, or `None` if it could not be started.
///
/// stdout is drained by a second thread while this one watches the clock, because a
/// pipe that nobody reads stops ffmpeg and a child that nobody kills can outlive the
/// answer being useful. Whatever arrived before the deadline is kept.
fn decode(target: &str, start: f64, seconds: f64) -> Option<Vec<f32>> {
    let mut child = Command::new("ffmpeg")
        .args(args(target, start, seconds))
        .stdin(Stdio::null())
        // ffmpeg's diagnostics are not actionable here - every failure is the same
        // `None` - and a pipe nobody drains is one more thing that can wedge.
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;

    // Enough for the request plus a second of slack. Closing the pipe on overrun is what
    // stops a target whose duration lied, and ffmpeg exits on the broken pipe.
    let cap = ((seconds + 1.0) * f64::from(RATE)) as usize * CHANNELS * 4;
    let reader = std::thread::spawn(move || read_capped(&mut out, cap));

    let deadline = Instant::now() + DEADLINE_BASE + estimated_cost(seconds) * DEADLINE_SLACK;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // The child is gone, so the write end is closed and the reader has finished or is
    // about to. A panicked reader loses the samples, not the process.
    let bytes = reader.join().unwrap_or_default();
    Some(
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
    )
}

/// Read until end of stream or `cap` bytes, whichever comes first.
fn read_capped(source: &mut impl std::io::Read, cap: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cap.min(1 << 20));
    let mut chunk = [0u8; 65536];
    while buf.len() < cap {
        match source.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n.min(cap - buf.len())]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    buf
}

/// Turn interleaved stereo samples into per-frame levels.
///
/// Windows are contiguous and do not overlap, exactly as the tap's `asetnsamples`
/// windows are, so no onset can fall between two frames. The ragged tail is dropped: a
/// part-full window reads as a sudden quiet frame, which is the one thing an onset
/// detector must not be told.
///
/// The two statistics are gathered the two different ways the tap gathers them, which
/// are not interchangeable once the channels differ. A band is `astats`'s
/// `Overall.RMS_level`, the mean *power* of the pair; the full-band figure is each
/// channel's own dB normalised and then averaged, which is what the player does with
/// the tap's `rms` pair before handing it to the beat tracker.
fn measure(pcm: &[f32], interval: f64) -> Frames {
    let window = ((interval * f64::from(RATE)).round() as usize).max(1);
    let stride = window * CHANNELS;
    let count = pcm.len() / stride;
    let mut frames = Frames {
        bands: vec![[0; BAND_COUNT]; count],
        rms: vec![0; count],
        interval,
        start: interval / 2.0,
    };
    if count == 0 {
        return frames;
    }

    for (f, slot) in frames.rms.iter_mut().enumerate() {
        let (mut left, mut right) = (0.0, 0.0);
        for pair in pcm[f * stride..(f + 1) * stride].chunks_exact(CHANNELS) {
            left += f64::from(pair[0]) * f64::from(pair[0]);
            right += f64::from(pair[1]) * f64::from(pair[1]);
        }
        let n = window as f64;
        *slot = quantize((level(left / n, FULL_FLOOR) + level(right / n, FULL_FLOOR)) / 2.0);
    }

    // One band at a time over the whole signal, rather than sixteen filters per sample:
    // the filter state then stays in registers for the length of a pass.
    for (b, freq) in FREQS.iter().enumerate() {
        let mut left = Biquad::bandpass(f64::from(*freq), f64::from(RATE), BANDWIDTH);
        let mut right = Biquad::bandpass(f64::from(*freq), f64::from(RATE), BANDWIDTH);
        for f in 0..count {
            let mut power = 0.0;
            for pair in pcm[f * stride..(f + 1) * stride].chunks_exact(CHANNELS) {
                let l = left.step(f64::from(pair[0]));
                let r = right.step(f64::from(pair[1]));
                power += l * l + r * r;
            }
            frames.bands[f][b] = quantize(level(power / (window * CHANNELS) as f64, BAND_FLOOR));
        }
    }
    frames
}

/// A mean square in dB, normalised against `floor` the way [`crate::audio_tap`]
/// normalises the text libavfilter prints: `-floor` dB maps to 0.0, 0 dB to 1.0.
/// Digital silence is `-inf` dB and lands on 0.0.
fn level(mean_square: f64, floor: f32) -> f32 {
    if !(mean_square.is_finite() && mean_square > 0.0) {
        return 0.0;
    }
    let db = (10.0 * mean_square.log10()) as f32;
    if !db.is_finite() {
        return 0.0;
    }
    ((db + floor) / floor).clamp(0.0, 1.0)
}

/// `level`'s `0.0..=1.0` packed into a byte - see [`Frames`] for why.
fn quantize(level: f32) -> u8 {
    (level.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The other half of [`quantize`].
pub(crate) fn dequantize(level: u8) -> f32 {
    f32::from(level) / 255.0
}

/// One RBJ cookbook bandpass section with constant 0 dB peak gain, in the same direct
/// form and the same double precision libavfilter's `bandpass` uses, so that the two
/// produce the same numbers rather than merely similar ones.
struct Biquad {
    b0: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    /// Coefficients for a centre of `freq` Hz at `rate`, `octaves` wide.
    ///
    /// `b1` is zero for this shape and `b0 == -b2`, so neither is stored.
    fn bandpass(freq: f64, rate: f64, octaves: f64) -> Biquad {
        let w0 = 2.0 * std::f64::consts::PI * freq / rate;
        let (sin_w0, cos_w0) = (w0.sin(), w0.cos());
        let alpha = sin_w0 * (std::f64::consts::LN_2 / 2.0 * octaves * w0 / sin_w0).sinh();
        let a0 = 1.0 + alpha;
        Biquad {
            b0: alpha / a0,
            b2: -alpha / a0,
            a1: -2.0 * cos_w0 / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// One sample in, one sample out.
    fn step(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_table_matches_the_taps_geometric_spacing() {
        // The tap's table is `50 * (12000/50)^(i/15)` rounded by hand, so it is checked
        // for the property that matters - octave spacing, same ends - rather than
        // against the formula to the hertz.
        assert_eq!(FREQS[0], 50);
        assert_eq!(FREQS[BAND_COUNT - 1], 11932);
        for (i, f) in FREQS.iter().enumerate() {
            let ideal = 50.0 * (12000.0f64 / 50.0).powf(i as f64 / 15.0);
            let off = (f64::from(*f) - ideal).abs() / ideal;
            assert!(
                off < 0.01,
                "band {i} is {f}, {:.1}% off {ideal:.0}",
                off * 100.0
            );
            assert!(
                i == 0 || *f > FREQS[i - 1],
                "band {i} is not above band {}",
                i - 1
            );
        }
        assert!(
            f64::from(FREQS[BAND_COUNT - 1]) < f64::from(RATE) / 2.0,
            "top band is above Nyquist at the decode rate"
        );
    }

    #[test]
    fn db_normalisation_matches_the_taps_mapping() {
        // 0 dBFS is a mean square of 1: a full-scale square wave.
        assert_eq!(level(1.0, BAND_FLOOR), 1.0);
        assert_eq!(level(0.0, BAND_FLOOR), 0.0);
        assert_eq!(level(f64::NAN, BAND_FLOOR), 0.0);
        assert_eq!(level(-1.0, BAND_FLOOR), 0.0);
        // -60 dB is the full-band floor exactly, and -30 dB is halfway up it.
        assert!(level(10f64.powf(-6.0), FULL_FLOOR).abs() < 1e-6);
        assert!((level(10f64.powf(-3.0), FULL_FLOOR) - 0.5).abs() < 1e-6);
        // The band floor is deeper, so the same signal reads higher against it.
        assert!((level(10f64.powf(-3.2), BAND_FLOOR) - 0.5).abs() < 1e-6);
        // Above full scale clamps rather than reporting more than 1.0.
        assert_eq!(level(100.0, BAND_FLOOR), 1.0);
    }

    #[test]
    fn bandpass_passes_its_centre_and_rejects_an_octave_out() {
        let rate = f64::from(RATE);
        let gain = |centre: f64, tone: f64| {
            let mut filter = Biquad::bandpass(centre, rate, BANDWIDTH);
            let n = (rate as usize) / 2;
            let mut sum = 0.0;
            for i in 0..n {
                let x = (2.0 * std::f64::consts::PI * tone * i as f64 / rate).sin();
                let y = filter.step(x);
                // Skip the settling transient; the tail is the steady state.
                if i > n / 2 {
                    sum += y * y;
                }
            }
            (sum / (n / 2) as f64).sqrt() * 2f64.sqrt()
        };
        // Unity at the centre is what "constant 0 dB peak gain" means.
        assert!((gain(1000.0, 1000.0) - 1.0).abs() < 0.01);
        // Half power at the edges of the 0.6-octave band, by definition of the width.
        let half = 1.0 / 2f64.sqrt();
        assert!((gain(1000.0, 1000.0 * 2f64.powf(0.3)) - half).abs() < 0.02);
        assert!((gain(1000.0, 1000.0 * 2f64.powf(-0.3)) - half).abs() < 0.02);
        // Two octaves out is a second-order skirt down, near enough -19 dB.
        assert!(gain(1000.0, 4000.0) < 0.15, "skirt is too wide");
        assert!(gain(1000.0, 250.0) < 0.15, "skirt is too wide");
    }

    #[test]
    fn measure_windows_the_samples_regularly() {
        // Half a second of silence at a 50 ms interval is ten frames and no more; the
        // ragged tail of an eleventh is dropped rather than reported as a quiet frame.
        let pcm = vec![0.0f32; (RATE as usize) / 2 * CHANNELS + 17];
        let frames = measure(&pcm, 0.05);
        assert_eq!(frames.bands.len(), 10);
        assert_eq!(frames.rms.len(), 10);
        assert_eq!(frames.interval, 0.05);
        assert!((frames.start - 0.025).abs() < 1e-12);
        assert!(frames.rms.iter().all(|r| *r == 0), "silence is not 0");
        assert!(frames.bands.iter().flatten().all(|b| *b == 0));
    }

    #[test]
    fn measure_finds_a_tone_in_its_own_band_and_not_the_others() {
        let rate = f64::from(RATE);
        let pcm: Vec<f32> = (0..RATE as usize * CHANNELS)
            .map(|i| {
                let t = f64::from((i / CHANNELS) as u32) / rate;
                (2.0 * std::f64::consts::PI * 929.0 * t).sin() as f32
            })
            .collect();
        let frames = measure(&pcm, 0.02);
        let last = frames
            .bands
            .last()
            .expect("a second of audio is many frames");
        // 929 Hz is band 8's centre.
        let peak = last
            .iter()
            .enumerate()
            .max_by_key(|(_, v)| **v)
            .map(|(i, _)| i);
        assert_eq!(peak, Some(8), "levels: {last:?}");
        assert!(
            dequantize(last[8]) > 0.9,
            "a full-scale tone should be near the top"
        );
        // A distant band still reads about half, because the 64 dB floor is deep and
        // a second-order skirt only reaches -33 dB this far out. That is the tap's
        // behaviour too, and it is why the beat tracker works on differences.
        assert!(
            dequantize(last[0]) < dequantize(last[8]) - 0.4,
            "50 Hz is too close to the tone's own band"
        );
    }

    #[test]
    fn short_or_impossible_requests_return_nothing() {
        assert!(sample_from("", 0.0, 30.0, 0.02).is_none());
        assert!(sample_from("/dev/null", 0.0, 0.0, 0.02).is_none());
        assert!(sample_from("/dev/null", 0.0, -5.0, 0.02).is_none());
        assert!(sample_from("/dev/null", 0.0, f64::NAN, 0.02).is_none());
        assert!(sample_from("/dev/null", 0.0, 30.0, f64::INFINITY).is_none());
        // No such path, and if ffmpeg is missing entirely the answer is the same.
        assert!(sample_from("/nonexistent/nothing.mp3", 0.0, 5.0, 0.02).is_none());
    }

    #[test]
    fn arguments_ask_for_what_the_measurement_needs() {
        let a = args("/tmp/song.mp3", 0.0, 30.0);
        let joined = a.join(" ");
        assert!(joined.contains("-i /tmp/song.mp3"), "{joined}");
        assert!(joined.contains("-f f32le"), "{joined}");
        assert!(joined.contains("-ac 2"), "{joined}");
        assert!(joined.contains("-ar 48000"), "{joined}");
        assert!(
            joined.contains("-nostdin"),
            "stdin would be stolen from the TUI"
        );
        assert_eq!(joined.matches("-t 30.000").count(), 2, "input and output");
        assert_eq!(
            a.last().map(String::as_str),
            Some("-"),
            "must write to stdout"
        );
        // A target is never concatenated into a longer string, so a path containing a
        // space or a leading dash stays one argument.
        let odd = args("-weird name.mp3", 0.0, 1.0);
        assert!(odd.contains(&"-weird name.mp3".to_string()));
    }

    #[test]
    fn cost_grows_with_the_request_and_is_never_absurd() {
        assert!(estimated_cost(0.0) >= SPAWN_COST);
        assert!(estimated_cost(30.0) > estimated_cost(10.0));
        assert!(estimated_cost(30.0) < Duration::from_secs(2));
        assert!(estimated_cost(MAX_SECONDS * 10.0) < Duration::from_secs(30));
        // A request that is not a number costs nothing, because `sample` refuses it.
        assert_eq!(estimated_cost(f64::NAN), SPAWN_COST);
        assert_eq!(estimated_cost(f64::INFINITY), SPAWN_COST);
        assert_eq!(estimated_cost(-1.0), SPAWN_COST);
    }
}
