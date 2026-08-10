//! The on-disk index of the user's local music folders, and the two things the browser view
//! needs from it: a rescan that does not block drawing, and a search that runs on every
//! keystroke.
//!
//! Tags come from `ffprobe` rather than a tag-parsing crate because ffprobe is already a
//! dependency of this project and already understands every container mpv will play - the
//! alternative is one crate per format and a matrix of edge cases nobody will maintain. The
//! price is a process per file, roughly 20 ms each, so a ten-thousand-track folder is minutes
//! of work. That single fact shapes the rest of the module: the scan runs on its own threads,
//! reports progress, can be cancelled, and never re-probes a file whose path and mtime it
//! already has on record.
//!
//! The index is a plain JSON document written by hand from [`serde_json::Value`]. Derived
//! serialisation would mean a `serde` dependency for one struct with six fields, and hand
//! writing it also means an index from a future version, or a half-written one, degrades to
//! "empty library, rescan" instead of an error the user has to understand.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

/// Extensions worth spending a process on. Deliberately narrower than what mpv opens: sweeping
/// in everything would put cover art, lyrics and stray `.m3u` files through ffprobe and then
/// into the browser. Video containers are here because a downloaded music video is still a
/// track the user wants to find by name.
const MEDIA_EXTS: &[&str] = &[
    "aac", "aif", "aiff", "alac", "flac", "m4a", "mkv", "mp3", "mp4", "oga", "ogg", "opus", "wav",
    "webm", "wma",
];

/// How far below a root the walk will go. Music libraries are artist/album/disc deep; anything
/// past this is either a mistake or a directory structure that will take longer to walk than
/// the user is willing to wait, and a hard cap means a pathological tree cannot wedge the scan.
const MAX_DEPTH: usize = 12;

/// How many ffprobe processes run at once. Each one is short, mostly blocked on a read, and
/// costs a fork - so a handful in flight turns the scan from serial process spawning into
/// something the disk paces, while staying far away from a fork bomb on a big library.
const MAX_PROBES: usize = 8;

/// ffmpeg's own `AVPROBE_SCORE_RETRY`: below this its demuxer detection is a guess, not a
/// recognition. A text file called `.flac` scores 1 and would otherwise be indexed as a
/// zero-length track that fails the moment it is played.
const MIN_PROBE_SCORE: u64 = 25;

/// Bumped whenever the meaning of a field changes. An index written by a different version is
/// discarded rather than migrated: the whole thing is a cache, and a rescan rebuilds it.
const INDEX_VERSION: u64 = 2;

/// Tag keys accepted for each field, best first. Containers disagree on spelling - Matroska
/// shouts `ARTIST`, iTunes-style MP4 writes `album_artist`, Vorbis comments use `performer` -
/// and case varies by muxer, so lookups are done on lowercased keys.
const TITLE_KEYS: &[&str] = &["title", "track_name"];
const ARTIST_KEYS: &[&str] = &[
    "artist",
    "album_artist",
    "albumartist",
    "performer",
    "author",
];
const ALBUM_KEYS: &[&str] = &["album"];
/// Where a tempo lives when a file carries one. `TBPM` is the ID3v2 frame every DJ tool
/// writes and reads; ffprobe surfaces it lowercased, and Vorbis/Matroska use the bare
/// word. Reading it is what makes an already-analysed file free to load a second time.
const BPM_KEYS: &[&str] = &["tbpm", "bpm"];

/// One indexed file. `modified` is what makes a rescan cheap, so it is part of the record
/// rather than something re-derived at scan time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub path: PathBuf,
    /// Never empty: an untagged file falls back to a cleaned-up file stem, because a blank row
    /// in the browser is worse than a slightly wrong one.
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Seconds. `0.0` when ffprobe reported nothing usable, which the UI shows as `--:--`
    /// rather than a convincing zero.
    pub duration: f64,
    /// Unix seconds of the file's mtime, the key to reusing an entry across scans.
    pub modified: u64,
    /// The tempo the file carries in its own tags, when it has one.
    ///
    /// Read here rather than measured, and that is the whole point: analysing a track is
    /// an ffmpeg decode of the entire thing, and a file that already says what it is has
    /// been paid for once already - by this player, on a previous run, or by whatever
    /// wrote the tag before that. Seeded into the analysis cache after a scan, so a local
    /// track with a `TBPM` is ready to mix the moment it is loaded.
    pub bpm: Option<f64>,
}

