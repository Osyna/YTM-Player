//! yt-dlp process wrapper: source classification, playlist resolution - including streaming
//! "smart loading" for Spotify - and background downloads.
//! Native JSON parsing replaces the old `jq` pipeline.

use crate::settings::{SaveFormat, Settings};
use crate::spotify;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// How long a finished-looking yt-dlp gets to actually exit before it is killed.
/// Its stdout has already closed by then, so anything still running is wedged - a stuck
/// ffmpeg merge, a hung connection teardown - and would otherwise hold the download slot
/// forever with the row frozen mid-percentage.
const EXIT_GRACE: Duration = Duration::from_secs(20);

/// `child.wait()` with a deadline: kill it once `limit` has passed and say so.
fn wait_with_deadline(child: &mut Child, limit: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("yt-dlp stopped responding and was terminated".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Where a URL's audio actually comes from. Decides which programs are required, whether
/// downloads may carry video, and how the playlist resolves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    YouTube,
    SoundCloud,
    Spotify,
    Local,
}

impl SourceKind {
    /// Sources with no video worth saving: downloads are always MP3 whatever the settings
    /// say. (A Spotify track's "video" is just its YouTube match - the user asked for the
    /// song, not somebody's upload of it.)
    pub fn audio_only(self) -> bool {
        matches!(self, SourceKind::SoundCloud | SourceKind::Spotify)
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceKind::YouTube => "YouTube",
            SourceKind::SoundCloud => "SoundCloud",
            SourceKind::Spotify => "Spotify",
            SourceKind::Local => "Local",
        }
    }
}

/// Classify before resolving: an existing path is local, everything else by host.
pub fn kind_of(url: &str) -> SourceKind {
    if Path::new(url).exists() {
        SourceKind::Local
    } else if spotify::is_spotify(url) {
        SourceKind::Spotify
    } else if url.contains("soundcloud.com") {
        SourceKind::SoundCloud
    } else {
        SourceKind::YouTube
    }
}

pub fn is_playlist(url: &str) -> bool {
    // YouTube marks playlists with `list=`. SoundCloud collects playlists under `/sets/`,
    // and its radio under `/stations/` or a track's `/recommended` page.
    url.contains("list=")
        || url.contains("/sets/")
        || url.contains("/stations/")
        || url.contains("/recommended")
}

/// A clipboard payload worth queueing: one plain http(s) link to a provider we play.
/// Local paths are deliberately excluded - silently playing arbitrary copied paths is
/// more surprise than feature.
pub fn is_media_link(text: &str) -> bool {
    let text = text.trim();
    (text.starts_with("https://") || text.starts_with("http://"))
        && !text.contains(char::is_whitespace)
        && [
            "youtube.com",
            "youtu.be",
            "soundcloud.com",
            "open.spotify.com",
        ]
        .iter()
        .any(|host| text.contains(host))
}

/// Run yt-dlp and hand back its stdout, turning any failure into a short human message.
/// Every blocking yt-dlp call goes through here so they all fail the same readable way.
fn run_ytdlp(args: &[&str]) -> Result<String, String> {
    let output = Command::new("yt-dlp")
        .args(args)
        .output()
        .map_err(|e| format!("can't run yt-dlp: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(classify_ytdlp_error(&String::from_utf8_lossy(
            &output.stderr,
        )))
    }
}

/// Run yt-dlp for its stdout alone, keeping whatever it printed even when it exits
/// non-zero. `--ignore-errors` skips a dead entry and carries on, but the process still
/// returns 1, so a success-only contract would throw away every good line in the batch
/// because of one private, deleted or geo-blocked track.
fn ytdlp_partial(args: &[&str]) -> String {
    Command::new("yt-dlp")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// One playlist entry: the title the playlist view shows, and the URL mpv plays - `None`
/// while a Spotify entry is still being matched on YouTube, or when no match was found.
pub struct Entry {
    pub title: String,
    pub url: Option<String>,
    /// False while `title` is a stand-in (a URL slug, a raw URL) rather than real
    /// metadata - the background title resolver targets exactly these.
    pub titled: bool,
}

/// Resolve a playlist URL into titled entries, in playlist order.
pub fn fetch_playlist_entries(url: &str) -> Result<Vec<Entry>, String> {
    let stdout = run_ytdlp(&["-j", "--flat-playlist", "--no-warnings", url])?;

    let mut entries = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let entry: Value =
            serde_json::from_str(line).map_err(|e| format!("bad playlist entry: {e}"))?;
        let Some(page_url) = entry_url(&entry) else {
            continue;
        };
        // SoundCloud sets list entries without titles in flat mode; the permalink slug is
        // close enough to read until the track actually plays.
        let real_title = entry
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && *t != "NA")
            .map(str::to_string);
        let titled = real_title.is_some();
        entries.push(Entry {
            title: real_title.unwrap_or_else(|| slug_title(&page_url)),
            url: Some(page_url),
            titled,
        });
    }
    Ok(entries)
}

/// The resolvable page URL for a flat-playlist entry. Prefer the human `webpage_url` (a clean
/// permalink on every extractor) over the raw `url` (an API URL on SoundCloud), falling back to a
/// YouTube watch URL built from a bare id.
fn entry_url(entry: &Value) -> Option<String> {
    let field = |key| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    field("webpage_url")
        .or_else(|| field("url"))
        .map(str::to_string)
        .or_else(|| field("id").map(|id| format!("https://youtu.be/{id}")))
}

/// `…/finding-mero-still-with-me` -> `finding mero still with me`.
fn slug_title(url: &str) -> String {
    let tail = url.trim_end_matches('/').rsplit('/').next().unwrap_or(url);
    let slug = tail.split(['?', '#']).next().unwrap_or(tail);
    slug.replace('-', " ")
}

/// A track resolved to its direct CDN URLs.
pub struct Resolved {
    pub title: String,
    /// Video stream, or the single muxed stream when the selector didn't have to merge.
    pub video: String,
    /// Separate audio stream when the selector merged two; `None` when `video` carries both.
    pub audio: Option<String>,
}

