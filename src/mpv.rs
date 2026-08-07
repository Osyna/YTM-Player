//! Minimal client for mpv's JSON IPC protocol over a Unix domain socket.
//! Replaces the old socat + jq + bc round-trips with native process/socket/JSON handling.

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const STDERR_TAIL_LINES: usize = 20;
/// How many unsolicited events to skip while waiting for replies before giving up.
const MAX_INTERLEAVED_EVENTS: usize = 64;

#[derive(Debug)]
pub enum MpvError {
    Io(std::io::Error),
    Protocol(String),
}

impl From<std::io::Error> for MpvError {
    fn from(e: std::io::Error) -> Self {
        MpvError::Io(e)
    }
}

impl std::fmt::Display for MpvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MpvError::Io(e) => write!(f, "{e}"),
            MpvError::Protocol(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for MpvError {}

/// What mpv should play, and with it whether mpv needs yt-dlp at all.
pub enum Media<'a> {
    /// A stream we already resolved, so mpv runs with `--no-ytdl` and spawns no yt-dlp of its
    /// own. This is the *audio* URL whenever yt-dlp gave us separate tracks - video is added
    /// later, on demand, so an audio-only session never demuxes a picture it won't show.
    /// The title has to be passed explicitly: a raw CDN URL carries no metadata to read it from.
    Direct { url: &'a str, title: &'a str },
    /// A playlist file of watch URLs, resolved entry by entry by mpv's own `ytdl_hook`.
    Playlist {
        file: &'a Path,
        ytdl_format: &'a str,
    },
    /// Nothing yet: mpv idles until the user opens something from inside the UI.
    Idle,
}

pub struct Mpv {
    child: Child,
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    socket_path: PathBuf,
    next_id: u64,
    /// Reused across requests so a redraw's IPC traffic allocates nothing.
    outbox: Vec<u8>,
    inbox: String,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    exit_status: Option<ExitStatus>,
    /// mpv's `tct` frames, waiting to be handed to the terminal's single writer.
    pub video_out: Option<ChildStdout>,
}