/// Progress of a scan running on a background thread.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScanProgress {
    pub done: usize,
    pub total: usize,
    pub finished: bool,
}

/// The indexed library: tracks in path order, plus the roots they came from.
#[derive(Clone, Debug, Default)]
pub struct Library {
    tracks: Vec<Track>,
    /// The searchable form of each track, in the same order. Search runs on every keystroke
    /// over the whole library, so the strings it matches against are normalised once at
    /// scan/load time rather than once per query per track.
    index: Vec<Searchable>,
    roots: Vec<PathBuf>,
}

/// What a query is actually matched against, normalised (lowercased, diacritics folded).
#[derive(Clone, Debug, Default)]
struct Searchable {
    /// "artist album title stem": the tagged fields first, so the same match in an artist
    /// outranks one in a path, and the file stem last so an untagged file is still findable by
    /// whatever the downloader called it.
    hay: String,
    /// The title on its own, so that a query matching the song rather than its album can be
    /// told apart from one that only matched the record it happens to sit on.
    title: String,
}

impl Library {
    /// Read the index from `path`. A missing file is a first run, a corrupt or foreign one is a
    /// cache that has to be rebuilt anyway; both are an empty library, because there is nothing
    /// the user could usefully do with the error.
    pub fn load(path: &Path) -> Library {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Library::default();
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            return Library::default();
        };
        if root.get("version").and_then(Value::as_u64) != Some(INDEX_VERSION) {
            return Library::default();
        }
        let roots = root
            .get("roots")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let tracks = root
            .get("tracks")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(track_from_json).collect())
            .unwrap_or_default();
        Library::new(tracks, roots)
    }

    /// Write the index, creating the parent directory if the config dir is new.
    ///
    /// Written to a sibling temp file and renamed, so a crash or a full disk mid-write leaves
    /// the previous index intact rather than a truncated one - which `load` would silently
    /// treat as empty, throwing away an hour of scanning.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let document = json!({
            "version": INDEX_VERSION,
            "roots": self.roots.iter().map(PathBuf::as_path).filter_map(path_to_json).collect::<Vec<Value>>(),
            "tracks": self.tracks.iter().filter_map(track_to_json).collect::<Vec<Value>>(),
        });
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, document.to_string())?;
        std::fs::rename(&temp, path)
    }

    /// True before the first scan, and after a scan that found nothing - the two cases where
    /// the browser has to say so rather than draw an empty list.
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// How many tracks are indexed.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Every track, in path order - which is also the order an empty search returns and the
    /// order the browser lists, so a redraw never reshuffles rows under the cursor.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The roots this library was built from, so a rescan needs no arguments.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Start (or restart) a scan on a background thread. Returns immediately; poll the handle.
    ///
    /// Takes `&self` because the caller keeps playing from the current library while the new
    /// one is built - the scan works from a snapshot of the existing entries and hands back a
    /// whole replacement at the end, so nothing is ever half-updated. Files whose path and
    /// mtime match an existing entry are copied across without an ffprobe call, which is what
    /// makes a rescan of an unchanged folder a directory walk rather than an hour of forking.
    pub fn rescan(&self, roots: Vec<PathBuf>) -> ScanHandle {
        let cached: HashMap<PathBuf, Track> = self
            .tracks
            .iter()
            .map(|t| (t.path.clone(), t.clone()))
            .collect();
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        std::thread::spawn(move || scan(roots, Arc::new(cached), &worker));
        ScanHandle { shared }
    }

    /// Ranked fuzzy search over "artist album title" and the file stem, best match first.
    ///
    /// Deliberately three tiers rather than a single edit distance: a typed substring is what
    /// the user meant, a substring starting a word is what they meant more, and a scattered
    /// subsequence is the safety net that keeps results on screen while they are still typing
    /// or have fat-fingered a letter. Every whitespace-separated term must match somewhere, so
    /// adding a word narrows the list instead of widening it.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Track> {
        let needle = normalise(query.trim());
        if needle.is_empty() {
            return self.tracks.iter().take(limit).collect();
        }
        let terms: Vec<&str> = needle.split_whitespace().collect();
        let mut hits: Vec<(i64, usize)> = self
            .index
            .iter()
            .enumerate()
            .filter_map(|(at, entry)| Some((score(entry, &needle, &terms)?, at)))
            .collect();
        // Descending by score, then by position so equal matches keep the browser's own order
        // and a redraw never reorders them.
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.iter()
            .take(limit)
            .map(|&(_, index)| &self.tracks[index])
            .collect()
    }

    /// The one place a `Library` is built, so the search index cannot drift out of step with the
    /// tracks it describes. Sorting here is what makes every later listing - the browser, an
    /// empty search, the saved index - agree on an order.
    fn new(mut tracks: Vec<Track>, roots: Vec<PathBuf>) -> Library {
        tracks.sort_by(|a, b| a.path.cmp(&b.path));
        tracks.dedup_by(|a, b| a.path == b.path);
        let index = tracks.iter().map(Searchable::of).collect();
        Library {
            tracks,
            index,
            roots,
        }
    }
}