impl Resolved {
    /// What mpv actually opens: the bare audio stream when yt-dlp handed us separate tracks,
    /// otherwise the single muxed stream (which unavoidably carries video along with it).
    pub fn playback_url(&self) -> &str {
        self.audio.as_deref().unwrap_or(&self.video)
    }

    /// The video stream held back until `v` asks for a picture, keeping the default session
    /// audio-only. `None` for a muxed stream, whose video is already in [`Self::playback_url`].
    pub fn deferred_video(&self) -> Option<&str> {
        self.audio.as_ref().map(|_| self.video.as_str())
    }
}

/// Playlist entries are resolved by mpv's own `ytdl_hook`, one per track; audio-only keeps
/// those as cheap as the single-video path, and `v` resolves a picture on demand.
pub const AUDIO_ONLY_FORMAT: &str = "bestaudio/best";

/// Selector pairing full-quality audio with a video stream capped at `height`. Both come back
/// from one call, and mpv is handed only the audio, so the first `v` press costs no network.
pub fn live_format(height: Option<u16>) -> String {
    match height {
        // The trailing `/bestaudio/best` lets an audio-only source (SoundCloud, or a Spotify
        // link's YouTube match with no video at this height) still resolve to its audio.
        Some(h) => format!("bestvideo[height<={h}]+bestaudio/best[height<={h}]/bestaudio/best"),
        None => "bestvideo+bestaudio/best/bestaudio".to_string(),
    }
}

/// Resolve a watch URL to direct stream URLs, in yt-dlp's merge order (video, then audio).
///
/// This doubles as the validity check that used to be a separate `--simulate` call: an
/// unplayable URL fails here, before mpv spawns, with the same clean one-line message. Handing
/// mpv the result means its `ytdl_hook` never runs, so a track is extracted once, not twice.
pub fn resolve_track(url: &str, format: &str) -> Result<Resolved, String> {
    parse_resolved(&run_ytdlp(&[
        "-f",
        format,
        "--no-playlist",
        "--no-warnings",
        "--print",
        "%(title)s",
        "--print",
        "%(urls)s",
        url,
    ])?)
}

/// Split yt-dlp's two `--print` templates: the title, then one URL per stream it would have
/// merged. Two URLs mean separate video and audio; one means a muxed stream carrying both.
fn parse_resolved(stdout: &str) -> Result<Resolved, String> {
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    let title = lines.next().unwrap_or_default().to_string();
    let video = lines
        .next()
        .ok_or_else(|| "yt-dlp returned no stream URL".to_string())?
        .to_string();
    Ok(Resolved {
        title,
        video,
        audio: lines.next().map(str::to_string),
    })
}

/// A late resolution landing from the background resolver (Spotify smart loading).
pub enum ResolveEvent {
    Resolved { index: usize, url: String },
    Failed { index: usize },
    Done,
}

/// Everything [`Source::resolve`] produced: the source, the playlist file mpv should load
/// (if any), the mpv-order -> entries-order map when the two differ (Spotify, where a track
/// with no YouTube match never reaches mpv), and the channel streaming late resolutions.
pub struct Resolution {
    pub source: Source,
    pub playlist_file: Option<PathBuf>,
    pub playlist_map: Option<Vec<usize>>,
    pub resolver: Option<Receiver<ResolveEvent>>,
}

/// What we're playing: a single track, or a playlist resolved to titled entries.
pub struct Source {
    pub url: String,
    pub kind: SourceKind,
    /// Titled entries in playlist order; empty for a single track.
    pub entries: Vec<Entry>,
    pub is_playlist: bool,
    /// Direct URLs for a single track, resolved once up front. `None` for a playlist, where
    /// mpv's `ytdl_hook` still resolves each entry lazily as it advances - one call per track
    /// either way, so there is nothing to collapse there.
    resolved: Option<Resolved>,
}

impl Source {
    /// Resolve the URL before mpv ever spawns: a single track to its stream URLs, a playlist
    /// to its titled entries (plus the temp playlist file mpv should load).
    pub fn resolve(url: String, settings: &Settings) -> Result<Resolution, String> {
        let kind = kind_of(&url);
        let format = live_format(settings.quality.height());
        match kind {
            SourceKind::Local => resolve_local(url),
            SourceKind::Spotify => {
                let tracks = spotify::tracks(&url)?
                    .ok_or("That Spotify link isn't playable.".to_string())?;
                resolve_spotify(url, tracks, &format, settings.smart_loading)
            }
            _ if is_playlist(&url) => {
                let entries =
                    fetch_playlist_entries(&url).map_err(|e| format!("Can't play that: {e}"))?;
                if entries.is_empty() {
                    return Err("This playlist doesn't exist or has no tracks.".to_string());
                }
                let file = write_playlist_file(entries.iter().filter_map(|e| e.url.as_deref()))
                    .map_err(|e| format!("can't write the playlist file: {e}"))?;
                Ok(Resolution {
                    source: Source {
                        url,
                        kind,
                        entries,
                        is_playlist: true,
                        resolved: None,
                    },
                    playlist_file: Some(file),
                    playlist_map: None,
                    resolver: None,
                })
            }
            _ => {
                let resolved =
                    resolve_track(&url, &format).map_err(|e| format!("Can't play that: {e}"))?;
                Ok(Resolution {
                    source: Source {
                        url,
                        kind,
                        entries: Vec::new(),
                        is_playlist: false,
                        resolved: Some(resolved),
                    },
                    playlist_file: None,
                    playlist_map: None,
                    resolver: None,
                })
            }
        }
    }

    /// The stream URLs we resolved ourselves, when there are any.
    pub fn resolved(&self) -> Option<&Resolved> {
        self.resolved.as_ref()
    }

    /// URL of the entry at `index`, falling back to whatever the user originally passed.
    pub fn track_url(&self, index: usize) -> String {
        self.entries
            .get(index)
            .and_then(|e| e.url.clone())
            .unwrap_or_else(|| self.url.clone())
    }

    /// The empty session a bare `ytmplayer` launch starts with: nothing loaded, nothing
    /// implied. Every capability check reads this as "no media yet".
    pub fn none() -> Source {
        Source {
            url: String::new(),
            kind: SourceKind::Local,
            entries: Vec::new(),
            is_playlist: false,
            resolved: None,
        }
    }

