//! Live audio analysis for the visualizers, tapped from mpv's own filter chain.
//!
//! mpv is the only thing decoding audio, so it is the only honest source of what's
//! actually playing. Its `af` chain gets a transparent tap: the audible path passes
//! through untouched, while a re-chunked side branch measures full-band RMS/peak per
//! channel plus the RMS of [`BAND_COUNT`] octave-spaced bandpass branches. Every
//! measurement is printed by `ametadata` into a FIFO we pre-open non-blocking, and a
//! reader thread turns those lines into [`VizSnapshot`]s ~45 times a second.
//!
//! No PCM leaves mpv, no capture device or loopback is involved, and nothing new is
//! linked: `astats`/`bandpass`/`ametadata` are core libavfilter, present in every mpv.
//! A dead tap (filters missing, FIFO trouble) degrades to `live: false` - the UI shows
//! idle visualizers instead of failing playback.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Spectrum resolution. Sixteen octave-ish bands cover 50 Hz to 12 kHz.
pub const BAND_COUNT: usize = 16;

/// Band centres, geometrically spaced: `50 * (12000/50)^(i/15)`.
const FREQS: [u32; BAND_COUNT] = [
    50, 72, 104, 150, 216, 311, 448, 645, 929, 1338, 1927, 2775, 3996, 5754, 8286, 11932,
];

/// Silence floor for full-band levels: -60 dB maps to 0.0, 0 dB to 1.0.
const FULL_FLOOR: f32 = 60.0;
/// Bandpassed branches carry less energy per band; a slightly deeper floor keeps the
/// spectrum lively at ordinary programme levels (pink noise sits near -24 dB per band).
pub const BAND_FLOOR: f32 = 64.0;

/// How much wave history the tap keeps. At ~45 samples/s this is over 40 seconds -
/// more than any terminal width the scope will ever scroll across.
const WAVE_CAP: usize = 2048;

/// A snapshot is `live` while data arrived within this window; a paused or idle mpv
/// stops printing and the visualizers show a becalmed state instead of stale motion.
const LIVE_WINDOW: Duration = Duration::from_millis(500);

/// O_NONBLOCK on Linux (both x86_64 and aarch64). Hardcoded to keep libc out of the
/// dependency tree; this player only ships on Linux.
const O_NONBLOCK: i32 = 0o4000;

/// What the renderers read, sampled once per frame.
#[derive(Clone)]
pub struct VizSnapshot {
    /// Per-band level, 0.0..=1.0, low frequencies first.
    pub bands: [f32; BAND_COUNT],
    /// Full-band RMS per channel (L, R), 0.0..=1.0.
    pub rms: [f32; 2],
    /// Full-band peak per channel (L, R), 0.0..=1.0.
    pub peak: [f32; 2],
    /// RMS history per channel, oldest first, newest last. One sample per tap window.
    pub wave: Vec<(f32, f32)>,
    /// Data arrived recently; false while paused, idle, or the tap is broken.
    pub live: bool,
}

impl Default for VizSnapshot {
    fn default() -> Self {
        VizSnapshot {
            bands: [0.0; BAND_COUNT],
            rms: [0.0; 2],
            peak: [0.0; 2],
            wave: Vec::new(),
            live: false,
        }
    }
}

struct VizData {
    bands: [f32; BAND_COUNT],
    rms: [f32; 2],
    peak: [f32; 2],
    wave: VecDeque<(f32, f32)>,
    last: Instant,
}

/// The running tap: FIFO plumbing plus the reader thread's shared state.
pub struct Tap {
    dir: PathBuf,
    data: Arc<Mutex<VizData>>,
    stop: Arc<AtomicBool>,
}

