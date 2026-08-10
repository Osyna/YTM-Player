//! On-disk state: what was playing, what has been played, and where the library index
//! lives.
//!
//! Split from [`crate::settings`] on purpose. Settings are a handful of user choices that
//! a person edits and might reasonably keep in a dotfile repo; this is machine-written
//! churn - a resume point rewritten every few seconds, a history that only grows. They
//! belong in different places for the same reason `~/.config` and `~/.local/state` are
//! different places, and XDG says so.
//!
//! Every read here treats a missing, truncated or malformed file as "no state". Losing a
//! resume point costs the user one keypress; refusing to start because a JSON line is
//! corrupt costs them the session.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many plays the history keeps. Long enough to be a memory, short enough that the
/// file stays a few tens of kilobytes and can be read in one go at startup.
const HISTORY_LIMIT: usize = 2000;
/// How much of a track has to play before it counts as played, in seconds. Skipping
/// through a queue should not fill the history with tracks nobody heard.
const PLAY_THRESHOLD: f64 = 20.0;

/// `$XDG_STATE_HOME/ytmplayer`, else `~/.local/state/ytmplayer`.
pub fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    Some(base.join("ytmplayer"))
}

fn path_in_state(name: &str) -> Option<PathBuf> {
    state_dir().map(|dir| dir.join(name))
}