    /// True until the session first opens something.
    pub fn is_empty(&self) -> bool {
        self.url.is_empty() && self.entries.is_empty() && self.resolved.is_none()
    }
}

/// Extensions worth loading from a local directory. mpv plays more than these, but sweeping
/// everything in would drag covers and lyrics into the playlist.
const MEDIA_EXTS: &[&str] = &[
    "aac", "aif", "aiff", "avi", "flac", "m4a", "m4v", "mka", "mkv", "mov", "mp3", "mp4", "oga",
    "ogg", "opus", "wav", "webm", "wma",
];

/// Entries for a local path that expands to a list - a directory of media files or an
/// `.m3u`/`.m3u8`/`.pls` - or `None` for a single playable file.
fn local_entries(path: &Path) -> Result<Option<Vec<Entry>>, String> {
    if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|e| format!("Can't read {}: {e}", path.display()))?
            .filter_map(Result::ok)
            .map(|d| d.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| MEDIA_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            })
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(format!("No playable media files in {}.", path.display()));
        }
        return Ok(Some(
            files
                .iter()
                .map(|p| Entry {
                    title: file_title(p),
                    url: Some(p.display().to_string()),
                    titled: true,
                })
                .collect(),
        ));
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if matches!(ext.as_str(), "m3u" | "m3u8" | "pls") {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let entries = parse_playlist_text(&text, ext == "pls", dir);
        if entries.is_empty() {
            return Err(format!("{} lists no playable entries.", path.display()));
        }
        return Ok(Some(entries));
    }
    Ok(None)
}

/// A path on disk: one file, a directory of media files, or an `.m3u`/`.pls` list.
/// No yt-dlp anywhere - mpv opens local files itself.
fn resolve_local(url: String) -> Result<Resolution, String> {
    let path = std::fs::canonicalize(&url).map_err(|e| format!("Can't open {url}: {e}"))?;
    if let Some(entries) = local_entries(&path)? {
        return local_playlist(url, entries);
    }

    // A single file. Reusing `Resolved` with the path as the "muxed stream" hands `v` the
    // file's own embedded video, if it has any.
    let title = file_title(&path);
    let target = path.display().to_string();
    Ok(Resolution {
        source: Source {
            url: target.clone(),
            kind: SourceKind::Local,
            entries: Vec::new(),
            is_playlist: false,
            resolved: Some(Resolved {
                title,
                video: target,
                audio: None,
            }),
        },
        playlist_file: None,
        playlist_map: None,
        resolver: None,
    })
}

/// Wrap already-built local entries into a playlist [`Resolution`].
fn local_playlist(url: String, entries: Vec<Entry>) -> Result<Resolution, String> {
    let file = write_playlist_file(entries.iter().filter_map(|e| e.url.as_deref()))
        .map_err(|e| format!("can't write the playlist file: {e}"))?;
    Ok(Resolution {
        source: Source {
            url,
            kind: SourceKind::Local,
            entries,
            is_playlist: true,
            resolved: None,
        },
        playlist_file: Some(file),
        playlist_map: None,
        resolver: None,
    })
}

/// `.m3u`/`.m3u8` lines or `.pls` `FileN=` values; relative paths are anchored at the
/// list's own directory, URLs pass through untouched.
fn parse_playlist_text(text: &str, is_pls: bool, dir: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let target = if is_pls {
            match line.split_once('=') {
                Some((key, value)) if key.starts_with("File") => value.trim(),
                _ => continue,
            }
        } else {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            line
        };
        if target.is_empty() {
            continue;
        }
        if target.contains("://") {
            entries.push(Entry {
                title: target.to_string(),
                url: Some(target.to_string()),
                // A bare URL is not a title; the background resolver fetches one.
                titled: false,
            });
        } else {
            let path = dir.join(target);
            entries.push(Entry {
                title: file_title(&path),
                url: Some(path.display().to_string()),
                titled: true,
            });
        }
    }
    entries
}

fn file_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Resolve a Spotify link's tracks into a playable [`Resolution`]. One track resolves up front
/// exactly like a single video. Several become a playlist of YouTube matches - resolved either
/// in one blocking batch, or (smart loading) seeded with the first match so playback starts
/// immediately while the rest stream in from a background thread.
fn resolve_spotify(
    original: String,
    tracks: Vec<spotify::Track>,
    format: &str,
    smart: bool,
) -> Result<Resolution, String> {
    if tracks.len() == 1 {
        let search = format!("ytsearch1:{}", tracks[0].query);
        let resolved = resolve_track(&search, format)
            .map_err(|e| format!("Couldn't find that track on YouTube: {e}"))?;
        return Ok(Resolution {
            source: Source {
                url: search,
                kind: SourceKind::Spotify,
                entries: Vec::new(),
                is_playlist: false,
                resolved: Some(resolved),
            },
            playlist_file: None,
            playlist_map: None,
            resolver: None,
        });
    }

    let mut entries: Vec<Entry> = tracks
        .iter()
        .map(|t| Entry {
            title: t.title.clone(),
            url: None,
            titled: true,
        })
        .collect();

    if !smart {
        eprintln!("Resolving {} Spotify tracks via YouTube…", tracks.len());
        let queries: Vec<String> = tracks.iter().map(|t| t.query.clone()).collect();
        let pairs = resolve_search_pairs(&queries)?;
        let mut waiting = query_lookup(&queries);
        let mut map = Vec::new();
        for (query, url) in pairs {
            let Some(index) = waiting
                .get_mut(query.as_str())
                .and_then(VecDeque::pop_front)
            else {
                continue;
            };
            entries[index].url = Some(url);
            map.push(index);
        }
        if map.is_empty() {
            return Err("Couldn't find any of those tracks on YouTube.".to_string());
        }
        let file = write_playlist_file(map.iter().filter_map(|&i| entries[i].url.as_deref()))
            .map_err(|e| format!("can't write the playlist file: {e}"))?;
        return Ok(Resolution {
            source: Source {
                url: original,
                kind: SourceKind::Spotify,
                entries,
                is_playlist: true,
                resolved: None,
            },
            playlist_file: Some(file),
            playlist_map: Some(map),
            resolver: None,
        });
    }

    // Smart loading: the seed is the first track with a YouTube match - resolved alone, so
    // playback starts after one search instead of after all of them.
    let mut seed = None;
    for (index, track) in tracks.iter().enumerate() {
        match resolve_search_pairs(std::slice::from_ref(&track.query)) {
            Ok(pairs) if !pairs.is_empty() => {
                seed = Some((index, pairs.into_iter().next().expect("non-empty").1));
                break;
            }
            _ => {}
        }
    }
    let Some((seed_index, seed_url)) = seed else {
        return Err("Couldn't find any of those tracks on YouTube.".to_string());
    };
    entries[seed_index].url = Some(seed_url.clone());

    let pending: Vec<(usize, String)> = tracks
        .iter()
        .enumerate()
        .skip(seed_index + 1)
        .map(|(i, t)| (i, t.query.clone()))
        .collect();
    let resolver = (!pending.is_empty()).then(|| spawn_search_resolver(pending));

    let file = write_playlist_file(std::iter::once(seed_url.as_str()))
        .map_err(|e| format!("can't write the playlist file: {e}"))?;
    Ok(Resolution {
        source: Source {
            url: original,
            kind: SourceKind::Spotify,
            entries,
            is_playlist: true,
            resolved: None,
        },
        playlist_file: Some(file),
        playlist_map: Some(vec![seed_index]),
        resolver: Some(resolver.unwrap_or_else(|| {
            // Nothing left to resolve: a closed channel whose `Done` already happened.
            let (tx, rx) = channel();
            let _ = tx.send(ResolveEvent::Done);
            rx
        })),
    })
}

