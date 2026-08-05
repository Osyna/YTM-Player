mod mpv;
mod recorder;
mod tty;
mod ui;
mod youtube;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use mpv::{Media, Mpv};
use recorder::Recorder;
use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use youtube::{DownloadControl, DownloadState, Quality, Source};

type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

/// The text UI redraws often; the video bar redraws rarely so it interleaves less with
/// mpv's own frame writes on the shared terminal.
const REDRAW_TEXT: Duration = Duration::from_millis(100);
const REDRAW_VIDEO: Duration = Duration::from_millis(250);
/// Short enough that quitting and redrawing still feel immediate.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// mpv needs a moment to finish releasing the terminal after its `tct` output is destroyed.
const TCT_TEARDOWN: Duration = Duration::from_millis(250);

const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f64 = 5.0;
/// How long the "loading next track" hint stays up after a playlist jump.
const LOADING_HINT: Duration = Duration::from_millis(600);

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn run() -> AppResult<()> {
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "ytmplayer".to_string());
    let Some(url) = std::env::args().nth(1) else {
        display_usage(&program);
        process::exit(1);
    };

    check_dependencies()?;

    // One yt-dlp call returns full-quality audio plus a low-resolution video URL. mpv is handed
    // only the audio, so a session that never presses `v` never demuxes a video track at all.
    let live = youtube::live_format(Quality::default().video_height());
    let (source, playlist_file) = Source::resolve(url, &live)?;
    let _cleanup = TempFile(playlist_file.clone());

    let socket_path = std::env::temp_dir().join(format!("mpvsocket_{}", process::id()));
    let deferred_video = source
        .resolved()
        .and_then(|track| track.deferred_video())
        .map(str::to_string);
    let media = match (source.resolved(), playlist_file.as_deref()) {
        (Some(track), _) => Media::Direct {
            url: track.playback_url(),
            title: &track.title,
        },
        (None, Some(file)) => Media::Playlist {
            file,
            ytdl_format: youtube::AUDIO_ONLY_FORMAT,
        },
        (None, None) => return Err("nothing to play".into()),
    };
    let mut mpv = Mpv::spawn(media, &socket_path)?;

    // From here on exactly one thing writes to this terminal. mpv's `tct` frames come to us on a
    // pipe and are forwarded by the same writer that paints the status bar, so the two can no
    // longer land inside each other.
    let term = tty::Terminal::new();
    if let Some(video) = mpv.video_out.take() {
        let forwarder = term.clone();
        std::thread::spawn(move || forwarder.forward(video));
    }

    let quit = Arc::new(AtomicBool::new(false));
    {
        let quit = quit.clone();
        ctrlc::set_handler(move || quit.store(true, Ordering::SeqCst))?;
    }

    let mut player = Player::new(mpv, source, deferred_video, term.clone());
    // Scoped so the terminal is restored before anything is printed to it.
    let stopped_by_user = {
        let _guard = ui::TerminalGuard::new(term)?;
        player.event_loop(&quit)?
    };
    player.shutdown();

    if stopped_by_user {
        println!("Stopped.");
    } else {
        println!("{}", player.mpv.exit_report());
    }
    Ok(())
}

fn display_usage(program: &str) {
    eprintln!("Usage: {program} <YouTube URL or Playlist URL>");
    eprintln!("Example for single video: {program} https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    eprintln!(
        "Example for playlist: {program} https://www.youtube.com/watch?v=XnG3YWYMY-I&list=RDQMxUfpwjvstDY&start_radio=1"
    );
    eprintln!();
    eprintln!("Controls: [p] play/pause  [h][l] seek 5s  [j][k] volume  [n][b] playlist next/prev");
    eprintln!(
        "          [v] toggle ASCII video  [Tab] cycle download quality  [d] download/cancel  [q] quit"
    );
    eprintln!("          click the bar to seek; click the status line or the video to play/pause");
}

fn check_dependencies() -> AppResult<()> {
    for dep in ["yt-dlp", "mpv"] {
        if !youtube::succeeds(Command::new(dep).arg("--version")) {
            return Err(format!(
                "{dep} is not installed or not on PATH. Please install it and try again."
            )
            .into());
        }
    }
    Ok(())
}

/// Removes a temp file once dropped, even on early return.
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The mpv properties the UI reads, sampled together once per redraw.
struct Snapshot {
    position: Option<f64>,
    duration: Option<f64>,
    title: String,
    paused: bool,
    volume: Option<f64>,
    playlist_pos: i64,
    playlist_count: i64,
}