/// A running scan. Dropping it cancels the scan: the browser drops the old handle when it
/// starts a new one, and a scan nobody can collect is only burning CPU.
pub struct ScanHandle {
    shared: Arc<Shared>,
}

impl ScanHandle {
    /// Cheap enough for the UI's ten-times-a-second poll: three atomic loads, no locking.
    pub fn progress(&self) -> ScanProgress {
        ScanProgress {
            done: self.shared.done.load(Ordering::Relaxed),
            total: self.shared.total.load(Ordering::Relaxed),
            finished: self.shared.finished.load(Ordering::Acquire),
        }
    }

    /// `Some(library)` exactly once, when the scan has finished.
    ///
    /// A cancelled scan reports `finished` but yields nothing, ever: half a library would
    /// silently delete the tracks the walk had not reached yet. The caller keeps what it had.
    pub fn take(&self) -> Option<Library> {
        self.shared.result.lock().ok()?.take()
    }

    /// Ask the scan to stop. It gives up between files rather than mid-probe, so it ends within
    /// one ffprobe of the call - killing a probe would save milliseconds and leak a zombie.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
    }
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Everything the scan threads and the UI thread share. Counters are atomics so `progress()`
/// never waits on a worker; only the finished library needs a lock, and only once.
#[derive(Default)]
struct Shared {
    done: AtomicUsize,
    total: AtomicUsize,
    finished: AtomicBool,
    cancel: AtomicBool,
    result: Mutex<Option<Library>>,
}

impl Shared {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Publish the outcome. `finished` is stored last and with release ordering so a UI thread
    /// that sees it is guaranteed to see the library too.
    fn finish(&self, library: Option<Library>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = library;
        }
        self.finished.store(true, Ordering::Release);
    }
}