/// `query text` -> queue of entry indices still waiting for it (duplicate queries queue up).
fn query_lookup(queries: &[String]) -> HashMap<&str, VecDeque<usize>> {
    let mut waiting: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (index, query) in queries.iter().enumerate() {
        waiting.entry(query.as_str()).or_default().push_back(index);
    }
    waiting
}

/// One yt-dlp line of `--print "%(playlist)s\x1f%(url)s"`: the echoed query and its match.
fn parse_search_pair(line: &str) -> Option<(String, String)> {
    let (query, url) = line.trim().split_once('\u{1f}')?;
    (!query.is_empty() && !url.is_empty()).then(|| (query.to_string(), url.to_string()))
}

/// Resolve YouTube searches to `(query, watch URL)` pairs in one blocking yt-dlp call.
/// Lines come back in argument order; a search that matches nothing prints nothing, and
/// `%(playlist)s` echoes the query so every line names the search it answers.
fn resolve_search_pairs(queries: &[String]) -> Result<Vec<(String, String)>, String> {
    let searches: Vec<String> = queries.iter().map(|q| format!("ytsearch1:{q}")).collect();
    let mut args = vec![
        "--flat-playlist",
        "--no-warnings",
        "--print",
        "%(playlist)s\u{1f}%(url)s",
    ];
    args.extend(searches.iter().map(String::as_str));
    Ok(run_ytdlp(&args)?
        .lines()
        .filter_map(parse_search_pair)
        .collect())
}

/// Resolve a URL or path opened *at runtime* (the `o` prompt, the clipboard watcher)
/// into queueable entries, every one carrying a playable URL. Runs on a worker thread,
/// so it may block on yt-dlp freely; unmatched Spotify tracks are dropped rather than
/// queued as dead rows.
pub fn resolve_for_queue(url: &str) -> Result<Vec<Entry>, String> {
    match kind_of(url) {
        SourceKind::Local => {
            let path = std::fs::canonicalize(url).map_err(|e| format!("Can't open {url}: {e}"))?;
            match local_entries(&path)? {
                Some(entries) => Ok(entries),
                None => Ok(vec![Entry {
                    title: file_title(&path),
                    url: Some(path.display().to_string()),
                    titled: true,
                }]),
            }
        }
        SourceKind::Spotify => {
            let tracks = crate::spotify::tracks(url)?
                .ok_or("That Spotify link isn't playable.".to_string())?;
            let queries: Vec<String> = tracks.iter().map(|t| t.query.clone()).collect();
            let mut matches = query_lookup(&queries);
            let mut urls: Vec<Option<String>> = vec![None; tracks.len()];
            for (query, url) in resolve_search_pairs(&queries)? {
                if let Some(index) = matches
                    .get_mut(query.as_str())
                    .and_then(VecDeque::pop_front)
                {
                    urls[index] = Some(url);
                }
            }
            let entries: Vec<Entry> = tracks
                .into_iter()
                .zip(urls)
                .filter_map(|(t, url)| {
                    url.map(|url| Entry {
                        title: t.title,
                        url: Some(url),
                        titled: true,
                    })
                })
                .collect();
            if entries.is_empty() {
                return Err("No YouTube match for that Spotify link.".to_string());
            }
            Ok(entries)
        }
        _ if is_playlist(url) => {
            let entries = fetch_playlist_entries(url)?;
            if entries.is_empty() {
                return Err("This playlist doesn't exist or has no tracks.".to_string());
            }
            Ok(entries)
        }
        _ => {
            let out = run_ytdlp(&[
                "--no-playlist",
                "--no-warnings",
                "--print",
                "%(title)s\u{1f}%(webpage_url)s",
                url,
            ])?;
            let (title, page) = out
                .lines()
                .find_map(|l| l.trim().split_once('\u{1f}'))
                .ok_or_else(|| "yt-dlp returned nothing for that link".to_string())?;
            Ok(vec![Entry {
                title: title.to_string(),
                url: Some(page.to_string()),
                titled: true,
            }])
        }
    }
}

/// A real title for entry `index`, fetched by the background title resolver.
pub struct TitleEvent {
    pub index: usize,
    /// The entry URL the title belongs to, so a receiver whose indexes have shifted
    /// (an add-next insert) can still match the right entry.
    pub url: String,
    pub title: String,
}

