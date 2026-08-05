//! yt-dlp process wrapper: playlist resolution and background downloads.
//! Native JSON parsing replaces the old `jq` pipeline.

use parking_lot::Mutex;
use serde_json::Value;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn is_playlist(url: &str) -> bool {
    url.contains("list=")
}

/// Run yt-dlp and hand back its stdout, turning any failure into a short human message.
/// Every yt-dlp call goes through here so they all fail the same readable way.
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

/// Resolve a playlist URL into individual watch URLs, in playlist order.
pub fn fetch_playlist_urls(url: &str) -> Result<Vec<String>, String> {
    let stdout = run_ytdlp(&["-j", "--flat-playlist", "--no-warnings", url])?;

    let mut urls = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let entry: Value =
            serde_json::from_str(line).map_err(|e| format!("bad playlist entry: {e}"))?;
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            urls.push(format!("https://youtu.be/{id}"));
        }
    }
    Ok(urls)
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

/// Smallest picture worth showing, and the height `v` uses until `Tab` asks for more. Audio
/// always resolves at full quality regardless - this only ever caps the video stream.
pub const DEFAULT_VIDEO_HEIGHT: u16 = 144;

/// Playlist entries are resolved by mpv's own `ytdl_hook`, one per track; audio-only keeps
/// those as cheap as the single-video path, and `v` resolves a picture on demand.
pub const AUDIO_ONLY_FORMAT: &str = "bestaudio/best";

/// Selector pairing full-quality audio with a video stream capped at `height`. Both come back
/// from one call, and mpv is handed only the audio, so the first `v` press costs no network.
pub fn live_format(height: Option<u16>) -> String {
    match height {
        Some(h) => format!("bestvideo[height<={h}]+bestaudio/best[height<={h}]"),
        None => "bestvideo+bestaudio/best".to_string(),
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

/// What we're playing: a single video, or a playlist resolved to its entries.
pub struct Source {
    pub url: String,
    /// Watch URLs in playlist order; empty for a single video.
    entries: Vec<String>,
    pub is_playlist: bool,
    /// Direct URLs for a single video, resolved once up front. `None` for a playlist, where
    /// mpv's `ytdl_hook` still resolves each entry lazily as it advances - one call per track
    /// either way, so there is nothing to collapse there.
    resolved: Option<Resolved>,
}

impl Source {
    /// Resolve the URL before mpv ever spawns: a single video to its stream URLs, a playlist
    /// to its entries. Also returns the temp playlist file mpv should load, if there is one.
    pub fn resolve(url: String, format: &str) -> Result<(Source, Option<PathBuf>), String> {
        if !is_playlist(&url) {
            let resolved =
                resolve_track(&url, format).map_err(|e| format!("Can't play that: {e}"))?;
            let source = Source {
                url,
                entries: Vec::new(),
                is_playlist: false,
                resolved: Some(resolved),
            };
            return Ok((source, None));
        }

        let entries = fetch_playlist_urls(&url).map_err(|e| format!("Can't play that: {e}"))?;
        if entries.is_empty() {
            return Err("This playlist doesn't exist or has no videos.".to_string());
        }
        let file = write_playlist_file(&entries)
            .map_err(|e| format!("can't write the playlist file: {e}"))?;
        Ok((
            Source {
                url,
                entries,
                is_playlist: true,
                resolved: None,
            },
            Some(file),
        ))
    }

    /// The stream URLs we resolved ourselves, when there are any.
    pub fn resolved(&self) -> Option<&Resolved> {
        self.resolved.as_ref()
    }

    /// URL of the track at `pos`, falling back to whatever the user originally passed.
    pub fn track_url(&self, pos: i64) -> String {
        self.entries
            .get(pos.max(0) as usize)
            .cloned()
            .unwrap_or_else(|| self.url.clone())
    }
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

pub fn write_playlist_file(urls: &[String]) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("ytmplayer_playlist_{}.txt", std::process::id()));
    std::fs::write(&path, urls.join("\n"))?;
    Ok(path)
}

/// Preset download quality tiers, cycled with [`Quality::next`]. Default is MP3: the smallest
/// thing `d` can produce, matching a session that plays audio only until asked otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Mp3,
    P480,
    P720,
    P1080,
    Best,
}

impl Quality {
    /// Every tier in `Tab` cycle order - ascending from the MP3 default, so `Tab` always means
    /// "more". Adding a tier is one row here: cycling, the label, the download selector and the
    /// live video cap all read it.
    const TIERS: [(Quality, &'static str, Option<u16>); 5] = [
        (Quality::Mp3, "MP3", None),
        (Quality::P480, "480p", Some(480)),
        (Quality::P720, "720p", Some(720)),
        (Quality::P1080, "1080p", Some(1080)),
        (Quality::Best, "Best", None),
    ];

    fn index(self) -> usize {
        Self::TIERS
            .iter()
            .position(|(q, ..)| *q == self)
            .unwrap_or(0)
    }

    pub fn next(self) -> Quality {
        Self::TIERS[(self.index() + 1) % Self::TIERS.len()].0
    }

    pub fn label(self) -> &'static str {
        Self::TIERS[self.index()].1
    }

    /// Height cap for the video track `v` hands to mpv. The audio-only tier still has to show
    /// *something*, so it falls back to the smallest useful picture rather than no cap at all.
    pub fn video_height(self) -> Option<u16> {
        match self {
            Quality::Mp3 => Some(DEFAULT_VIDEO_HEIGHT),
            _ => Self::TIERS[self.index()].2,
        }
    }

    /// yt-dlp format-selection args for this tier, including the fallbacks for when ffmpeg
    /// is missing: height caps need it to merge separate video+audio streams, MP3 to transcode.
    fn ytdlp_args(self, has_ffmpeg: bool) -> Vec<String> {
        if self == Quality::Mp3 {
            let mut args = vec!["-f".to_string(), "ba/b".to_string()];
            if has_ffmpeg {
                args.extend(["-x", "--audio-format", "mp3"].map(String::from));
            }
            return args;
        }
        let format = match (Self::TIERS[self.index()].2, has_ffmpeg) {
            (Some(h), true) => format!("bv*[height<={h}]+ba/b[height<={h}]"),
            (Some(h), false) => format!("b[height<={h}]/b"),
            (None, true) => "bv*+ba/b".to_string(),
            (None, false) => "b".to_string(),
        };
        vec!["-f".to_string(), format]
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
    /// cache fast path, which finishes synchronously and has no subprocess to track.
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

    /// Download `url` at `quality` into `downloads/` in the background.
    pub fn start(&self, url: String, quality: Quality) {
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
            cmd.args(quality.ytdlp_args(has_ffmpeg()))
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

            let status = child.wait();
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
                    Err(e) => DownloadState::Failed {
                        message: e.to_string(),
                    },
                }
            };
            // Don't let a finished download's message shadow the cache indicator below it
            // forever - show it for a few seconds, then step aside.
            std::thread::sleep(std::time::Duration::from_secs(4));
            *state.lock() = DownloadState::Idle;
        });
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
}