/// Walk, probe, publish. Runs on its own thread; the probing itself fans out to [`MAX_PROBES`]
/// more, since the bottleneck is process startup and not anything this thread does.
fn scan(roots: Vec<PathBuf>, cached: Arc<HashMap<PathBuf, Track>>, shared: &Arc<Shared>) {
    let files = Arc::new(collect_files(&roots, shared));
    if shared.cancelled() {
        shared.finish(None);
        return;
    }
    shared.total.store(files.len(), Ordering::Relaxed);

    let next = Arc::new(AtomicUsize::new(0));
    let found: Arc<Mutex<Vec<Track>>> = Arc::new(Mutex::new(Vec::with_capacity(files.len())));
    // One missing ffprobe would otherwise mean thousands of failed spawns; the first worker to
    // notice sets this and the rest fall back to filename-only entries, which still gives a
    // usable browser on a machine without ffmpeg installed.
    let probes = Arc::new(AtomicBool::new(true));

    let workers = MAX_PROBES.min(files.len().max(1));
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let files = Arc::clone(&files);
        let cached = Arc::clone(&cached);
        let found = Arc::clone(&found);
        let next = Arc::clone(&next);
        let probes = Arc::clone(&probes);
        let shared = Arc::clone(shared);
        handles.push(std::thread::spawn(move || {
            loop {
                if shared.cancelled() {
                    return;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = files.get(index) else {
                    return;
                };
                if let Some(track) = index_file(path, &cached, &probes)
                    && let Ok(mut list) = found.lock()
                {
                    list.push(track);
                }
                shared.done.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }

    if shared.cancelled() {
        shared.finish(None);
        return;
    }
    let tracks = found.lock().map(|list| list.clone()).unwrap_or_default();
    shared.done.store(files.len(), Ordering::Relaxed);
    shared.finish(Some(Library::new(tracks, roots)));
}

/// Index one file: reuse the cached entry when the file has not been touched, otherwise probe.
/// `None` means the file is not really playable and does not belong in the browser.
///
/// Rejected files are the one thing the cache cannot help with - there is no entry to key an
/// mtime off, so every rescan probes them again. That is deliberate: remembering rejections
/// would mean a file the user has since repaired stays invisible until they delete the index.
fn index_file(path: &Path, cached: &HashMap<PathBuf, Track>, probes: &AtomicBool) -> Option<Track> {
    let modified = mtime(path)?;
    if let Some(hit) = cached.get(path)
        && hit.modified == modified
    {
        return Some(hit.clone());
    }
    if !probes.load(Ordering::Relaxed) {
        return Some(untagged(path, modified));
    }
    match probe(path) {
        Probe::Tags(value) => read_tags(path, &value, modified),
        Probe::Unplayable => None,
        Probe::NoFfprobe => {
            probes.store(false, Ordering::Relaxed);
            Some(untagged(path, modified))
        }
    }
}

/// What one ffprobe attempt told us. The distinction that matters is between "ffprobe says this
/// file is junk", which skips the file, and "ffprobe is not installed", which must not silently
/// empty the user's library.
enum Probe {
    Tags(Value),
    Unplayable,
    NoFfprobe,
}

/// One `ffprobe` run. `-v quiet` keeps its diagnostics off the terminal the UI is drawing on.
fn probe(path: &Path) -> Probe {
    let output = Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_format"])
        .arg("-show_streams")
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Probe::NoFfprobe;
    };
    if !output.status.success() {
        // Exit status alone: unreadable, truncated, or not a media file at all.
        return Probe::Unplayable;
    }
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(value) if playable(&value) => Probe::Tags(value),
        _ => Probe::Unplayable,
    }
}

/// Whether ffprobe actually recognised the file, rather than guessing from its extension.
fn playable(value: &Value) -> bool {
    let score = value
        .get("format")
        .and_then(|f| f.get("probe_score"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if score < MIN_PROBE_SCORE {
        return false;
    }
    value
        .get("streams")
        .and_then(Value::as_array)
        .is_some_and(|streams| {
            streams.iter().any(|s| {
                matches!(
                    s.get("codec_type").and_then(Value::as_str),
                    Some("audio" | "video")
                )
            })
        })
}

/// Pull the fields we keep out of ffprobe's JSON.
///
/// Tags are looked up in `format.tags` first and then in each stream's tags, because Vorbis and
/// Opus carry their comments on the stream while everything else puts them on the container -
/// reading only one of the two loses the tags for a whole shelf of the library.
fn read_tags(path: &Path, value: &Value, modified: u64) -> Option<Track> {
    let mut tags: HashMap<String, String> = HashMap::new();
    let mut absorb = |source: Option<&Value>| {
        if let Some(map) = source.and_then(Value::as_object) {
            for (key, item) in map {
                if let Some(text) = item.as_str().map(str::trim)
                    && !text.is_empty()
                {
                    tags.entry(key.to_lowercase())
                        .or_insert_with(|| text.to_string());
                }
            }
        }
    };
    let format = value.get("format");
    absorb(format.and_then(|f| f.get("tags")));
    if let Some(streams) = value.get("streams").and_then(Value::as_array) {
        for stream in streams {
            absorb(stream.get("tags"));
        }
    }

    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| tags.get(*key))
            .cloned()
            .unwrap_or_default()
    };
    Some(Track {
        title: match pick(TITLE_KEYS) {
            title if title.is_empty() => clean_stem(path),
            title => title,
        },
        artist: pick(ARTIST_KEYS),
        album: pick(ALBUM_KEYS),
        duration: duration_of(value, format),
        modified,
        path: path.to_path_buf(),
        // A tag is only worth trusting if it is a plausible tempo: files in the wild
        // carry `0`, empty strings and the occasional sample rate in this field.
        bpm: BPM_KEYS
            .iter()
            .find_map(|key| tags.get(*key))
            .and_then(|text| text.trim().parse::<f64>().ok())
            .filter(|bpm| bpm.is_finite() && (20.0..=300.0).contains(bpm)),
    })
}

/// Playing time in seconds, `0.0` when nothing reported one. Matroska and WebM often leave
/// `format.duration` out and only time the streams, so the longest stream is the fallback.
fn duration_of(value: &Value, format: Option<&Value>) -> f64 {
    let seconds = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d > 0.0)
    };
    if let Some(duration) = seconds(format.and_then(|f| f.get("duration"))) {
        return duration;
    }
    value
        .get("streams")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| seconds(s.get("duration")))
        .fold(0.0, f64::max)
}