/// Fetch real titles for entries whose display name is a stand-in (URL slugs, numeric
/// SoundCloud API ids, raw URLs from an `.m3u`). Batched yt-dlp calls on one background
/// thread; `%(original_url)s` echoes each input URL, which maps every line back to its
/// entry even when a dead URL in the middle prints nothing.
///
/// The batch is read for partial output on purpose: one unplayable track makes yt-dlp
/// exit non-zero, and treating that as failure used to discard every title it had
/// already printed - a whole page of the queue left showing raw ids.
pub fn spawn_title_resolver(items: Vec<(usize, String)>) -> Receiver<TitleEvent> {
    const BATCH: usize = 12;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for chunk in items.chunks(BATCH) {
            let mut waiting: HashMap<&str, VecDeque<usize>> = HashMap::new();
            for (index, url) in chunk {
                waiting.entry(url.as_str()).or_default().push_back(*index);
            }
            let mut args = vec![
                "--no-playlist",
                "--skip-download",
                "--ignore-errors",
                "--no-warnings",
                "--print",
                "%(original_url)s\u{1f}%(title)s",
            ];
            args.extend(chunk.iter().map(|(_, url)| url.as_str()));
            let stdout = ytdlp_partial(&args);
            for event in take_titles(&stdout, &mut waiting) {
                if tx.send(event).is_err() {
                    return; // player gone
                }
            }

            // A single dead URL can make yt-dlp print nothing for the whole batch, which
            // used to leave its 11 innocent neighbours showing raw ids forever. Retry
            // whatever the batch left unresolved, one URL at a time, so one bad link only
            // costs itself.
            let leftovers: Vec<&str> = waiting
                .iter()
                .filter(|(_, queue)| !queue.is_empty())
                .map(|(url, _)| *url)
                .collect();
            for url in leftovers {
                let stdout = ytdlp_partial(&[
                    "--no-playlist",
                    "--skip-download",
                    "--ignore-errors",
                    "--no-warnings",
                    "--print",
                    "%(original_url)s\u{1f}%(title)s",
                    url,
                ]);
                for event in take_titles(&stdout, &mut waiting) {
                    if tx.send(event).is_err() {
                        return; // player gone
                    }
                }
            }
        }
    });
    rx
}

/// Turn yt-dlp `url\u{1f}title` lines into events, consuming each match from `waiting`.
/// Split out from the resolver thread so the batch/retry bookkeeping is testable without
/// running yt-dlp: whatever is left in `waiting` afterwards is exactly what got no title.
fn take_titles(stdout: &str, waiting: &mut HashMap<&str, VecDeque<usize>>) -> Vec<TitleEvent> {
    let mut events = Vec::new();
    for line in stdout.lines() {
        let Some((url, title)) = line.trim().split_once('\u{1f}') else {
            continue;
        };
        if title.is_empty() || title == "NA" {
            continue;
        }
        if let Some(index) = waiting.get_mut(url).and_then(VecDeque::pop_front) {
            events.push(TitleEvent {
                index,
                url: url.to_string(),
                title: title.to_string(),
            });
        }
    }
    events
}

/// Resolve `(entry index, query)` searches on a background thread, streaming each match as it
/// lands. One yt-dlp process serves the whole batch; its stdout arrives one line per resolved
/// search, so the channel sees results in playlist order without waiting for the tail.
fn spawn_search_resolver(pending: Vec<(usize, String)>) -> Receiver<ResolveEvent> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let queries: Vec<String> = pending.iter().map(|(_, q)| q.clone()).collect();
        let mut waiting: HashMap<String, VecDeque<usize>> = HashMap::new();
        for (index, query) in &pending {
            waiting.entry(query.clone()).or_default().push_back(*index);
        }

        let mut cmd = Command::new("yt-dlp");
        cmd.args([
            "--flat-playlist",
            "--no-warnings",
            "--print",
            "%(playlist)s\u{1f}%(url)s",
        ]);
        cmd.args(queries.iter().map(|q| format!("ytsearch1:{q}")));
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let fail_all = |waiting: &mut HashMap<String, VecDeque<usize>>| {
            for queue in waiting.values_mut() {
                while let Some(index) = queue.pop_front() {
                    let _ = tx.send(ResolveEvent::Failed { index });
                }
            }
        };

        let Ok(mut child) = cmd.spawn() else {
            fail_all(&mut waiting);
            let _ = tx.send(ResolveEvent::Done);
            return;
        };
        if let Some(stdout) = child.stdout.take() {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                let Some((query, url)) = parse_search_pair(&line) else {
                    continue;
                };
                if let Some(index) = waiting.get_mut(&query).and_then(VecDeque::pop_front) {
                    let _ = tx.send(ResolveEvent::Resolved { index, url });
                }
            }
        }
        let _ = child.wait();
        // Anything still waiting got no line: no YouTube match.
        fail_all(&mut waiting);
        let _ = tx.send(ResolveEvent::Done);
    });
    rx
}

/// Map yt-dlp's stderr onto a short, human message instead of a raw traceback-style dump.
fn classify_ytdlp_error(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    const NETWORK: &[&str] = &[
        "failed to resolve",
        "name or service not known",
        "network is unreachable",
        "connection refused",
        "temporary failure in name resolution",
        "no route to host",
        "urlopen error",
    ];
    const UNAVAILABLE: &[&str] = &[
        "video unavailable",
        "private video",
        "this video has been removed",
        "content isn't available",
        "account associated with this video has been terminated",
    ];
    if NETWORK.iter().any(|p| lower.contains(p)) {
        "No internet connection.".to_string()
    } else if UNAVAILABLE.iter().any(|p| lower.contains(p)) {
        "This video doesn't exist or isn't available.".to_string()
    } else {
        // Prefer an actual ERROR line over a preceding retry WARNING, when both are present.
        let picked = stderr
            .lines()
            .find(|l| l.trim_start().starts_with("ERROR:"))
            .or_else(|| stderr.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or(stderr)
            .trim();
        format!("yt-dlp error: {picked}")
    }
}

pub fn write_playlist_file<'a>(urls: impl Iterator<Item = &'a str>) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("ytmplayer_playlist_{}.txt", std::process::id()));
    let joined: Vec<&str> = urls.collect();
    std::fs::write(&path, joined.join("\n"))?;
    Ok(path)
}

/// What one download fetches, decided from the settings and the source: audio-only sources
/// force MP3 no matter the configured format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DownloadSpec {
    Audio,
    Video { height: Option<u16> },
}