impl Mpv {
    /// Spawn mpv headless (audio-only until a video track is explicitly selected),
    /// wire it to a fresh IPC socket, and block briefly until the socket is ready.
    ///
    /// `idle` keeps mpv alive once the playlist runs dry - required while a background
    /// resolver may still append entries, since mpv would otherwise exit in the gap.
    pub fn spawn(media: Media, socket_path: &Path, idle: bool) -> Result<Mpv, MpvError> {
        let _ = std::fs::remove_file(socket_path);

        let mut cmd = Command::new("mpv");
        cmd.arg(format!("--input-ipc-server={}", socket_path.display()))
            // Never let mpv read our stdin; the TUI owns keyboard input.
            .arg("--input-terminal=no")
            // Silence mpv's own status line / log spam on the shared terminal.
            .arg("--really-quiet")
            .arg("--osd-level=0")
            // The on-screen controller is a Lua overlay we never show, and loading it costs
            // ~8MB of interpreter and font machinery. Measured, not guessed.
            .arg("--osc=no")
            // Start audio-only; toggled on the fly via the `vid` property (see set_video_enabled).
            .arg("--vid=no")
            .arg("--vo=tct")
            // Prefetch aggressively so the whole stream lands in cache (and therefore in
            // `--stream-record`) within seconds, which is what makes `d` a cache hit.
            // Spilling to disk keeps memory bounded on long videos.
            .arg("--cache=yes")
            .arg("--cache-on-disk=yes")
            .arg("--demuxer-max-bytes=2GiB")
            .arg("--demuxer-readahead-secs=100000")
            // Default max is 130; raised so [up] can push past 100%.
            .arg("--volume-max=150")
            .stdin(Stdio::null())
            // Piped, not inherited: mpv must not paint the terminal behind our back. Its `tct`
            // frames are forwarded by the one writer that owns the screen (see `tty::Terminal`).
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match media {
            Media::Direct { url, title } => {
                // We already resolved this track, so mpv must not re-extract it.
                cmd.arg("--no-ytdl")
                    .arg(format!("--force-media-title={title}"))
                    .arg(url);
            }
            Media::Playlist { file, ytdl_format } => {
                cmd.arg(format!("--ytdl-format={ytdl_format}"))
                    .arg(format!("--playlist={}", file.display()));
            }
            Media::Idle => {}
        }
        if idle {
            cmd.arg("--idle=yes");
        }

        let mut child = cmd.spawn()?;
        let child_video = child.stdout.take();

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut tail = tail.lock();
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let writer = loop {
            match UnixStream::connect(socket_path) {
                Ok(s) => break s,
                Err(e) => {
                    if let Ok(Some(status)) = child.try_wait() {
                        let reason = Self::format_stderr_tail(&stderr_tail);
                        return Err(MpvError::Protocol(format!(
                            "mpv exited before it was ready (status {status}){reason}"
                        )));
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err(MpvError::Io(e));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };

        let reader_sock = writer.try_clone()?;
        reader_sock
            .set_read_timeout(Some(Duration::from_millis(1500)))
            .ok();

        Ok(Mpv {
            child,
            writer,
            reader: BufReader::new(reader_sock),
            socket_path: socket_path.to_path_buf(),
            next_id: 1,
            outbox: Vec::new(),
            inbox: String::new(),
            stderr_tail,
            exit_status: None,
            video_out: child_video,
        })
    }

    fn format_stderr_tail(tail: &Arc<Mutex<VecDeque<String>>>) -> String {
        let tail = tail.lock();
        if tail.is_empty() {
            String::new()
        } else {
            format!(":\n{}", Vec::from_iter(tail.iter().cloned()).join("\n"))
        }
    }

    /// Non-blocking: true once mpv's process has exited (track/playlist finished, or errored).
    pub fn has_exited(&mut self) -> bool {
        if self.exit_status.is_some() {
            return true;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exit_status = Some(status);
            true
        } else {
            false
        }
    }

    /// Human-readable summary of why mpv stopped, including any captured stderr.
    pub fn exit_report(&self) -> String {
        let status = self
            .exit_status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let tail = Self::format_stderr_tail(&self.stderr_tail);
        format!("mpv stopped ({status}){tail}")
    }

    /// Encode `commands` as one pipelined write, returning the id given to the first.
    ///
    /// mpv reads its socket line by line, so a batch costs a single `write` instead of one per
    /// command - and gives the UI thread one place to block instead of several.
    fn send(&mut self, commands: &[Value]) -> Result<u64, MpvError> {
        let first_id = self.next_id;
        self.next_id += commands.len() as u64;

        self.outbox.clear();
        for (offset, command) in commands.iter().enumerate() {
            let payload = json!({ "command": command, "request_id": first_id + offset as u64 });
            serde_json::to_writer(&mut self.outbox, &payload)
                .map_err(|e| MpvError::Protocol(e.to_string()))?;
            self.outbox.push(b'\n');
        }
        self.writer.write_all(&self.outbox)?;
        Ok(first_id)
    }

    /// The next line from mpv, parsed. `Ok(None)` means it wasn't JSON and should be skipped.
    fn read_json(&mut self) -> Result<Option<Value>, MpvError> {
        self.inbox.clear();
        if self.reader.read_line(&mut self.inbox)? == 0 {
            return Err(MpvError::Protocol("mpv closed the IPC connection".into()));
        }
        Ok(serde_json::from_str(self.inbox.trim_end()).ok())
    }

    fn request(&mut self, command: Value) -> Result<Value, MpvError> {
        let id = self.send(std::slice::from_ref(&command))?;
        // The socket also carries unsolicited events; skip those and wait for our reply.
        for _ in 0..MAX_INTERLEAVED_EVENTS {
            let Some(value) = self.read_json()? else {
                continue;
            };
            if value.get("request_id").and_then(Value::as_u64) == Some(id) {
                return Ok(value);
            }
        }
        Err(MpvError::Protocol(
            "no reply from mpv (too many interleaved events)".into(),
        ))
    }

    pub fn run_command(&mut self, command: Value) -> Result<(), MpvError> {
        self.request(command)?;
        Ok(())
    }

    /// Read several properties in a single round trip.
    ///
    /// A redraw samples the whole UI state at once, so batching collapses one blocking round
    /// trip per property into one for the set. Results line up with `names`; anything that
    /// errored or never answered comes back `None`.
    pub fn get_properties<const N: usize>(&mut self, names: [&str; N]) -> [Option<Value>; N] {
        let mut out = std::array::from_fn(|_| None);
        let commands = names.map(|name| json!(["get_property", name]));
        let Ok(first_id) = self.send(&commands) else {
            return out;
        };

        // Replies are matched by id, not arrival order, since events interleave freely.
        let mut answered = [false; N];
        let mut outstanding = N;
        for _ in 0..N + MAX_INTERLEAVED_EVENTS {
            if outstanding == 0 {
                break;
            }
            let Ok(reply) = self.read_json() else { break };
            let Some(mut value) = reply else { continue };
            let Some(slot) = reply_slot(&value, first_id, N).filter(|&slot| !answered[slot]) else {
                continue;
            };
            answered[slot] = true;
            outstanding -= 1;
            out[slot] = reply_data(&mut value);
        }
        out
    }

    pub fn get_property(&mut self, name: &str) -> Option<Value> {
        let [value] = self.get_properties([name]);
        value
    }

    pub fn set_property(&mut self, name: &str, value: Value) -> Result<(), MpvError> {
        self.run_command(json!(["set_property", name, value]))
    }

    pub fn toggle_pause(&mut self) -> Result<(), MpvError> {
        self.run_command(json!(["cycle", "pause"]))
    }

    pub fn seek(&mut self, seconds: f64) -> Result<(), MpvError> {
        self.run_command(json!(["seek", seconds, "relative"]))
    }

    /// Exact (non-keyframe-rounded) seek: what a bar click should land on.
    pub fn seek_absolute(&mut self, seconds: f64) -> Result<(), MpvError> {
        self.run_command(json!(["seek", seconds, "absolute+exact"]))
    }

    pub fn add_volume(&mut self, delta: f64) -> Result<(), MpvError> {
        self.run_command(json!(["add", "volume", delta]))
    }

    pub fn playlist_next(&mut self) -> Result<(), MpvError> {
        self.run_command(json!(["playlist-next", "force"]))
    }

    pub fn playlist_prev(&mut self) -> Result<(), MpvError> {
        self.run_command(json!(["playlist-prev", "force"]))
    }

    /// Append `url` to the playlist; if nothing is playing (idle after running dry), start it.
    pub fn playlist_append(&mut self, url: &str) -> Result<(), MpvError> {
        self.run_command(json!(["loadfile", url, "append-play"]))
    }

    /// Move playlist entry `from` so it sits at index `to` (mpv semantics: the entry is
    /// inserted *before* the entry currently at `to`).
    pub fn playlist_move(&mut self, from: i64, to: i64) -> Result<(), MpvError> {
        self.run_command(json!(["playlist-move", from, to]))
    }

    /// Force (or clear, with `None`) the displayed title. **Global**: it overrides
    /// `media-title` for every entry until cleared, so anything that grows the playlist
    /// beyond the one directly-loaded stream must clear it first.
    pub fn set_forced_title(&mut self, title: Option<&str>) -> Result<(), MpvError> {
        self.set_property("force-media-title", json!(title.unwrap_or("")))
    }

    /// Replace the whole playlist with `url` and start playing it.
    pub fn load_url(&mut self, url: &str, title: Option<&str>) -> Result<(), MpvError> {
        // The property (not a loadfile option) so it applies however the file loads.
        self.set_forced_title(title)?;
        self.run_command(json!(["loadfile", url, "replace"]))
    }

    /// Replace the whole playlist with the entries of a playlist file.
    pub fn load_playlist_file(&mut self, file: &Path) -> Result<(), MpvError> {
        self.set_forced_title(None)?;
        self.run_command(json!(["loadlist", &file.display().to_string(), "replace"]))
    }

    /// Point yt-dlp resolution (mpv's `ytdl_hook`) at a format for entries mpv resolves
    /// itself. Direct URLs ignore it.
    pub fn set_ytdl_format(&mut self, format: &str) -> Result<(), MpvError> {
        self.set_property("ytdl-format", json!(format))
    }

    /// Enable or disable mpv's own yt-dlp hook for *future* loads. Direct CDN URLs we
    /// resolved ourselves must not be re-extracted; page URLs appended at runtime must be.
    pub fn set_ytdl_enabled(&mut self, enabled: bool) -> Result<(), MpvError> {
        self.set_property("ytdl", json!(enabled))
    }

    /// Absolute volume, 0..=150 (see `--volume-max` in [`Mpv::spawn`]).
    pub fn set_volume(&mut self, volume: f64) -> Result<(), MpvError> {
        self.set_property("volume", json!(volume.clamp(0.0, 150.0).round()))
    }

    /// How many entries mpv's playlist holds right now.
    pub fn playlist_count(&mut self) -> i64 {
        self.get_property("playlist-count")
            .as_ref()
            .and_then(Value::as_i64)
            .unwrap_or(0)
    }

    /// Jump straight to playlist entry `index` and play it.
    pub fn playlist_play_index(&mut self, index: i64) -> Result<(), MpvError> {
        self.run_command(json!(["playlist-play-index", index]))
    }

    /// True when an `--idle=yes` mpv has nothing loaded - i.e. the playlist truly ran out.
    pub fn idle_active(&mut self) -> bool {
        self.get_property("idle-active")
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Whether the current file brought a real video stream along (embedded, not added).
    /// Local audio files have none, and enabling `vid` on them would just blank the screen.
    pub fn has_video_track(&mut self) -> bool {
        self.get_property("track-list")
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|tracks| {
                tracks
                    .iter()
                    .any(|t| t.get("type").and_then(Value::as_str) == Some("video"))
            })
    }

    /// Toggle the mpv-native ASCII/true-color terminal video renderer (`--vo=tct`) on or off
    /// without reloading the stream.
    pub fn set_video_enabled(&mut self, enabled: bool) -> Result<(), MpvError> {
        self.set_property("vid", if enabled { json!("auto") } else { json!("no") })
    }

    /// Install (or clear, with `None`) the audio filter chain. The visualizer tap rides
    /// here: a transparent graph that measures the playing audio and prints levels into
    /// FIFOs (see `crate::viz::Tap`) while the audible path passes through untouched.
    /// Set through IPC rather than the command line so none of the graph's separators
    /// ever meet a shell or mpv's option parser.
    pub fn set_af(&mut self, af: Option<&str>) -> Result<(), MpvError> {
        self.set_property("af", json!(af.unwrap_or("")))
    }

    /// Gapless decode plus opening the next playlist entry before the current one ends.
    /// Together they remove the silent seam at an advance - the gap a crossfade would
    /// otherwise fade into. Follows the crossfade setting.
    pub fn set_seamless(&mut self, on: bool) -> Result<(), MpvError> {
        let flag = if on { json!("yes") } else { json!("no") };
        self.set_property("gapless-audio", flag.clone())?;
        self.set_property("prefetch-playlist", flag)
    }

    /// Hand mpv a video stream to play alongside the audio it is already playing, and select it.
    /// Kept out of the spawn arguments entirely: a track mpv never receives is a track it never
    /// demuxes, which is what makes the default session cheaper than one started with video.
    ///
    /// Selecting here rather than leaving it to `vid=auto` matters when an older track is still
    /// loaded - `auto` would pick the first one, which is the resolution we just replaced.
    pub fn add_video_track(&mut self, url: &str) -> Result<(), MpvError> {
        self.run_command(json!(["video-add", url, "select"]))
    }

    /// The selected video track's id, so the track we added can later be removed by name.
    pub fn video_track_id(&mut self) -> Option<i64> {
        self.get_property("vid").as_ref().and_then(Value::as_i64)
    }

    /// Drop one track by id. A bare `video-remove` only drops the *selected* track, which is
    /// nothing at all while video is toggled off - leaving a stale resolution behind to be
    /// re-selected later.
    pub fn remove_video_track(&mut self, id: i64) -> Result<(), MpvError> {
        self.run_command(json!(["video-remove", id]))
    }

    /// Constrain the `tct` renderer to `cols` x `rows`, leaving the bottom rows for our own
    /// status bar. Only takes effect at VO init - see [`Mpv::resync_tct_geometry`].
    pub fn set_tct_geometry(&mut self, cols: u16, rows: u16) -> Result<(), MpvError> {
        self.set_property("vo-tct-width", json!(cols.max(1)))?;
        self.set_property("vo-tct-height", json!(rows.max(1)))
    }

    /// Apply new terminal dimensions to an *already streaming* ASCII video.
    ///
    /// `vo-tct-width`/`-height` are read once when the `tct` output initializes and are not
    /// re-applied on a plain property change - confirmed empirically (a live `set_property`
    /// leaves already-active output rendering at the old size). Deselecting and reselecting the
    /// video track forces mpv to tear down and reinitialize the renderer, which does pick up
    /// the current property values. This briefly drops and re-enters the alt screen/mouse
    /// capture mpv's `tct` output owns; callers should follow up with `ui::reassert_terminal()`.
    pub fn resync_tct_geometry(&mut self, cols: u16, rows: u16) -> Result<(), MpvError> {
        self.set_tct_geometry(cols, rows)?;
        self.set_video_enabled(false)?;
        std::thread::sleep(Duration::from_millis(150));
        self.set_video_enabled(true)
    }

    /// Point mpv's stream recorder at `path`, or pass `None` to stop.
    ///
    /// Note: clearing this does **not** finalize the Matroska file while mpv keeps running -
    /// mpv holds the trailer open until the process exits, so a reader will see a "premature
    /// EOF" container until then. Callers that need a clean file immediately (see
    /// `youtube::finalize_recording`) remux it with `ffmpeg -c copy` instead of waiting.
    pub fn set_stream_record(&mut self, path: Option<&Path>) -> Result<(), MpvError> {
        let value = path.map(|p| p.display().to_string()).unwrap_or_default();
        self.set_property("stream-record", json!(value))
    }

    /// True once the demuxer has read the stream to EOF, i.e. everything the recorder can
    /// capture has already been fetched.
    pub fn demuxer_at_eof(&mut self) -> bool {
        self.get_property("demuxer-cache-state")
            .and_then(|s| s.get("eof").and_then(Value::as_bool))
            .unwrap_or(false)
    }
}

/// Which slot of a batch starting at `first_id` a reply belongs to, or `None` if it isn't one
/// of ours: mpv interleaves unsolicited events with replies and may answer out of order.
fn reply_slot(reply: &Value, first_id: u64, batch_len: usize) -> Option<usize> {
    let id = reply.get("request_id").and_then(Value::as_u64)?;
    let slot = id.checked_sub(first_id)? as usize;
    (slot < batch_len).then_some(slot)
}

/// The payload of a successful reply, moved out rather than copied. A property that errored
/// yields `None`, which callers read the same as "unavailable".
fn reply_data(reply: &mut Value) -> Option<Value> {
    if reply.get("error").and_then(Value::as_str) != Some("success") {
        return None;
    }
    reply.get_mut("data").map(Value::take)
}

/// Take an owned `String` out of a property value without copying its contents.
pub fn owned_string(value: Option<Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text),
        _ => None,
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_route_by_id_not_arrival_order() {
        // mpv is free to answer a batch out of order; the id, not the position, decides.
        assert_eq!(reply_slot(&json!({"request_id": 12}), 10, 3), Some(2));
        assert_eq!(reply_slot(&json!({"request_id": 10}), 10, 3), Some(0));
    }

    #[test]
    fn foreign_replies_never_land_in_a_slot() {
        // An unsolicited event, a straggler from an earlier batch, and an id past the end.
        // Getting any of these wrong writes one property's value over another's.
        assert_eq!(reply_slot(&json!({"event": "seek"}), 10, 3), None);
        assert_eq!(reply_slot(&json!({"request_id": 9}), 10, 3), None);
        assert_eq!(reply_slot(&json!({"request_id": 13}), 10, 3), None);
    }

    #[test]
    fn errored_properties_read_as_unavailable() {
        let mut ok = json!({"error": "success", "data": 42});
        assert_eq!(reply_data(&mut ok), Some(json!(42)));
        // mpv still sends a `data` field on failure; it must not be mistaken for a value.
        let mut failed = json!({"error": "property unavailable", "data": 99});
        assert_eq!(reply_data(&mut failed), None);
    }
}