/// The entry for a file we could not read tags from but still want listed.
fn untagged(path: &Path, modified: u64) -> Track {
    Track {
        title: clean_stem(path),
        modified,
        path: path.to_path_buf(),
        ..Track::default()
    }
}

/// A file stem turned into something worth showing: underscores back to spaces, a leading
/// "01 - " track number dropped, runs of whitespace collapsed.
///
/// A bare leading number is only stripped when a separator follows it, because plenty of real
/// titles start with a digit and "15 Step" must not become "Step".
fn clean_stem(path: &Path) -> String {
    let stem = path
        .file_stem()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .replace('_', " ");
    let trimmed = stem.trim_start();
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    let rest = &trimmed[digits.len()..];
    let body = match rest.trim_start().strip_prefix(['-', '.', ')', ']']) {
        Some(after) if !digits.is_empty() && digits.len() <= 3 => after,
        _ => trimmed,
    };
    let cleaned = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned()
    } else {
        cleaned
    }
}

/// mtime in unix seconds. `None` for a file that vanished or that we may not stat, which drops
/// it from the index - the same outcome as an unreadable file, for the same reason.
fn mtime(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Every media file under `roots`, sorted and deduplicated so overlapping roots do not index a
/// file twice and the resulting library always lists in the same order.
///
/// Symlinks are never followed, in either direction: a link into a parent directory is a
/// bottomless walk, and a link to a file the walk will reach anyway is a duplicate row.
fn collect_files(roots: &[PathBuf], shared: &Shared) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = Vec::new();
    for root in roots {
        // A root that is a single file rather than a folder: indexed on its own, because a
        // settings line pointing at one file should give a library of one, not an empty
        // browser and no explanation.
        if root.is_file() {
            if is_media(root) {
                found.push(root.clone());
            }
        } else {
            stack.push((root.clone(), 0));
        }
    }
    while let Some((dir, depth)) = stack.pop() {
        if shared.cancelled() {
            return found;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // unreadable directory: skipped, not fatal
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                // Hidden directories are caches, version control and trash - never a library.
                if depth < MAX_DEPTH && !is_hidden(&path) {
                    stack.push((path, depth + 1));
                }
            } else if kind.is_file() && is_media(&path) && !is_hidden(&path) {
                found.push(path);
                // Something to show while the walk is still running, before the total is known.
                shared.total.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

impl Searchable {
    fn of(track: &Track) -> Searchable {
        Searchable {
            hay: normalise(&format!(
                "{} {} {} {}",
                track.artist,
                track.album,
                track.title,
                clean_stem(&track.path)
            )),
            title: normalise(&track.title),
        }
    }
}

/// Lowercase and strip accents, so that a query typed on a UK keyboard finds the half of any
/// real music collection that is spelled with diacritics: "bjork" has to find Björk, "sigur ros"
/// Sigur Rós, "jga" nothing at all. Both the index and the query go through here, so folding can
/// never make the two disagree.
fn normalise(text: &str) -> String {
    text.to_lowercase().chars().map(fold).collect()
}

/// The Latin-1 and Latin Extended-A letters, folded to the ASCII letter a keyboard offers.
/// Hand-written because a Unicode normalisation crate is three megabytes of tables to solve a
/// problem that, for track titles, is thirty lines. Anything outside these blocks - Cyrillic,
/// CJK, Greek - passes through and is matched verbatim, which is the right answer: there is no
/// ASCII key that means "ж".
fn fold(c: char) -> char {
    match c {
        'à'..='å' | 'ā' | 'ă' | 'ą' | 'æ' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' | 'ð' => 'd',
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' | 'ĸ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' | 'ŋ' => 'n',
        'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'œ' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' | 'ß' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    }
}

/// A term that starts a word.
const WORD_PREFIX: i64 = 1_000;
/// A term found inside a word.
const MIDWORD: i64 = 600;
/// A term whose letters merely appear in order.
const SUBSEQUENCE: i64 = 200;
/// The term is a whole word, not just its start.
const WHOLE_WORD: i64 = 150;
/// The complete query appears verbatim: "kid a" beats a track that happens to hold both words.
const PHRASE: i64 = 500;
/// Every term is in the title, not merely somewhere in the artist, album or path.
const TITLE: i64 = 400;
/// Ceiling on the "matched late in the string" penalty, kept well under the gap between tiers
/// so position only ever breaks ties within a tier.
const MAX_POSITION_PENALTY: usize = 80;
/// Characters a subsequence match may skip before it stops being a plausible typo, on top of an
/// allowance of one skip per two letters typed. Two covers a dropped letter and a swapped pair.
const MIN_SLACK: usize = 2;

/// Total score for one track, or `None` if any term is missing entirely.
fn score(entry: &Searchable, needle: &str, terms: &[&str]) -> Option<i64> {
    let mut total = 0;
    for term in terms {
        total += score_term(&entry.hay, term)?;
    }
    if terms.len() > 1 && entry.hay.contains(needle) {
        total += PHRASE;
    }
    // Whoever types "kid a" wants the song called Kid A, not the eleven other tracks on the
    // album of that name, which match the very same letters in the very same place.
    if terms.iter().all(|t| score_term(&entry.title, t).is_some()) {
        total += TITLE;
    }
    Some(total)
}

/// Best score for a single term: the highest-scoring of its substring matches, or a subsequence
/// match if it is not a substring anywhere.
fn score_term(hay: &str, term: &str) -> Option<i64> {
    let mut best: Option<i64> = None;
    for (at, _) in hay.match_indices(term) {
        let starts_word = hay[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let ends_word = hay[at + term.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        let mut value = if starts_word { WORD_PREFIX } else { MIDWORD };
        if starts_word && ends_word {
            value += WHOLE_WORD;
        }
        value -= hay[..at].chars().count().min(MAX_POSITION_PENALTY) as i64;
        best = Some(best.map_or(value, |b: i64| b.max(value)));
    }
    best.or_else(|| subsequence_score(hay, term))
}

/// Score the tightest run of `hay` containing `term`'s letters in order, or `None` if they are
/// not all there, or are too far apart to be a typo.
///
/// Tightest rather than first: "canda" typed for "canada" should be recognised by the six
/// letters it nearly spans, not by a "c" in the artist and an "a" at the end of the filename.
/// The slack limit is what keeps this tier from matching everything - over a sixty-character
/// haystack almost any short term appears in order somewhere, and a browser that never filters
/// anything out is no better than no search at all.
fn subsequence_score(hay: &str, term: &str) -> Option<i64> {
    let wanted: Vec<char> = term.chars().collect();
    let text: Vec<char> = hay.chars().collect();
    let first = *wanted.first()?;
    let mut best: Option<usize> = None;
    for start in 0..text.len() {
        if text[start] != first {
            continue;
        }
        let mut matched = 1;
        let mut end = start;
        for (at, c) in text.iter().enumerate().skip(start + 1) {
            if matched == wanted.len() {
                break;
            }
            if *c == wanted[matched] {
                matched += 1;
                end = at;
            }
        }
        if matched == wanted.len() && best.is_none_or(|b| end - start < b) {
            best = Some(end - start);
        }
    }
    // Only the letters the user did not type count against them, so a long term is not punished
    // for being long.
    let slack = best?.saturating_sub(wanted.len().saturating_sub(1));
    if slack > wanted.len() / 2 + MIN_SLACK {
        return None;
    }
    Some((SUBSEQUENCE - slack as i64).max(10))
}

/// Paths are stored as UTF-8 strings. A path that is not valid UTF-8 is dropped from the saved
/// index rather than mangled through a lossy conversion into a path that opens nothing; the
/// next scan simply probes it again.
fn path_to_json(path: &Path) -> Option<Value> {
    path.to_str().map(Value::from)
}

fn track_to_json(track: &Track) -> Option<Value> {
    Some(json!({
        "path": path_to_json(&track.path)?,
        "title": track.title,
        "artist": track.artist,
        "album": track.album,
        "duration": track.duration,
        "modified": track.modified,
        "bpm": track.bpm,
    }))
}

/// One entry back out of the index. A row missing its path is unusable and dropped; every other
/// field falls back to its default, so an index written by an older version still loads.
fn track_from_json(value: &Value) -> Option<Track> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let path = PathBuf::from(value.get("path").and_then(Value::as_str)?);
    let title = match text("title") {
        title if title.is_empty() => clean_stem(&path),
        title => title,
    };
    Some(Track {
        path,
        title,
        artist: text("artist"),
        album: text("album"),
        duration: value
            .get("duration")
            .and_then(Value::as_f64)
            .filter(|d| d.is_finite() && *d >= 0.0)
            .unwrap_or_default(),
        modified: value.get("modified").and_then(Value::as_u64).unwrap_or(0),
        bpm: value
            .get("bpm")
            .and_then(Value::as_f64)
            .filter(|bpm| bpm.is_finite() && (20.0..=300.0).contains(bpm)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(artist: &str, album: &str, title: &str, path: &str) -> Track {
        Track {
            path: PathBuf::from(path),
            title: title.to_string(),
            artist: artist.to_string(),
            album: album.to_string(),
            duration: 123.5,
            modified: 1_700_000_000,
            bpm: None,
        }
    }

    fn sample() -> Library {
        Library::new(
            vec![
                track(
                    "Aphex Twin",
                    "Selected Ambient Works",
                    "Xtal",
                    "/m/Aphex Twin/SAW/01 - Xtal.mp3",
                ),
                track(
                    "Boards of Canada",
                    "Music Has the Right to Children",
                    "Roygbiv",
                    "/m/BoC/03 - Roygbiv.ogg",
                ),
                track(
                    "Radiohead",
                    "Kid A",
                    "Idioteque",
                    "/m/Radiohead/Kid A/Idioteque.mp3",
                ),
                track(
                    "Radiohead",
                    "In Rainbows",
                    "Nude",
                    "/m/Radiohead/In Rainbows/Nude.m4a",
                ),
                Track {
                    path: PathBuf::from("/m/Unsorted/moderat_-_a_new_error.flac"),
                    title: "moderat - a new error".to_string(),
                    ..Track::default()
                },
            ],
            vec![PathBuf::from("/m")],
        )
    }

    fn titles(hits: &[&Track]) -> Vec<String> {
        hits.iter().map(|t| t.title.clone()).collect()
    }

    #[test]
    fn exact_substring_outranks_a_scattered_subsequence() {
        let library = sample();
        // "nude" is a word in one title and merely letters-in-order elsewhere.
        let hits = library.search("nude", 5);
        assert_eq!(hits[0].title, "Nude");
    }

    #[test]
    fn word_prefix_outranks_a_mid_word_hit() {
        let library = Library::new(
            vec![
                track("Someone", "Album", "Unmodern", "/m/a.mp3"),
                track("Moderat", "Album", "Rusty Nails", "/m/b.mp3"),
            ],
            Vec::new(),
        );
        // "moder" starts a word in the artist of one and sits inside "Unmodern" in the other.
        assert_eq!(
            titles(&library.search("moder", 2)),
            ["Rusty Nails", "Unmodern"]
        );
    }

    #[test]
    fn search_is_case_insensitive_and_matches_across_fields() {
        let library = sample();
        assert_eq!(library.search("APHEX xtal", 3)[0].title, "Xtal");
        assert_eq!(library.search("kid a idioteque", 3)[0].title, "Idioteque");
    }

    #[test]
    fn a_partial_artist_narrows_to_that_artist() {
        let library = sample();
        let hits = library.search("radioh", 5);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|t| t.artist == "Radiohead"));
    }

    #[test]
    fn every_term_must_match() {
        let library = sample();
        assert!(library.search("radiohead xtal", 5).is_empty());
    }

    #[test]
    fn a_typo_still_finds_the_track_through_the_subsequence_tier() {
        let library = sample();
        // "canda" is missing an "a" from "canada"; "roygbv" is missing the "i".
        assert_eq!(library.search("boards canda", 3)[0].title, "Roygbiv");
        assert_eq!(library.search("roygbv", 3)[0].title, "Roygbiv");
    }

    #[test]
    fn an_empty_query_returns_the_first_tracks_in_path_order() {
        let library = sample();
        let hits = library.search("   ", 3);
        assert_eq!(hits.len(), 3);
        let paths: Vec<&Path> = hits.iter().map(|t| t.path.as_path()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
        assert_eq!(library.search("", 99).len(), library.len());
    }

    #[test]
    fn the_limit_is_honoured() {
        assert_eq!(sample().search("a", 2).len(), 2);
    }

    #[test]
    fn a_title_match_outranks_the_album_of_the_same_name() {
        let library = sample();
        // Every Kid A track matches "kid a" through its album; only one is the song.
        assert_eq!(library.search("kid a", 3)[0].title, "Idioteque");
    }

    #[test]
    fn accents_are_folded_so_an_ascii_query_finds_them() {
        let library = Library::new(
            vec![track(
                "Björk",
                "Homogénic",
                "Jóga",
                "/m/Bjork/01 - Joga.flac",
            )],
            Vec::new(),
        );
        assert_eq!(library.search("bjork", 1).len(), 1);
        assert_eq!(library.search("joga", 1).len(), 1);
        assert_eq!(library.search("homogenic", 1).len(), 1);
        // And the accented spelling still works, because the query is folded too.
        assert_eq!(library.search("björk jóga", 1).len(), 1);
    }

    #[test]
    fn index_round_trips_through_disk() {
        let path = std::env::temp_dir()
            .join(format!("ytmlibrary_{}", std::process::id()))
            .join("index.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let library = sample();
        library.save(&path).unwrap();

        let back = Library::load(&path);
        assert_eq!(back.tracks(), library.tracks());
        assert_eq!(back.roots(), library.roots());
        assert_eq!(back.len(), library.len());
        // The search index is rebuilt on load, not stored.
        assert_eq!(back.search("roygbiv", 1)[0].title, "Roygbiv");

        // A corrupt index is an empty library, not an error.
        std::fs::write(&path, b"{not json at all").unwrap();
        assert!(Library::load(&path).is_empty());
        // So is one from another version.
        std::fs::write(&path, br#"{"version":999,"tracks":[]}"#).unwrap();
        assert!(Library::load(&path).is_empty());
        // So is a missing one.
        assert!(Library::load(Path::new("/nonexistent/index.json")).is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_tempo_in_the_tags_survives_the_index_and_a_silly_one_does_not() {
        // The whole point of reading `TBPM`: a scan that finds one seeds the analysis
        // cache, so the file is never decoded end to end to learn what it already says.
        let mut tagged = track("Artist", "Album", "Title", "/m/a.mp3");
        tagged.bpm = Some(128.0);
        let back = track_from_json(&track_to_json(&tagged).expect("to json")).expect("back");
        assert_eq!(back.bpm, Some(128.0), "the tempo did not survive the index");

        // Files in the wild carry `0`, empty strings and the occasional sample rate in
        // this field. A tempo that cannot be one is no tempo at all - seeding the cache
        // with 44100 would hand the automix a number it would then try to beat-match to.
        for silly in ["0", "", "not a number", "44100", "3"] {
            let value = json!({
                "format": { "probe_score": 100, "duration": "180.0",
                            "tags": { "title": "T", "TBPM": silly } },
                "streams": [ { "codec_type": "audio" } ]
            });
            let track = read_tags(Path::new("/m/a.mp3"), &value, 0).expect("a track");
            assert_eq!(track.bpm, None, "{silly:?} was accepted as a tempo");
        }
        let value = json!({
            "format": { "probe_score": 100, "duration": "180.0",
                        "tags": { "title": "T", "TBPM": "174" } },
            "streams": [ { "codec_type": "audio" } ]
        });
        let track = read_tags(Path::new("/m/a.mp3"), &value, 0).expect("a track");
        assert_eq!(track.bpm, Some(174.0), "a real tempo was thrown away");
    }

    #[test]
    fn untagged_files_fall_back_to_a_cleaned_stem() {
        assert_eq!(
            clean_stem(Path::new("/m/07 - some_untagged_track.mp3")),
            "some untagged track"
        );
        assert_eq!(
            clean_stem(Path::new("/m/13. Nils Frahm - Says.opus")),
            "Nils Frahm - Says"
        );
        // A title that genuinely starts with a number keeps it.
        assert_eq!(clean_stem(Path::new("/m/15 Step.m4a")), "15 Step");
        assert_eq!(clean_stem(Path::new("/m/.hidden")), ".hidden");
    }

    #[test]
    fn media_extensions_are_matched_case_insensitively() {
        assert!(is_media(Path::new("/m/a.FLAC")));
        assert!(is_media(Path::new("/m/a.Mp3")));
        assert!(!is_media(Path::new("/m/cover.jpg")));
        assert!(!is_media(Path::new("/m/notes.txt")));
        assert!(!is_media(Path::new("/m/noextension")));
    }
}