impl DownloadSpec {
    pub fn choose(settings: &Settings, kind: SourceKind) -> DownloadSpec {
        if kind.audio_only() || settings.format == SaveFormat::Mp3 {
            DownloadSpec::Audio
        } else {
            DownloadSpec::Video {
                height: settings.quality.height(),
            }
        }
    }

    /// yt-dlp format-selection args, including the fallbacks for when ffmpeg is missing:
    /// height caps need it to merge separate video+audio streams, MP3 to transcode.
    fn ytdlp_args(self, has_ffmpeg: bool) -> Vec<String> {
        match self {
            DownloadSpec::Audio => {
                let mut args = vec!["-f".to_string(), "ba/b".to_string()];
                if has_ffmpeg {
                    args.extend(["-x", "--audio-format", "mp3"].map(String::from));
                }
                args
            }
            DownloadSpec::Video { height } => {
                let format = match (height, has_ffmpeg) {
                    (Some(h), true) => format!("bv*[height<={h}]+ba/b[height<={h}]"),
                    (Some(h), false) => format!("b[height<={h}]/b"),
                    (None, true) => "bv*+ba/b".to_string(),
                    (None, false) => "b".to_string(),
                };
                vec!["-f".to_string(), format]
            }
        }
    }
}

#[derive(Clone)]
pub enum DownloadState {
    Idle,
    Running { percent: f32 },
    Done { path: String },
    Failed { message: String },
    Cancelled,
}

/// Strip path separators and control characters so a video title is safe as a filename.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "video".to_string()
    } else {
        // Keep well clear of the 255-byte filename limit on typical filesystems.
        trimmed.chars().take(150).collect()
    }
}

/// `--stream-record` never writes a Matroska trailer while mpv keeps running (mpv only closes
/// it at process exit), so the raw temp file always looks like a "premature EOF" container to
/// other readers even once every byte is on disk. The content itself is intact - one ffmpeg
/// pass gets back a normal, cleanly finalized file. That pass also does the MP3 transcode when
/// asked, rather than remuxing first and re-encoding after. Falls back to the raw file if
/// ffmpeg is unavailable or the pass fails; that file is still playable, just as a raw `.mkv`
/// without upfront duration metadata.
fn finalize_recording(temp: &std::path::Path, as_mp3: bool) -> (std::path::PathBuf, &'static str) {
    if !has_ffmpeg() {
        return (temp.to_path_buf(), "mkv");
    }
    let ext = if as_mp3 { "mp3" } else { "mkv" };
    let codec: &[&str] = if as_mp3 {
        &["-vn", "-c:a", "libmp3lame", "-q:a", "2"]
    } else {
        &["-c", "copy"]
    };
    let out = temp.with_extension(format!("final.{ext}"));
    let converted = succeeds(
        Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(temp)
            .args(codec)
            .arg(&out),
    );
    if converted && out.exists() {
        let _ = std::fs::remove_file(temp);
        (out, ext)
    } else {
        let _ = std::fs::remove_file(&out);
        (temp.to_path_buf(), "mkv")
    }
}

/// Move an already-recorded stream into `downloads/`. This is the zero-network path: the bytes
/// were captured by mpv while the track was streaming, so only the conversion is left to do.
pub fn save_recording(temp: &std::path::Path, title: &str, as_mp3: bool) -> Result<String, String> {
    let (finalized, ext) = finalize_recording(temp, as_mp3);
    std::fs::create_dir_all("downloads").map_err(|e| format!("can't create downloads/: {e}"))?;
    let dest = PathBuf::from("downloads").join(format!("{}.{ext}", sanitize_filename(title)));
    // Same filesystem in the common case, so try the cheap rename before falling back to a copy.
    if std::fs::rename(&finalized, &dest).is_err() {
        std::fs::copy(&finalized, &dest).map_err(|e| format!("can't save recording: {e}"))?;
        let _ = std::fs::remove_file(&finalized);
    }
    Ok(dest.display().to_string())
}