/// Where the local library index is cached. Public so the library module does not have
/// to duplicate the directory rules.
pub fn library_index_path() -> Option<PathBuf> {
    path_in_state("library.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write `contents` to `path` so a reader never sees half of it.
///
/// Same trick the settings file uses: write a sibling temp file and rename over the
/// target, because `rename(2)` within a directory is atomic and a plain write is not.
/// The resume point is rewritten every few seconds; a crash mid-write must not turn it
/// into an empty file.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, contents)?;
    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resume
// ---------------------------------------------------------------------------

/// Where the last session was when it ended, so the next one can offer to carry on.
#[derive(Clone, Debug, PartialEq)]
pub struct Resume {
    /// What to reopen: the URL, path or search the session was launched with.
    pub target: String,
    /// Human-readable name of the track that was playing, for the offer.
    pub title: String,
    /// Which entry of the queue, so a 60-track playlist comes back where it was.
    pub entry: usize,
    /// Seconds into that track.
    pub position: f64,
    /// The deck-start lag learned by the end of the session, so the correction loop does
    /// not begin every fresh session at zero. `None` for a session that never crossfaded,
    /// or a resume point written before this existed.
    pub release_lag: Option<f64>,
    /// When this was written, so a stale point can be aged out by the caller.
    pub at: u64,
}

impl Resume {
    pub fn load() -> Option<Resume> {
        let path = path_in_state("resume.json")?;
        let text = std::fs::read_to_string(path).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let target = value.get("target")?.as_str()?.to_string();
        if target.is_empty() {
            return None;
        }
        Some(Resume {
            target,
            title: value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            entry: value
                .get("entry")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize,
            position: value
                .get("position")
                .and_then(Value::as_f64)
                .unwrap_or_default(),
            release_lag: value.get("release_lag").and_then(Value::as_f64),
            at: value.get("at").and_then(Value::as_u64).unwrap_or_default(),
        })
    }

    /// Best-effort: a session that cannot write its resume point still plays.
    pub fn save(&self) {
        let Some(path) = path_in_state("resume.json") else {
            return;
        };
        let value = json!({
            "target": self.target,
            "title": self.title,
            "entry": self.entry,
            "position": self.position,
            "release_lag": self.release_lag,
            // Stamped here rather than by the caller: "when this was written" is this
            // function's business, and a caller that forgets makes every point look new.
            "at": now_secs(),
        });
        let _ = write_atomic(&path, &value.to_string());
    }

    /// Drop the resume point - the session ended by running out, not by being left.
    pub fn clear() {
        if let Some(path) = path_in_state("resume.json") {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// One track that actually played.
#[derive(Clone, Debug, PartialEq)]
pub struct Play {
    pub title: String,
    /// The page URL or file path, so a history entry is playable again.
    pub url: String,
    /// `YouTube`, `SoundCloud`, `Spotify`, `Local` - as the status bar spells it.
    pub source: String,
    pub at: u64,
}

/// Append-only play history.
///
/// Append-only because the write happens while music is playing: one `open(O_APPEND)`
/// and one line costs nothing and cannot corrupt what is already there, where
/// rewriting a whole file every few minutes eventually will. Trimming to
/// [`HISTORY_LIMIT`] happens on load, which is the one moment the whole file is in
/// memory anyway.
pub struct History {
    plays: Vec<Play>,
}

impl History {
    pub fn load() -> History {
        let plays = path_in_state("history.jsonl")
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|line| {
                        let value: Value = serde_json::from_str(line).ok()?;
                        Some(Play {
                            title: value.get("title")?.as_str()?.to_string(),
                            url: value
                                .get("url")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            source: value
                                .get("source")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            at: value.get("at").and_then(Value::as_u64).unwrap_or_default(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut history = History { plays };
        history.trim();
        history
    }

    fn trim(&mut self) {
        if self.plays.len() > HISTORY_LIMIT {
            let excess = self.plays.len() - HISTORY_LIMIT;
            self.plays.drain(..excess);
            // The file is now longer than what we hold, so rewrite it once.
            if let Some(path) = path_in_state("history.jsonl") {
                let text: String = self.plays.iter().map(Self::line).collect();
                let _ = write_atomic(&path, &text);
            }
        }
    }

    fn line(play: &Play) -> String {
        format!(
            "{}\n",
            json!({
                "title": play.title,
                "url": play.url,
                "source": play.source,
                "at": play.at,
            })
        )
    }

    /// Newest first - which is the order anything showing a history wants.
    pub fn recent(&self) -> impl Iterator<Item = &Play> {
        self.plays.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.plays.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plays.is_empty()
    }

    /// How many times each URL has been played, for ranking a library search by taste.
    pub fn play_counts(&self) -> std::collections::HashMap<&str, usize> {
        let mut counts = std::collections::HashMap::new();
        for play in &self.plays {
            if !play.url.is_empty() {
                *counts.entry(play.url.as_str()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Record a play, in memory and on disk. Repeats of the track already at the end are
    /// ignored, so a track that loops does not write a line a second.
    pub fn record(&mut self, title: &str, url: &str, source: &str) {
        if title.trim().is_empty() {
            return;
        }
        if self
            .plays
            .last()
            .is_some_and(|last| last.title == title && last.url == url)
        {
            return;
        }
        let play = Play {
            title: title.to_string(),
            url: url.to_string(),
            source: source.to_string(),
            at: now_secs(),
        };
        if let Some(path) = path_in_state("history.jsonl")
            && let Some(dir) = path.parent()
        {
            let _ = std::fs::create_dir_all(dir);
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = file.write_all(Self::line(&play).as_bytes());
            }
        }
        self.plays.push(play);
    }
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// How many distinct tracks the analysis cache remembers. Same order as [`HISTORY_LIMIT`]
/// for the same reason: long enough to matter, short enough to stay a startup-time read.
const ANALYSIS_LIMIT: usize = 2000;

/// What a probe found for one track, kept so it is ready to mix again without one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackAnalysis {
    pub bpm: f64,
    pub downbeat: Option<f64>,
    /// When this was measured. Not currently aged out by the caller, but written so a
    /// future one can be without a format change.
    pub at: u64,
}

/// Tempo and downbeat already measured, one entry per track, keyed by the URL or path a
/// later session will ask about again.
///
/// Append-only on disk for the reason [`History`]'s is: the write happens while music is
/// playing, and one line costs nothing where rewriting a whole file would eventually cost
/// something. Unlike history this is a cache, not a log - a track measured twice keeps
/// only the newer reading, folded together on [`AnalysisCache::load`], which is also the
/// one place [`ANALYSIS_LIMIT`] is enforced and the file rewritten to match.
pub struct AnalysisCache {
    by_url: HashMap<String, TrackAnalysis>,
}

impl AnalysisCache {
    pub fn load() -> AnalysisCache {
        let mut by_url: HashMap<String, TrackAnalysis> = HashMap::new();
        if let Some(text) =
            path_in_state("analysis.jsonl").and_then(|path| std::fs::read_to_string(path).ok())
        {
            for line in text.lines() {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let Some(url) = value
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                else {
                    continue;
                };
                let Some(bpm) = value.get("bpm").and_then(Value::as_f64) else {
                    continue;
                };
                // Lines are appended in order, so a later one for the same URL is always
                // the newer reading and simply replaces the older entry.
                by_url.insert(
                    url.to_string(),
                    TrackAnalysis {
                        bpm,
                        downbeat: value.get("downbeat").and_then(Value::as_f64),
                        at: value.get("at").and_then(Value::as_u64).unwrap_or_default(),
                    },
                );
            }
        }
        let mut cache = AnalysisCache { by_url };
        cache.compact();
        cache
    }

    /// Drop the oldest entries once there are more than [`ANALYSIS_LIMIT`], and rewrite
    /// the file to match - the one moment the whole cache is in memory anyway.
    fn compact(&mut self) {
        if self.by_url.len() <= ANALYSIS_LIMIT {
            return;
        }
        let mut by_age: Vec<(String, u64)> = self
            .by_url
            .iter()
            .map(|(url, a)| (url.clone(), a.at))
            .collect();
        by_age.sort_by_key(|(_, at)| *at);
        let excess = by_age.len() - ANALYSIS_LIMIT;
        for (url, _) in &by_age[..excess] {
            self.by_url.remove(url);
        }
        let Some(path) = path_in_state("analysis.jsonl") else {
            return;
        };
        let text: String = self
            .by_url
            .iter()
            .map(|(url, a)| Self::line(url, a))
            .collect();
        let _ = write_atomic(&path, &text);
    }

    fn line(url: &str, a: &TrackAnalysis) -> String {
        format!(
            "{}\n",
            json!({"url": url, "bpm": a.bpm, "downbeat": a.downbeat, "at": a.at})
        )
    }

    /// What was last measured for `url`, if anything - ready to mix without a probe.
    pub fn get(&self, url: &str) -> Option<TrackAnalysis> {
        self.by_url.get(url).copied()
    }

    /// Record a measurement, in memory and on disk.
    pub fn record(&mut self, url: &str, bpm: f64, downbeat: Option<f64>) {
        if url.is_empty() || !bpm.is_finite() || bpm <= 0.0 {
            return;
        }
        let analysis = TrackAnalysis {
            bpm,
            downbeat,
            at: now_secs(),
        };
        if let Some(path) = path_in_state("analysis.jsonl")
            && let Some(dir) = path.parent()
        {
            let _ = std::fs::create_dir_all(dir);
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = file.write_all(Self::line(url, &analysis).as_bytes());
            }
        }
        self.by_url.insert(url.to_string(), analysis);
    }
}

// ---------------------------------------------------------------------------
// Downloads
// ---------------------------------------------------------------------------

/// How many saved tracks the index remembers. Same order as [`ANALYSIS_LIMIT`] and for
/// the same reason.
const DOWNLOADS_LIMIT: usize = 2000;

/// One saved track: where it landed, and when, so [`DownloadIndex::compact`] can age
/// entries out the same way [`AnalysisCache`] does.
#[derive(Clone, Debug, PartialEq)]
struct Saved {
    path: PathBuf,
    at: u64,
}

/// Where a track that has been saved to disk actually is, one entry per URL, keyed the
/// same way [`AnalysisCache`] is and for the same reason: a track downloaded once should
/// never cost a second network trip, whether that trip is mpv opening it or a probe
/// measuring it.
///
/// Append-only on disk, a cache rather than a log, [`DOWNLOADS_LIMIT`] enforced and the
/// file rewritten to match on [`DownloadIndex::load`] - identical to [`AnalysisCache`] in
/// every way but what it stores, because it is answering the same question ("has this
/// already been paid for") about a different fact.
pub struct DownloadIndex {
    by_url: HashMap<String, Saved>,
}

impl DownloadIndex {
    pub fn load() -> DownloadIndex {
        let mut by_url: HashMap<String, Saved> = HashMap::new();
        if let Some(text) =
            path_in_state("downloads.jsonl").and_then(|path| std::fs::read_to_string(path).ok())
        {
            for line in text.lines() {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let Some(url) = value
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                else {
                    continue;
                };
                let Some(path) = value
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                else {
                    continue;
                };
                // Lines are appended in order, so a later one for the same URL is always
                // the current file and simply replaces the older entry.
                by_url.insert(
                    url.to_string(),
                    Saved {
                        path: PathBuf::from(path),
                        at: value.get("at").and_then(Value::as_u64).unwrap_or_default(),
                    },
                );
            }
        }
        let mut index = DownloadIndex { by_url };
        index.compact();
        index
    }

    /// Drop the oldest entries once there are more than [`DOWNLOADS_LIMIT`], and rewrite
    /// the file to match - the one moment the whole index is in memory anyway.
    fn compact(&mut self) {
        if self.by_url.len() <= DOWNLOADS_LIMIT {
            return;
        }
        let mut by_age: Vec<(String, u64)> = self
            .by_url
            .iter()
            .map(|(url, saved)| (url.clone(), saved.at))
            .collect();
        by_age.sort_by_key(|(_, at)| *at);
        let excess = by_age.len() - DOWNLOADS_LIMIT;
        for (url, _) in &by_age[..excess] {
            self.by_url.remove(url);
        }
        let Some(path) = path_in_state("downloads.jsonl") else {
            return;
        };
        let text: String = self
            .by_url
            .iter()
            .map(|(url, saved)| Self::line(url, saved))
            .collect();
        let _ = write_atomic(&path, &text);
    }

    fn line(url: &str, saved: &Saved) -> String {
        format!(
            "{}\n",
            json!({"url": url, "path": saved.path.to_string_lossy(), "at": saved.at})
        )
    }

    /// Where `url` was saved to, if it still exists there. A download later moved or
    /// deleted by hand is worth checking for, not worth handing to ffmpeg as though it
    /// were still where the index last saw it.
    pub fn get(&self, url: &str) -> Option<&Path> {
        self.by_url
            .get(url)
            .map(|saved| saved.path.as_path())
            .filter(|path| path.is_file())
    }

    /// Record a save, in memory and on disk.
    pub fn record(&mut self, url: &str, path: &str) {
        if url.is_empty() || path.is_empty() {
            return;
        }
        let saved = Saved {
            path: PathBuf::from(path),
            at: now_secs(),
        };
        if let Some(state_path) = path_in_state("downloads.jsonl")
            && let Some(dir) = state_path.parent()
        {
            let _ = std::fs::create_dir_all(dir);
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&state_path)
            {
                let _ = file.write_all(Self::line(url, &saved).as_bytes());
            }
        }
        self.by_url.insert(url.to_string(), saved);
    }
}

/// Whether a track has played long enough to be worth remembering.
pub fn worth_recording(position: Option<f64>, duration: Option<f64>) -> bool {
    let Some(position) = position else {
        return false;
    };
    // Short tracks count once they are half done; anything else at the fixed threshold,
    // so skipping through a queue leaves no trace.
    match duration {
        Some(d) if d > 0.0 && d < PLAY_THRESHOLD * 2.0 => position >= d / 2.0,
        _ => position >= PLAY_THRESHOLD,
    }
}

/// A timestamp as `2 minutes ago` / `3 days ago`, for a history listing.
pub fn ago(then: u64) -> String {
    let now = now_secs();
    let secs = now.saturating_sub(then);
    let (n, unit) = match secs {
        0..=59 => return "just now".to_string(),
        60..=3599 => (secs / 60, "minute"),
        3600..=86_399 => (secs / 3600, "hour"),
        86_400..=2_591_999 => (secs / 86_400, "day"),
        _ => (secs / 2_592_000, "month"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skipped_track_leaves_no_trace() {
        // Three seconds in: the user is scrubbing through a queue, not listening.
        assert!(!worth_recording(Some(3.0), Some(240.0)));
        assert!(worth_recording(Some(20.0), Some(240.0)));
        // A jingle counts at its own halfway point rather than never.
        assert!(!worth_recording(Some(4.0), Some(12.0)));
        assert!(worth_recording(Some(6.0), Some(12.0)));
        // Nothing playing is nothing to record.
        assert!(!worth_recording(None, Some(240.0)));
        // A stream with no duration falls back to the fixed threshold.
        assert!(worth_recording(Some(30.0), None));
    }

    #[test]
    fn repeats_of_the_same_track_are_one_entry() {
        let mut history = History { plays: Vec::new() };
        // No state dir in the test environment is fine: the in-memory list is the
        // contract, and the file write is best-effort by design.
        history.record("Song", "u1", "YouTube");
        history.record("Song", "u1", "YouTube");
        history.record("Other", "u2", "YouTube");
        history.record("Song", "u1", "YouTube");
        assert_eq!(history.len(), 3, "a looping track wrote a line a second");
        let titles: Vec<&str> = history.recent().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, vec!["Song", "Other", "Song"], "newest first");
    }

    #[test]
    fn play_counts_rank_by_url_not_title() {
        let mut history = History { plays: Vec::new() };
        history.record("A", "u1", "Local");
        history.record("B", "u2", "Local");
        history.record("A again", "u1", "Local");
        let counts = history.play_counts();
        assert_eq!(counts.get("u1"), Some(&2));
        assert_eq!(counts.get("u2"), Some(&1));
    }

    #[test]
    fn ages_read_like_english() {
        let now = now_secs();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 60), "1 minute ago");
        assert_eq!(ago(now - 7200), "2 hours ago");
        assert_eq!(ago(now - 86_400), "1 day ago");
        // A timestamp from the future must not underflow into "584 million years ago".
        assert_eq!(ago(now + 5000), "just now");
    }

    #[test]
    fn a_measured_track_is_ready_to_mix_next_time() {
        let mut cache = AnalysisCache {
            by_url: HashMap::new(),
        };
        assert_eq!(cache.get("u1"), None, "nothing measured yet");
        cache.record("u1", 128.0, Some(4.5));
        let known = cache.get("u1").expect("just recorded");
        assert_eq!(known.bpm, 128.0);
        assert_eq!(known.downbeat, Some(4.5));
    }

    #[test]
    fn re_measuring_a_track_keeps_only_the_newer_reading() {
        // A cache, not a log: unlike history, a second reading for the same track
        // replaces the first rather than sitting beside it.
        let mut cache = AnalysisCache {
            by_url: HashMap::new(),
        };
        cache.record("u1", 128.0, None);
        cache.record("u1", 127.5, Some(2.1));
        assert_eq!(cache.by_url.len(), 1);
        let known = cache.get("u1").expect("recorded");
        assert_eq!(known.bpm, 127.5);
        assert_eq!(known.downbeat, Some(2.1));
    }

    #[test]
    fn nonsense_readings_are_not_recorded() {
        let mut cache = AnalysisCache {
            by_url: HashMap::new(),
        };
        cache.record("", 128.0, None);
        cache.record("u1", 0.0, None);
        cache.record("u1", -5.0, None);
        cache.record("u1", f64::NAN, None);
        assert!(cache.by_url.is_empty());
    }

    #[test]
    fn compaction_drops_the_oldest_readings_first() {
        let over = ANALYSIS_LIMIT + 3;
        let by_url = (0..over)
            .map(|i| {
                (
                    format!("u{i}"),
                    TrackAnalysis {
                        bpm: 120.0,
                        downbeat: None,
                        at: i as u64,
                    },
                )
            })
            .collect();
        let mut cache = AnalysisCache { by_url };
        cache.compact();
        assert_eq!(cache.by_url.len(), ANALYSIS_LIMIT);
        assert!(
            !cache.by_url.contains_key("u0"),
            "the oldest should be gone"
        );
        assert!(
            !cache.by_url.contains_key("u2"),
            "the third-oldest should be gone"
        );
        assert!(
            cache.by_url.contains_key(&format!("u{}", over - 1)),
            "the newest must survive"
        );
    }

    /// A file under `std::env::temp_dir()`, unique to this test process, cleaned up when
    /// the guard drops - so `DownloadIndex::get`'s "does it still exist" check has
    /// something real to check against without touching any file another test might.
    struct TempTrack(PathBuf);

    impl TempTrack {
        fn new(name: &str) -> TempTrack {
            let path = std::env::temp_dir()
                .join(format!("ytmplayer_test_{name}_{}.mp3", std::process::id()));
            std::fs::write(&path, b"not really audio").expect("write temp track");
            TempTrack(path)
        }
    }

    impl Drop for TempTrack {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_downloaded_track_is_found_by_its_url() {
        let track = TempTrack::new("found");
        let mut index = DownloadIndex {
            by_url: HashMap::new(),
        };
        assert_eq!(index.get("u1"), None, "nothing saved yet");
        index.record("u1", &track.0.to_string_lossy());
        assert_eq!(index.get("u1"), Some(track.0.as_path()));
        assert_eq!(index.get("u2"), None, "a different URL was never saved");
    }

    #[test]
    fn a_download_moved_or_deleted_by_hand_is_not_handed_out() {
        let track = TempTrack::new("vanishing");
        let mut index = DownloadIndex {
            by_url: HashMap::new(),
        };
        index.record("u1", &track.0.to_string_lossy());
        assert!(index.get("u1").is_some());
        std::fs::remove_file(&track.0).expect("remove temp track");
        assert_eq!(
            index.get("u1"),
            None,
            "a path the index remembers but the disk does not must not be handed to ffmpeg"
        );
    }

    #[test]
    fn nonsense_saves_are_not_recorded() {
        let track = TempTrack::new("nonsense");
        let mut index = DownloadIndex {
            by_url: HashMap::new(),
        };
        index.record("", &track.0.to_string_lossy());
        index.record("u1", "");
        assert!(index.by_url.is_empty());
    }

    #[test]
    fn re_saving_the_same_url_keeps_only_the_newer_path() {
        let first = TempTrack::new("first");
        let second = TempTrack::new("second");
        let mut index = DownloadIndex {
            by_url: HashMap::new(),
        };
        index.record("u1", &first.0.to_string_lossy());
        index.record("u1", &second.0.to_string_lossy());
        assert_eq!(index.by_url.len(), 1);
        assert_eq!(index.get("u1"), Some(second.0.as_path()));
    }

    #[test]
    fn download_compaction_drops_the_oldest_first() {
        let over = DOWNLOADS_LIMIT + 3;
        let by_url = (0..over)
            .map(|i| {
                (
                    format!("u{i}"),
                    Saved {
                        path: PathBuf::from(format!("/tmp/track{i}.mp3")),
                        at: i as u64,
                    },
                )
            })
            .collect();
        let mut index = DownloadIndex { by_url };
        index.compact();
        assert_eq!(index.by_url.len(), DOWNLOADS_LIMIT);
        assert!(
            !index.by_url.contains_key("u0"),
            "the oldest should be gone"
        );
        assert!(
            index.by_url.contains_key(&format!("u{}", over - 1)),
            "the newest must survive"
        );
    }
}