/// Sampled together in one round trip; the order matches the destructuring in [`Snapshot::read`].
const SNAPSHOT_PROPS: [&str; 7] = [
    "time-pos",
    "duration",
    "media-title",
    "pause",
    "volume",
    "playlist-pos",
    "playlist-count",
];

impl Snapshot {
    fn read(mpv: &mut Mpv) -> Snapshot {
        let [
            position,
            duration,
            title,
            paused,
            volume,
            playlist_pos,
            playlist_count,
        ] = mpv.get_properties(SNAPSHOT_PROPS);
        Snapshot {
            position: position.as_ref().and_then(Value::as_f64),
            duration: duration.as_ref().and_then(Value::as_f64),
            title: mpv::owned_string(title).unwrap_or_default(),
            paused: paused.as_ref().and_then(Value::as_bool).unwrap_or(false),
            volume: volume.as_ref().and_then(Value::as_f64),
            playlist_pos: playlist_pos.as_ref().and_then(Value::as_i64).unwrap_or(0),
            playlist_count: playlist_count.as_ref().and_then(Value::as_i64).unwrap_or(0),
        }
    }
}

/// Whether the event loop carries on after handling an event.
enum Flow {
    Continue,
    Quit,
}

/// Everything the event loop reads and mutates. Keeping it together lets each interaction be
/// its own small method rather than one long match arm.
struct Player {
    /// The one writer for this terminal; mpv's forwarder holds the other end of the same lock.
    term: tty::Terminal,
    mpv: Mpv,
    source: Source,
    snap: Snapshot,
    recorder: Recorder,
    download: DownloadControl,
    quality: Quality,
    video_mode: bool,
    /// The video URL `v` hands to mpv, and the height it was resolved at. Holding it here
    /// instead of passing it at spawn is what keeps a default session audio-only.
    video_url: Option<String>,
    video_height: Option<u16>,
    /// True once mpv owns the added track; from then on `v` is just a `vid` flip, no network.
    video_track_loaded: bool,
    /// Set when yt-dlp had no separate streams to merge, so the playing file already carries
    /// video and `v` never has anything to add.
    video_is_muxed: bool,
    /// mpv's id for the track we added, so a quality change removes exactly that one.
    video_track_id: Option<i64>,
    term_cols: u16,
    term_rows: u16,
    /// A deadline rather than a blocking sleep, so the "loading next track" line actually
    /// renders and the UI stays responsive across a playlist switch.
    loading_until: Option<Instant>,
    next_title: String,
    /// Set by anything that changes what's on screen, so it repaints without waiting out the
    /// redraw interval.
    dirty: bool,
}

impl Player {
    fn new(mut mpv: Mpv, source: Source, video_url: Option<String>, term: tty::Terminal) -> Player {
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let snap = Snapshot::read(&mut mpv);
        // A muxed fallback stream carries its own video, so there is nothing to add later.
        let video_is_muxed = source.resolved().is_some() && video_url.is_none();
        let mut player = Player {
            term,
            mpv,
            source,
            snap,
            recorder: Recorder::default(),
            download: DownloadControl::default(),
            quality: Quality::default(),
            video_mode: false,
            video_url,
            video_height: Quality::default().video_height(),
            video_track_loaded: false,
            video_is_muxed,
            video_track_id: None,
            term_cols,
            term_rows,
            loading_until: None,
            next_title: String::new(),
            dirty: true,
        };
        // The live stream is audio-only, which is exactly what the MP3 default downloads:
        // capture it from track start so `d` is instant instead of re-fetching what played.
        player.attach_recorder();
        player
    }