/// Run a command with its output discarded; true if it exited 0. Used for "is this installed"
/// probes and for fire-and-forget ffmpeg work.
pub fn succeeds(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn has_ffmpeg() -> bool {
    succeeds(Command::new("ffmpeg").arg("-version"))
}

/// Owns everything needed to start, observe, and cancel a background download from the UI
/// thread, so callers hold one handle instead of juggling several loose `Arc`s.
#[derive(Clone)]
pub struct DownloadControl {
    state: Arc<Mutex<DownloadState>>,
    pid: Arc<Mutex<Option<u32>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl Default for DownloadControl {
    fn default() -> Self {
        DownloadControl {
            state: Arc::new(Mutex::new(DownloadState::Idle)),
            pid: Arc::new(Mutex::new(None)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl DownloadControl {
    pub fn snapshot(&self) -> DownloadState {
        self.state.lock().clone()
    }

    pub fn is_running(&self) -> bool {
        matches!(*self.state.lock(), DownloadState::Running { .. })
    }

    /// Directly report an outcome, bypassing the background-thread machinery. Used by the
    /// cache fast path (which finishes synchronously) and to acknowledge a finished state
    /// before starting the next queued download.
    pub fn set(&self, state: DownloadState) {
        *self.state.lock() = state;
    }

    /// Stop the in-flight download, if any. Sends SIGTERM immediately (rather than waiting on
    /// the background thread to notice) so the UI reflects the cancellation right away.
    pub fn cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock() {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
        *self.state.lock() = DownloadState::Cancelled;
    }

    /// Download `url` as `spec` into `downloads/` in the background. The terminal state
    /// (`Done`/`Failed`/`Cancelled`) stays until the caller acknowledges it - display timing
    /// belongs to the UI, and the playlist queue needs to read outcomes reliably.
    pub fn start(&self, url: String, spec: DownloadSpec) {
        self.cancel_requested.store(false, Ordering::SeqCst);
        *self.state.lock() = DownloadState::Running { percent: 0.0 };
        let state = self.state.clone();
        let pid_slot = self.pid.clone();
        let cancelled = self.cancel_requested.clone();

        std::thread::spawn(move || {
            if let Err(e) = std::fs::create_dir_all("downloads") {
                *state.lock() = DownloadState::Failed {
                    message: format!("can't create downloads/: {e}"),
                };
                return;
            }

            let mut cmd = Command::new("yt-dlp");
            cmd.args(spec.ytdlp_args(has_ffmpeg()))
                // A few parallel fragment connections noticeably speed up DASH downloads.
                .args(["--concurrent-fragments", "4", "--newline"])
                .args(["-o", "downloads/%(title)s.%(ext)s", &url])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    *state.lock() = DownloadState::Failed {
                        message: format!("can't start yt-dlp: {e}"),
                    };
                    return;
                }
            };
            *pid_slot.lock() = Some(child.id());

            let stdout = child.stdout.take().expect("piped stdout");
            let mut last_destination: Option<String> = None;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if let Some(pct) = parse_percent(&line) {
                    *state.lock() = DownloadState::Running { percent: pct };
                }
                if let Some(path) = extract_destination(&line) {
                    last_destination = Some(path);
                }
            }

            let status = wait_with_deadline(&mut child, EXIT_GRACE);
            *pid_slot.lock() = None;
            *state.lock() = if cancelled.load(Ordering::SeqCst) {
                DownloadState::Cancelled
            } else {
                match status {
                    Ok(s) if s.success() => DownloadState::Done {
                        path: last_destination.unwrap_or_else(|| "downloads/".to_string()),
                    },
                    Ok(s) => DownloadState::Failed {
                        message: format!("yt-dlp exited with {s}"),
                    },
                    Err(message) => DownloadState::Failed { message },
                }
            };
        });
    }
}

/// Age at which an abandoned `.part` is considered dead rather than in flight.
/// Comfortably longer than any real download, so a running one is never touched.
const PART_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Delete `downloads/*.part` / `*.ytdl` left by killed or crashed downloads.
///
/// yt-dlp resumes its own partial file when the same download is started again, but a
/// download that is never retried leaves the bytes on disk forever - gigabytes of them,
/// invisibly. Only files older than [`PART_MAX_AGE`] go, so an in-flight download (and a
/// resumable one from earlier today) survives.
pub fn sweep_stale_parts() {
    sweep_parts_in(Path::new("downloads"), PART_MAX_AGE);
}

/// [`sweep_stale_parts`] against an explicit directory and age, so it is testable.
fn sweep_parts_in(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // no downloads yet: nothing to sweep
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_partial = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "part" || e == "ytdl");
        if !is_partial {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Parse a `[download]  42.3% of ...` progress line.
fn parse_percent(line: &str) -> Option<f32> {
    let line = line.trim();
    if !line.starts_with("[download]") {
        return None;
    }
    let pct_idx = line.find('%')?;
    let start = line[..pct_idx].rfind(char::is_whitespace)? + 1;
    line[start..pct_idx].parse::<f32>().ok()
}

/// Pull the output path out of yt-dlp's "Destination: X" / "Merging formats into "X"" lines.
fn extract_destination(line: &str) -> Option<String> {
    for marker in ["Destination: ", "Merging formats into "] {
        if let Some(pos) = line.find(marker) {
            let mut path = line[pos + marker.len()..].trim().to_string();
            if path.starts_with('"') && path.ends_with('"') && path.len() >= 2 {
                path = path[1..path.len() - 1].to_string();
            }
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::VideoQuality;

    fn waiting_for<'a>(urls: &[&'a str]) -> HashMap<&'a str, VecDeque<usize>> {
        let mut waiting: HashMap<&str, VecDeque<usize>> = HashMap::new();
        for (i, url) in urls.iter().enumerate() {
            waiting.entry(url).or_default().push_back(i);
        }
        waiting
    }

    #[test]
    fn sweep_takes_abandoned_parts_and_leaves_finished_files() {
        let dir = std::env::temp_dir().join(format!("ytmsweep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["dead.mp4.part", "dead.ytdl", "keeper.mp3", "cover.jpg"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        // Zero max age: everything written above already counts as abandoned.
        sweep_parts_in(&dir, Duration::ZERO);
        assert!(!dir.join("dead.mp4.part").exists());
        assert!(!dir.join("dead.ytdl").exists());
        assert!(
            dir.join("keeper.mp3").exists(),
            "deleted a finished download"
        );
        assert!(dir.join("cover.jpg").exists());

        // A part younger than the cutoff is a download in flight - never touched.
        std::fs::write(dir.join("live.mp4.part"), b"x").unwrap();
        sweep_parts_in(&dir, PART_MAX_AGE);
        assert!(dir.join("live.mp4.part").exists(), "killed a live download");

        // A missing downloads/ is not an error.
        std::fs::remove_dir_all(&dir).unwrap();
        sweep_parts_in(&dir, Duration::ZERO);
    }

    #[test]
    fn take_titles_matches_lines_back_to_entries() {
        let mut waiting = waiting_for(&["a", "b", "c"]);
        // Out of order, with a junk line and an "NA" title in the middle.
        let out = "c\u{1f}Third\nnot a pair\nb\u{1f}NA\na\u{1f}First\n";
        let events = take_titles(out, &mut waiting);
        let mut got: Vec<(usize, String)> =
            events.into_iter().map(|e| (e.index, e.title)).collect();
        got.sort();
        assert_eq!(
            got,
            vec![(0, "First".to_string()), (2, "Third".to_string())]
        );
        // Only the unresolved URL is left, which is what the retry pass picks up.
        assert_eq!(waiting["b"].len(), 1);
        assert!(waiting["a"].is_empty() && waiting["c"].is_empty());
    }

    #[test]
    fn empty_batch_leaves_every_url_for_the_retry_pass() {
        // One dead link can make yt-dlp print nothing for the whole batch; the retry
        // pass must then see all of them, not none.
        let mut waiting = waiting_for(&["a", "b", "c"]);
        assert!(take_titles("", &mut waiting).is_empty());
        let leftovers = waiting.values().filter(|q| !q.is_empty()).count();
        assert_eq!(leftovers, 3, "the retry pass must see all three URLs");

        // The individual retry for "b" resolves only "b" and clears it.
        let events = take_titles("b\u{1f}Second\n", &mut waiting);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].index, 1);
        assert!(waiting["b"].is_empty());
    }

    #[test]
    fn duplicate_urls_resolve_one_entry_per_line() {
        let mut waiting = waiting_for(&["a", "a"]);
        assert_eq!(take_titles("a\u{1f}One\n", &mut waiting)[0].index, 0);
        assert_eq!(waiting["a"].len(), 1);
        assert_eq!(take_titles("a\u{1f}One\n", &mut waiting)[0].index, 1);
    }

    #[test]
    fn merged_selector_splits_video_from_audio() {
        // Order is yt-dlp's merge order; swapping it would hand mpv audio as the video track.
        let r = parse_resolved("Never Gonna Give You Up\nhttps://cdn/video\nhttps://cdn/audio\n")
            .unwrap();
        assert_eq!(r.title, "Never Gonna Give You Up");
        assert_eq!(r.video, "https://cdn/video");
        assert_eq!(r.audio.as_deref(), Some("https://cdn/audio"));
    }

    #[test]
    fn muxed_selector_carries_both_tracks_in_one_url() {
        // One URL means no --audio-file; treating it as video-only would play silence.
        let r = parse_resolved("Some Title\nhttps://cdn/muxed\n").unwrap();
        assert_eq!(r.video, "https://cdn/muxed");
        assert_eq!(r.audio, None);
    }

    #[test]
    fn a_title_with_no_stream_url_is_an_error() {
        // Rather than spawning mpv with an empty URL and letting it fail obscurely.
        assert!(parse_resolved("Title but nothing else\n").is_err());
        assert!(parse_resolved("").is_err());
    }

    #[test]
    fn blank_lines_do_not_shift_the_fields() {
        // An empty title still leaves the URLs in their slots.
        let r = parse_resolved("Title\n\nhttps://cdn/v\n\nhttps://cdn/a\n").unwrap();
        assert_eq!(
            (r.video.as_str(), r.audio.as_deref()),
            ("https://cdn/v", Some("https://cdn/a"))
        );
    }

    #[test]
    fn playlists_are_recognised_across_providers() {
        // YouTube lists, SoundCloud sets, and SoundCloud radio (stations / recommended).
        assert!(is_playlist("https://www.youtube.com/watch?v=x&list=RDx"));
        assert!(is_playlist("https://soundcloud.com/artist/sets/album"));
        assert!(is_playlist(
            "https://soundcloud.com/stations/track/artist/song"
        ));
        assert!(is_playlist(
            "https://soundcloud.com/artist/song/recommended"
        ));
        assert!(!is_playlist("https://www.youtube.com/watch?v=x"));
        assert!(!is_playlist("https://soundcloud.com/artist/song"));
    }

    #[test]
    fn slug_titles_read_like_words() {
        assert_eq!(
            slug_title("https://soundcloud.com/monstercat/finding-mero-still-with-me"),
            "finding mero still with me"
        );
        assert_eq!(slug_title("https://x.com/a/b-c?utm=1"), "b c");
    }

    #[test]
    fn search_pairs_split_on_the_unit_separator() {
        assert_eq!(
            parse_search_pair("daft punk get lucky\u{1f}https://youtu.be/x"),
            Some(("daft punk get lucky".into(), "https://youtu.be/x".into()))
        );
        // A line with no separator (a warning, say) is ignored rather than misfiled.
        assert_eq!(parse_search_pair("some stray warning"), None);
        assert_eq!(parse_search_pair("\u{1f}url-without-query"), None);
    }

    #[test]
    fn audio_only_sources_force_mp3_downloads() {
        let mp4_settings = Settings {
            format: SaveFormat::Mp4,
            quality: VideoQuality::P720,
            ..Settings::default()
        };
        // The user prefers MP4, but SoundCloud and Spotify still save audio.
        assert_eq!(
            DownloadSpec::choose(&mp4_settings, SourceKind::SoundCloud),
            DownloadSpec::Audio
        );
        assert_eq!(
            DownloadSpec::choose(&mp4_settings, SourceKind::Spotify),
            DownloadSpec::Audio
        );
        // YouTube honours the format and carries the configured height cap.
        assert_eq!(
            DownloadSpec::choose(&mp4_settings, SourceKind::YouTube),
            DownloadSpec::Video { height: Some(720) }
        );
        // And MP3 as the configured format wins everywhere.
        let mp3_settings = Settings::default();
        assert_eq!(
            DownloadSpec::choose(&mp3_settings, SourceKind::YouTube),
            DownloadSpec::Audio
        );
    }

    #[test]
    fn m3u_and_pls_lines_resolve_against_their_directory() {
        let dir = Path::new("/music");
        let m3u = parse_playlist_text(
            "#EXTM3U\n#EXTINF:123,Some Song\nsong.mp3\n\nhttps://example.com/stream\n",
            false,
            dir,
        );
        assert_eq!(m3u.len(), 2);
        assert_eq!(m3u[0].title, "song");
        assert_eq!(m3u[0].url.as_deref(), Some("/music/song.mp3"));
        assert_eq!(m3u[1].url.as_deref(), Some("https://example.com/stream"));

        let pls = parse_playlist_text(
            "[playlist]\nFile1=a.flac\nTitle1=ignored\nFile2=sub/b.ogg\nNumberOfEntries=2\n",
            true,
            dir,
        );
        assert_eq!(pls.len(), 2);
        assert_eq!(pls[0].url.as_deref(), Some("/music/a.flac"));
        assert_eq!(pls[1].url.as_deref(), Some("/music/sub/b.ogg"));
    }

    #[test]
    fn kind_detection_prefers_local_paths() {
        // `Cargo.toml` exists wherever the tests run.
        assert_eq!(kind_of("Cargo.toml"), SourceKind::Local);
        assert_eq!(
            kind_of("https://open.spotify.com/track/x"),
            SourceKind::Spotify
        );
        assert_eq!(
            kind_of("https://soundcloud.com/artist/track"),
            SourceKind::SoundCloud
        );
        assert_eq!(
            kind_of("https://www.youtube.com/watch?v=x"),
            SourceKind::YouTube
        );
    }
}