impl Tap {
    /// Create the FIFOs and start the reader. The returned tap is inert until
    /// [`Tap::graph`] is installed as mpv's `af`.
    pub fn start() -> std::io::Result<Tap> {
        sweep_stale_taps();
        let dir = std::env::temp_dir().join(format!("ytmviz_{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;

        let paths: Vec<PathBuf> = (0..=BAND_COUNT)
            .map(|i| dir.join(format!("f{i}")))
            .collect();
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        // mkfifo(3) has no std wrapper; the coreutils binary is universal on Linux.
        let status = Command::new("mkfifo")
            .args(&paths)
            .status()
            .map_err(|e| std::io::Error::other(format!("mkfifo: {e}")))?;
        if !status.success() {
            return Err(std::io::Error::other("mkfifo failed"));
        }

        // Open the read ends before mpv ever opens the write ends, non-blocking, so
        // `ametadata`'s open() never stalls mpv's audio thread waiting for a reader.
        let mut files = Vec::with_capacity(paths.len());
        for p in &paths {
            files.push(
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(O_NONBLOCK)
                    .open(p)?,
            );
        }

        let data = Arc::new(Mutex::new(VizData {
            bands: [0.0; BAND_COUNT],
            rms: [0.0; 2],
            peak: [0.0; 2],
            wave: VecDeque::with_capacity(WAVE_CAP),
            last: Instant::now() - LIVE_WINDOW * 10,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let data = data.clone();
            let stop = stop.clone();
            std::thread::spawn(move || reader_loop(files, &data, &stop));
        }
        Ok(Tap { dir, data, stop })
    }

    /// The `af` value that feeds this tap: `lavfi=[...]`, ready for
    /// `set_property "af"`. Passing it through IPC dodges every layer of shell and
    /// option-parser escaping the graph's own separators would otherwise fight.
    pub fn graph(&self) -> String {
        let d = self.dir.display();
        let mut g = String::with_capacity(2048);
        // The audible path: [m] straight into amix below, untouched.
        g.push_str("asplit=2[m][t];[t]asetnsamples=n=1024,asplit=");
        g.push_str(&(BAND_COUNT + 1).to_string());
        for i in 0..=BAND_COUNT {
            g.push_str(&format!("[t{i}]"));
        }
        g.push(';');
        // Full-band: per-channel RMS + peak, every key printed.
        g.push_str(&format!(
            "[t0]astats=metadata=1:reset=1:measure_overall=none:\
             measure_perchannel=RMS_level+Peak_level,\
             ametadata=mode=print:file={d}/f0:direct=1[c0];"
        ));
        for (i, f) in FREQS.iter().enumerate() {
            let n = i + 1;
            g.push_str(&format!(
                "[t{n}]bandpass=f={f}:width_type=o:w=0.6,\
                 astats=metadata=1:reset=1:measure_perchannel=none:measure_overall=RMS_level,\
                 ametadata=mode=print:key=lavfi.astats.Overall.RMS_level:file={d}/f{n}:direct=1[c{n}];"
            ));
        }
        // Everything reconverges so the graph has one output; zero weights mute the
        // tap branches. normalize=0 keeps the audible branch at unity gain.
        g.push_str("[m]");
        for i in 0..=BAND_COUNT {
            g.push_str(&format!("[c{i}]"));
        }
        g.push_str(&format!(
            "amix=inputs={}:weights='1{}':normalize=0",
            BAND_COUNT + 2,
            " 0".repeat(BAND_COUNT + 1)
        ));
        format!("lavfi=[{g}]")
    }

    /// Latest data, shaped for rendering. `wave` returns at most `wave_len` newest
    /// samples, oldest first.
    pub fn snapshot(&self, wave_len: usize) -> VizSnapshot {
        let data = self.data.lock();
        let skip = data.wave.len().saturating_sub(wave_len);
        VizSnapshot {
            bands: data.bands,
            rms: data.rms,
            peak: data.peak,
            wave: data.wave.iter().skip(skip).copied().collect(),
            live: data.last.elapsed() < LIVE_WINDOW,
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Delete `ytmviz_<pid>` directories left by players that are gone.
///
/// [`Tap`]'s `Drop` cleans up a normal exit, but SIGKILL, a power cut or an OOM kill
/// leave the directory and its 17 FIFOs behind forever. `/proc/<pid>` is the liveness
/// test - this player is Linux-only anyway, and it keeps libc out of the tree.
fn sweep_stale_taps() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("ytmviz_")) else {
            continue;
        };
        // Anything not a plain pid is not ours to delete.
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if !Path::new("/proc").join(pid).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Normalise a dB text value against a floor: `-floor` dB -> 0.0, 0 dB -> 1.0.
fn norm_db(text: &str, floor: f32) -> f32 {
    let db: f32 = text.trim().parse().unwrap_or(f32::NEG_INFINITY);
    if !db.is_finite() {
        return 0.0;
    }
    ((db + floor) / floor).clamp(0.0, 1.0)
}

/// Poll every FIFO, parse `ametadata` lines, publish. FIFO reads return 0 while no
/// writer exists and `WouldBlock` while a writer is quiet, so this sweeps and sleeps.
fn reader_loop(mut files: Vec<File>, data: &Mutex<VizData>, stop: &AtomicBool) {
    let mut tails: Vec<String> = vec![String::new(); files.len()];
    let mut chunk = [0u8; 65536];
    // Full-band keys accumulate here until the next `frame:` marker commits them.
    let mut pending: [Option<f32>; 4] = [None; 4]; // rms_l, rms_r, peak_l, peak_r

    while !stop.load(Ordering::Relaxed) {
        let mut touched = false;
        for (i, file) in files.iter_mut().enumerate() {
            loop {
                match file.read(&mut chunk) {
                    Ok(0) => break, // no writer right now
                    Ok(n) => {
                        tails[i].push_str(&String::from_utf8_lossy(&chunk[..n]));
                        touched = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            if !tails[i].contains('\n') {
                continue;
            }
            let text = std::mem::take(&mut tails[i]);
            let (complete, rest) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
            tails[i] = rest.to_string();

            if i == 0 {
                parse_full(complete, &mut pending, data);
            } else {
                // Band FIFO: the newest RMS line wins the frame.
                if let Some(v) = complete
                    .lines()
                    .rev()
                    .find_map(|l| l.strip_prefix("lavfi.astats.Overall.RMS_level="))
                {
                    data.lock().bands[i - 1] = norm_db(v, BAND_FLOOR);
                }
            }
        }
        if touched {
            data.lock().last = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Full-band FIFO: `frame:` markers delimit windows; between them arrive per-channel
/// `lavfi.astats.<ch>.RMS_level=` / `.Peak_level=` keys. Mono input simply never
/// mentions channel 2 and the left values serve both sides.
fn parse_full(text: &str, pending: &mut [Option<f32>; 4], data: &Mutex<VizData>) {
    for line in text.lines() {
        if line.starts_with("frame:") {
            commit_full(pending, data);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let slot = match key {
            "lavfi.astats.1.RMS_level" => 0,
            "lavfi.astats.2.RMS_level" => 1,
            "lavfi.astats.1.Peak_level" => 2,
            "lavfi.astats.2.Peak_level" => 3,
            _ => continue,
        };
        pending[slot] = Some(norm_db(value, FULL_FLOOR));
    }
}

fn commit_full(pending: &mut [Option<f32>; 4], data: &Mutex<VizData>) {
    let Some(rms_l) = pending[0] else {
        return; // marker before any keys (start of stream)
    };
    let rms_r = pending[1].unwrap_or(rms_l);
    let peak_l = pending[2].unwrap_or(rms_l);
    let peak_r = pending[3].unwrap_or(peak_l);
    let mut d = data.lock();
    d.rms = [rms_l, rms_r];
    d.peak = [peak_l, peak_r];
    if d.wave.len() == WAVE_CAP {
        d.wave.pop_front();
    }
    d.wave.push_back((rms_l, rms_r));
    *pending = [None; 4];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_dead_taps_and_keeps_live_ones() {
        let tmp = std::env::temp_dir();
        // A pid above the kernel's maximum can never be running.
        let dead = tmp.join("ytmviz_4194305");
        let live = tmp.join(format!("ytmviz_{}", std::process::id()));
        let foreign = tmp.join("ytmviz_notapid");
        for d in [&dead, &live, &foreign] {
            std::fs::create_dir_all(d).unwrap();
        }

        sweep_stale_taps();

        assert!(!dead.exists(), "a dead player's FIFOs were left behind");
        assert!(live.exists(), "swept a running player's own directory");
        assert!(foreign.exists(), "deleted something that is not ours");
        let _ = std::fs::remove_dir_all(&live);
        let _ = std::fs::remove_dir_all(&foreign);
    }

    #[test]
    fn graph_is_one_chain_with_all_fifos() {
        let tap = Tap {
            dir: PathBuf::from("/tmp/viz_test"),
            data: Arc::new(Mutex::new(VizData {
                bands: [0.0; BAND_COUNT],
                rms: [0.0; 2],
                peak: [0.0; 2],
                wave: VecDeque::new(),
                last: Instant::now(),
            })),
            stop: Arc::new(AtomicBool::new(true)),
        };
        let g = tap.graph();
        assert!(g.starts_with("lavfi=["), "must be an af value: {g}");
        assert!(g.ends_with(']'));
        // Every FIFO is wired in, and the audible branch keeps unity weight.
        for i in 0..=BAND_COUNT {
            assert!(
                g.contains(&format!("/tmp/viz_test/f{i}")),
                "fifo {i} missing"
            );
        }
        assert_eq!(g.matches("bandpass=").count(), BAND_COUNT);
        assert!(g.contains("weights='1 0"));
        std::mem::forget(tap); // don't remove_dir_all a directory we never made
    }

    #[test]
    fn db_normalisation_clamps_and_maps() {
        assert_eq!(norm_db("0.0", 60.0), 1.0);
        assert_eq!(norm_db("-60.0", 60.0), 0.0);
        assert_eq!(norm_db("-inf", 60.0), 0.0);
        assert_eq!(norm_db("garbage", 60.0), 0.0);
        let mid = norm_db("-30.0", 60.0);
        assert!((mid - 0.5).abs() < 1e-6);
    }

    #[test]
    fn full_band_lines_commit_on_frame_markers() {
        let data = Mutex::new(VizData {
            bands: [0.0; BAND_COUNT],
            rms: [0.0; 2],
            peak: [0.0; 2],
            wave: VecDeque::new(),
            last: Instant::now(),
        });
        let mut pending = [None; 4];
        let text = "frame:0 pts:0 pts_time:0\n\
                    lavfi.astats.1.RMS_level=-30.0\n\
                    lavfi.astats.1.Peak_level=-12.0\n\
                    lavfi.astats.2.RMS_level=-30.0\n\
                    lavfi.astats.2.Peak_level=-6.0\n\
                    frame:1 pts:1024 pts_time:0.023\n";
        parse_full(text, &mut pending, &data);
        let d = data.lock();
        assert_eq!(d.wave.len(), 1);
        assert!((d.rms[0] - 0.5).abs() < 1e-6);
        assert!(d.peak[1] > d.peak[0], "R peak louder than L");
    }

    #[test]
    fn mono_duplicates_left_channel() {
        let data = Mutex::new(VizData {
            bands: [0.0; BAND_COUNT],
            rms: [0.0; 2],
            peak: [0.0; 2],
            wave: VecDeque::new(),
            last: Instant::now(),
        });
        let mut pending = [None; 4];
        parse_full(
            "frame:0\nlavfi.astats.1.RMS_level=-30.0\nlavfi.astats.1.Peak_level=-12.0\nframe:1\n",
            &mut pending,
            &data,
        );
        let d = data.lock();
        assert_eq!(d.rms[0], d.rms[1]);
        assert_eq!(d.peak[0], d.peak[1]);
    }
}