    /// Runs until the user quits or mpv stops. `true` means the user ended it.
    fn event_loop(&mut self, quit: &AtomicBool) -> AppResult<bool> {
        let mut last_render = Instant::now();

        while !quit.load(Ordering::SeqCst) {
            if self.mpv.has_exited() {
                return Ok(false);
            }

            let interval = if self.video_mode {
                REDRAW_VIDEO
            } else {
                REDRAW_TEXT
            };
            if self.dirty || last_render.elapsed() >= interval {
                self.refresh();
                self.render()?;
                last_render = Instant::now();
                self.dirty = false;
            }

            if !event::poll(POLL_INTERVAL)? {
                continue;
            }
            match event::read()? {
                Event::Resize(cols, rows) => self.on_resize(cols, rows)?,
                Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                    self.on_click(m.column, m.row);
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Flow::Quit = self.on_key(key)? {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// Sample mpv and re-evaluate the recording before drawing.
    fn refresh(&mut self) {
        self.snap = Snapshot::read(&mut self.mpv);

        // A playlist advance invalidates the recording: it belongs to the previous track.
        if self.recorder.belongs_to_other_track(self.snap.playlist_pos) && self.source.is_playlist {
            self.recorder.discard(&mut self.mpv);
            self.attach_recorder();
            // mpv discards an added video track when it loads the next entry, and a track
            // re-added while that load is still in flight is silently orphaned rather than
            // selected. So don't race the load: hand the terminal back and let `v` fetch a
            // picture for the new track if the user still wants one.
            if self.video_mode {
                let _ = self.disable_video();
            }
            self.video_url = None;
            self.video_track_loaded = false;
            // mpv drops added tracks with the old file, so its id means nothing now.
            self.video_track_id = None;
        }
        self.recorder.refresh(&mut self.mpv);
    }

    fn render(&self) -> io::Result<()> {
        let download = self.download.snapshot();
        self.term.paint(
            ui::frame(&ui::FrameData {
                title: &self.snap.title,
                position: self.snap.position,
                duration: self.snap.duration,
                paused: self.snap.paused,
                is_playlist: self.source.is_playlist,
                playlist_pos: self.snap.playlist_pos,
                playlist_count: self.snap.playlist_count,
                is_loading: self.loading_until.is_some_and(|t| Instant::now() < t),
                next_title: &self.next_title,
                download: &download,
                cache: self.recorder.cache(),
                volume: self.snap.volume,
                quality_label: self.quality.label(),
                video_mode: self.video_mode,
                term_cols: self.term_cols,
                term_rows: self.term_rows,
            })
            .as_bytes(),
        )
    }

    fn on_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.term_cols = cols;
        self.term_rows = rows;
        if self.video_mode {
            // A plain property change doesn't re-layout an already-streaming tct output - this
            // forces a reinit, which also drops and re-enters our alt screen and mouse capture,
            // so reassert those right after.
            let _ = self.mpv.resync_tct_geometry(cols, ui::video_rows(rows));
            ui::reassert_terminal(&self.term)?;
        }
        ui::clear_screen(&self.term)?;
        self.dirty = true;
        Ok(())
    }

    fn on_click(&mut self, col: u16, row: u16) {
        match ui::click_action(col, row, self.video_mode, self.term_rows, self.term_cols) {
            ui::ClickAction::Seek(fraction) => {
                if let Some(duration) = self.snap.duration.filter(|d| *d > 0.0) {
                    let _ = self.mpv.seek_absolute(fraction * duration);
                }
            }
            ui::ClickAction::TogglePause => {
                let _ = self.mpv.toggle_pause();
            }
            ui::ClickAction::Ignore => {}
        }
        self.dirty = true;
    }

    fn on_key(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') => return Ok(Flow::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Flow::Quit);
            }
            KeyCode::Char('p') => {
                let _ = self.mpv.toggle_pause();
            }
            KeyCode::Char('h') => {
                let _ = self.mpv.seek(-SEEK_STEP);
            }
            KeyCode::Char('l') => {
                let _ = self.mpv.seek(SEEK_STEP);
            }
            KeyCode::Char('j') => {
                let _ = self.mpv.add_volume(VOLUME_STEP);
            }
            KeyCode::Char('k') => {
                let _ = self.mpv.add_volume(-VOLUME_STEP);
            }
            KeyCode::Tab => {
                self.quality = self.quality.next();
                self.dirty = true;
            }
            KeyCode::Char('n') if self.source.is_playlist => {
                let next = self.snap.playlist_pos + 1;
                self.next_title =
                    mpv::owned_string(self.mpv.get_property(&format!("playlist/{next}/title")))
                        .unwrap_or_default();
                self.begin_track_switch();
                let _ = self.mpv.playlist_next();
            }
            KeyCode::Char('b') if self.source.is_playlist => {
                self.begin_track_switch();
                let _ = self.mpv.playlist_prev();
            }
            KeyCode::Char('v') => self.toggle_video()?,
            KeyCode::Char('d') => self.save_or_cancel(),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn begin_track_switch(&mut self) {
        self.loading_until = Some(Instant::now() + LOADING_HINT);
        self.dirty = true;
    }

    /// Start capturing the playing track, remembering where playback was at the time.
    fn attach_recorder(&mut self) {
        let position = self.snap.position.unwrap_or(0.0);
        self.recorder
            .attach(&mut self.mpv, self.snap.playlist_pos, position);
    }

    fn toggle_video(&mut self) -> io::Result<()> {
        if self.video_mode {
            return self.disable_video();
        }
        self.video_mode = true;
        if !self.enable_video() {
            // Nothing to show - stay in text mode rather than blanking the UI.
            self.video_mode = false;
            self.dirty = true;
            return Ok(());
        }
        ui::clear_screen(&self.term)?;
        self.dirty = true;
        Ok(())
    }

    /// Tear the picture down and take the terminal back. mpv's `tct` output resets alt screen,
    /// cursor and mouse reporting as it goes, so that has to land before we redraw over it.
    fn disable_video(&mut self) -> io::Result<()> {
        self.video_mode = false;
        let _ = self.mpv.set_video_enabled(false);
        std::thread::sleep(TCT_TEARDOWN);
        ui::reassert_terminal(&self.term)?;
        ui::clear_screen(&self.term)?;
        self.dirty = true;
        Ok(())
    }

    /// Bring the picture up: reserve the rows first so mpv's very first frame already respects
    /// them, make sure a track at the selected height exists, then select it. False when there
    /// is nothing to show.
    fn enable_video(&mut self) -> bool {
        let _ = self
            .mpv
            .set_tct_geometry(self.term_cols, ui::video_rows(self.term_rows));
        self.ensure_video_track() && self.mpv.set_video_enabled(true).is_ok()
    }

    /// Make sure mpv owns a video track at the selected height. False when there is no picture
    /// to show, which leaves the session audio-only rather than staring at a blank screen.
    ///
    /// The default tier's URL arrives with startup resolution, so the usual path is pure IPC.
    /// Only a deliberate `Tab` to a higher tier or a playlist advance pays for a yt-dlp call,
    /// and since that blocks we paint the loading hint before going out to the network.
    fn ensure_video_track(&mut self) -> bool {
        if self.video_is_muxed {
            return true;
        }
        let wanted = self.quality.video_height();
        if self.video_track_loaded && self.video_height == wanted {
            return true;
        }
        if self.video_url.is_none() || self.video_height != wanted {
            self.loading_until = Some(Instant::now() + LOADING_HINT);
            let _ = self.render();
            let url = self.source.track_url(self.snap.playlist_pos);
            let Ok(track) = youtube::resolve_track(&url, &youtube::live_format(wanted)) else {
                return false;
            };
            self.video_url = track.deferred_video().map(str::to_string);
            self.video_height = wanted;
        }
        let Some(url) = self.video_url.clone() else {
            return false;
        };
        // Drop the track we added last, by id: it is the previous resolution, and while video
        // is toggled off mpv has nothing "current" for a bare remove to act on.
        if let Some(stale) = self.video_track_id.take() {
            let _ = self.mpv.remove_video_track(stale);
        }
        if self.mpv.add_video_track(&url).is_err() {
            return false;
        }
        self.video_track_id = self.mpv.video_track_id();
        self.video_track_loaded = true;
        true
    }

    /// `d`: cancel a running download, save the cached copy instantly, or start a real one.
    fn save_or_cancel(&mut self) {
        // The recording only ever holds the audio-only live stream, so MP3 is the one tier it
        // can serve; anything with a picture needs a real download. A complete cache stays
        // valid for later even if bypassed now.
        let cache_hit = self.quality == Quality::Mp3 && self.recorder.is_complete();

        if self.download.is_running() {
            self.download.cancel();
        } else if cache_hit {
            if let Some(temp) = self.recorder.take(&mut self.mpv) {
                self.download.set(
                    match youtube::save_recording(
                        &temp,
                        &self.snap.title,
                        self.quality == Quality::Mp3,
                    ) {
                        Ok(path) => DownloadState::Done { path },
                        Err(message) => DownloadState::Failed { message },
                    },
                );
            }
        } else {
            // A partial recording is no use to a full download - drop it and free the disk.
            if self.recorder.is_recording() && !self.recorder.is_complete() {
                self.recorder.discard(&mut self.mpv);
            }
            self.download
                .start(self.source.track_url(self.snap.playlist_pos), self.quality);
        }
        self.dirty = true;
    }

    /// Stop and clean up any recording still in flight.
    fn shutdown(&mut self) {
        if self.recorder.is_recording() {
            self.recorder.discard(&mut self.mpv);
        }
    }
}
