mod effects;
mod mpv;
mod recorder;
mod settings;
mod spotify;
mod tty;
mod ui;
mod visualizer;
mod viz;
mod youtube;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use mpv::{Media, Mpv};
use recorder::Recorder;
use serde_json::Value;
use settings::Settings;
use std::io;
use std::path::PathBuf;
use std::process::{self, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};
use ui::{Action, EntryStatus, Flow, PromptState, View};
use visualizer::Visualizer;
use youtube::{
    DownloadControl, DownloadSpec, DownloadState, Entry, ResolveEvent, Source, SourceKind,
    TitleEvent,
};

type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

/// The text UI redraws often; the video bar redraws rarely so it interleaves less with
/// mpv's own frame writes on the shared terminal.
const REDRAW_TEXT: Duration = Duration::from_millis(100);
/// A live visualizer wants motion; the tap lands ~45 samples a second.
const REDRAW_VIZ: Duration = Duration::from_millis(33);
const REDRAW_VIDEO: Duration = Duration::from_millis(250);
/// Short enough that quitting and redrawing still feel immediate.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// mpv needs a moment to finish releasing the terminal after its `tct` output is destroyed.
const TCT_TEARDOWN: Duration = Duration::from_millis(250);

const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f64 = 5.0;
/// How long the "loading next track" hint stays up after a playlist jump.
const LOADING_HINT: Duration = Duration::from_millis(600);
/// How long a finished download's message stays up before the line steps aside.
const MSG_LINGER: Duration = Duration::from_secs(4);
/// How long a toast (clipboard queue notice and friends) stays up.
const TOAST_LINGER: Duration = Duration::from_secs(3);

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
    let arg = std::env::args().nth(1);
    if matches!(arg.as_deref(), Some("-h" | "--help")) {
        display_usage(&program);
        return Ok(());
    }

    let settings = Settings::load();
    check_dependencies(arg.as_deref().map(youtube::kind_of))?;

    // With a URL argument, resolve before the UI comes up - exactly the old flow. A bare
    // launch skips this: mpv idles and the Open view asks for a link.
    let resolution = match arg {
        Some(url) => Some(Source::resolve(url, &settings).map_err(|e| e.to_string())?),
        None => None,
    };

    let socket_path = std::env::temp_dir().join(format!("mpvsocket_{}", process::id()));
    let (media, deferred_video) = match &resolution {
        Some(res) => {
            let deferred = res
                .source
                .resolved()
                .and_then(|track| track.deferred_video())
                .map(str::to_string);
            let media = match (res.source.resolved(), res.playlist_file.as_deref()) {
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
            (media, deferred)
        }
        None => (Media::Idle, None),
    };
    // Always idle: runtime opens, add-next and the resolver all need mpv to survive an
    // empty playlist. Session end is decided by the player, not by mpv exiting.
    let mut mpv = Mpv::spawn(media, &socket_path, true)?;

    // The visualizer tap rides mpv's own audio chain; a failed tap costs nothing but the
    // scopes. Installed before anything plays so the very first track is measured.
    let tap = viz::Tap::start().ok();
    if let Some(tap) = &tap {
        let _ = mpv.set_af(Some(&tap.graph()));
    }

    // From here on exactly one thing writes to this terminal. mpv's `tct` frames come to us
    // on a pipe and are forwarded by the same writer that paints the status bar.
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

    let mut player = Player::new(mpv, resolution, deferred_video, tap, term.clone(), settings)?;
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
    eprintln!("Usage: {program} [URL | PATH]");
    eprintln!();
    eprintln!("Plays audio from YouTube, SoundCloud, or Spotify (Spotify tracks are matched");
    eprintln!("on YouTube), and local files. Accepts a single track or video, a playlist,");
    eprintln!("album, set, or SoundCloud station, a media file, a folder, or an .m3u/.pls list.");
    eprintln!("Started bare, it opens a URL bar - paste a link there.");
    eprintln!();
    eprintln!("Controls: [Space] play/pause  [h][l] seek 5s  [j][k] volume  [n][b] next/prev");
    eprintln!("          [o] add a link  [p] queue  [s] settings  [v] ASCII video  [c] scopes");
    eprintln!("          [d] download  [q] quit");
    eprintln!("          Every labelled (key) is clickable; the wheel scrolls lists and volume.");
}

/// `Some(kind)` for a URL launch (local files play without yt-dlp), `None` for a bare
/// launch, where only mpv is needed until a link is opened.
fn check_dependencies(kind: Option<SourceKind>) -> AppResult<()> {
    let deps: &[&str] = match kind {
        Some(SourceKind::Local) | None => &["mpv"],
        Some(_) => &["yt-dlp", "mpv"],
    };
    for dep in deps {
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
}

/// Sampled together in one round trip; the order matches the destructuring in [`Snapshot::read`].
const SNAPSHOT_PROPS: [&str; 6] = [
    "time-pos",
    "duration",
    "media-title",
    "pause",
    "volume",
    "playlist-pos",
];

impl Snapshot {
    fn read(mpv: &mut Mpv) -> Snapshot {
        let [position, duration, title, paused, volume, playlist_pos] =
            mpv.get_properties(SNAPSHOT_PROPS);
        Snapshot {
            position: position.as_ref().and_then(Value::as_f64),
            duration: duration.as_ref().and_then(Value::as_f64),
            title: mpv::owned_string(title).unwrap_or_default(),
            paused: paused.as_ref().and_then(Value::as_bool).unwrap_or(false),
            volume: volume.as_ref().and_then(Value::as_f64),
            playlist_pos: playlist_pos.as_ref().and_then(Value::as_i64).unwrap_or(0),
        }
    }
}

/// What a background open produced. `Launch` replaces the session; `Queue` inserts
/// after the current track (or seeds an empty session).
enum OpenOutcome {
    Launch(Result<youtube::Resolution, String>),
    Queue(Result<Vec<Entry>, String>),
}

/// Crossfade envelope state, alive only while a transition is being shaped.
#[derive(Clone, Copy)]
struct Fade {
    /// The user's volume - what j/k set and the rail shows; restored when the fade ends.
    base: f64,
    /// Last volume written, to skip sub-1% IPC writes. `NAN` forces the next write.
    applied: f64,
    /// Past the track boundary, ramping back up.
    fading_in: bool,
}

/// Everything the event loop reads and mutates.
struct Player {
    /// The one writer for this terminal; mpv's forwarder holds the other end of the same lock.
    term: tty::Terminal,
    /// Ratatui terminal for the text-mode views; its writer paints through `term` atomically.
    tui: ui::Tui,
    /// Clickable regions of the last text-mode frame.
    click_map: ui::ClickMap,
    mpv: Mpv,
    source: Source,
    snap: Snapshot,
    recorder: Recorder,
    download: DownloadControl,
    settings: Settings,
    view: View,
    /// Playlist view cursor.
    selected: usize,
    /// Playlist reorder mode.
    edit_mode: bool,
    settings_cursor: usize,
    /// Per-entry download state, driving the playlist view's status column.
    entry_status: Vec<EntryStatus>,
    /// A whole-playlist download is in flight (or waiting on resolution).
    batch_active: bool,
    /// The entry the queue is currently downloading.
    batch_current: Option<usize>,
    /// Streaming resolver for entries that started life unresolved (Spotify smart loading).
    resolver: Option<Receiver<ResolveEvent>>,
    resolver_done: bool,
    /// Entries with a known URL, for the "matching m/n" progress line.
    resolved_count: usize,
    /// Entries whose resolution finished with no match - they will never play.
    missing: Vec<bool>,
    /// mpv playlist order -> entries order, when the two differ (Spotify: unmatched tracks
    /// never reach mpv). `None` means the identity mapping.
    playlist_map: Option<Vec<usize>>,
    /// Background title fetches for entries whose display name is a stand-in.
    titles: Option<Receiver<TitleEvent>>,
    /// The in-flight runtime open, if any. One at a time keeps the state machine simple.
    opens: Option<Receiver<OpenOutcome>>,
    /// URL prompt: the Open view's input, or the inline add-next bar.
    input: Option<PromptState>,
    /// Links copied to the system clipboard while the watcher setting is on.
    clipboard_rx: Receiver<String>,
    /// Live handle the watcher thread checks; follows the setting.
    clipboard_enabled: Arc<AtomicBool>,
    /// Transient notice line and when it expires.
    toast: Option<(String, Instant)>,
    /// Playlist files written by runtime opens; dropped (deleted) when replaced.
    playlist_tmp: Option<TempFile>,
    /// End the session when the playlist runs out. True for URL launches, false once
    /// the user starts opening things interactively.
    auto_exit: bool,
    /// When the current download message expires and the line steps aside.
    msg_deadline: Option<Instant>,
    video_mode: bool,
    /// Live audio measurements for the scopes; `None` when the tap failed to start.
    tap: Option<viz::Tap>,
    /// The visualizer registry; `visualizer` indexes into it.
    vizzers: Vec<Box<dyn Visualizer>>,
    visualizer: Option<usize>,
    /// Which of [`effects::ALL`] are live; composed into `af` alongside the tap.
    effects_on: Vec<bool>,
    /// Effects view cursor.
    effects_cursor: usize,
    /// Crossfade envelope: `Some` while a transition is shaping mpv's volume.
    fade: Option<Fade>,
    /// The video URL `v` hands to mpv, and the height it was resolved at.
    video_url: Option<String>,
    video_height: Option<u16>,
    /// True once mpv owns the added track; from then on `v` is just a `vid` flip, no network.
    video_track_loaded: bool,
    /// Set when yt-dlp had no separate streams to merge (or the file is local), so the playing
    /// file already carries any video and `v` never has anything to add.
    video_is_muxed: bool,
    /// mpv's id for the track we added, so a quality change removes exactly that one.
    video_track_id: Option<i64>,
    term_cols: u16,
    term_rows: u16,
    /// A deadline rather than a blocking sleep, so the "loading next track" line actually
    /// renders and the UI stays responsive across a playlist switch.
    loading_until: Option<Instant>,
    /// Set by anything that changes what's on screen, so it repaints without waiting out the
    /// redraw interval.
    dirty: bool,
}

impl Player {
    fn new(
        mut mpv: Mpv,
        resolution: Option<youtube::Resolution>,
        video_url: Option<String>,
        tap: Option<viz::Tap>,
        term: tty::Terminal,
        settings: Settings,
    ) -> io::Result<Player> {
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let snap = Snapshot::read(&mut mpv);

        let launched_with_url = resolution.is_some();
        let (source, playlist_file, playlist_map, resolver) = match resolution {
            Some(res) => (
                res.source,
                res.playlist_file,
                res.playlist_map,
                res.resolver,
            ),
            None => (Source::none(), None, None, None),
        };
        // A muxed fallback stream (or a local file) carries its own video track, if any.
        let video_is_muxed = source.resolved().is_some() && video_url.is_none();
        let entry_count = source.entries.len();
        let resolved_count = source.entries.iter().filter(|e| e.url.is_some()).count();
        let tui = ui::make_tui(term.clone())?;

        let clipboard_enabled = Arc::new(AtomicBool::new(settings.clipboard_watch));
        let clipboard_rx = spawn_clipboard_watcher(clipboard_enabled.clone());

        let mut player = Player {
            term,
            tui,
            click_map: ui::ClickMap::default(),
            mpv,
            snap,
            recorder: Recorder::default(),
            download: DownloadControl::default(),
            settings,
            view: if launched_with_url {
                View::Main
            } else {
                View::Open
            },
            selected: 0,
            edit_mode: false,
            settings_cursor: 0,
            entry_status: vec![EntryStatus::None; entry_count],
            batch_active: false,
            batch_current: None,
            resolver_done: resolver.is_none(),
            resolver,
            resolved_count,
            missing: vec![false; entry_count],
            playlist_map,
            titles: None,
            opens: None,
            input: (!launched_with_url).then(PromptState::default),
            clipboard_rx,
            clipboard_enabled,
            toast: None,
            playlist_tmp: Some(TempFile(playlist_file)),
            auto_exit: launched_with_url,
            msg_deadline: None,
            video_mode: false,
            tap,
            vizzers: visualizer::all(),
            visualizer: None,
            effects_on: vec![false; effects::ALL.len()],
            effects_cursor: 0,
            fade: None,
            video_url,
            video_height: settings.quality.height(),
            video_track_loaded: false,
            video_is_muxed,
            video_track_id: None,
            term_cols,
            term_rows,
            loading_until: None,
            dirty: true,
            source,
        };
        // The live stream is audio-only, which is exactly what an MP3 download wants:
        // capture it from track start so `d` is instant instead of re-fetching what played.
        player.attach_recorder();
        player.spawn_title_resolver();
        player.sync_seamless();
        Ok(player)
    }

    /// Runs until the user quits or the session ends. `true` means the user ended it.
    fn event_loop(&mut self, quit: &AtomicBool) -> AppResult<bool> {
        let mut last_render = Instant::now();

        while !quit.load(Ordering::SeqCst) {
            if self.mpv.has_exited() {
                return Ok(false);
            }
            self.drain_resolver();
            self.drain_titles();
            self.drain_opens();
            self.drain_clipboard();
            self.pump_batch();

            let interval = if self.video_mode {
                REDRAW_VIDEO
            } else if self.visualizer.is_some() && self.view == View::Main && !self.snap.paused {
                REDRAW_VIZ
            } else {
                REDRAW_TEXT
            };
            if self.dirty || last_render.elapsed() >= interval {
                self.refresh();
                self.render()?;
                last_render = Instant::now();
                self.dirty = false;
                // A URL session ends when the playlist truly runs out. Interactive
                // sessions (bare launch, anything opened from inside) stay for more.
                if self.auto_exit
                    && self.resolver_done
                    && !self.batch_active
                    && self.opens.is_none()
                    && self.input.is_none()
                    && self.mpv.idle_active()
                {
                    return Ok(false);
                }
            }

            if !event::poll(POLL_INTERVAL)? {
                continue;
            }
            match event::read()? {
                Event::Resize(cols, rows) => self.on_resize(cols, rows)?,
                Event::Paste(text) => {
                    if let Some(input) = &mut self.input {
                        input.insert_str(text.trim());
                        self.dirty = true;
                    }
                }
                Event::Mouse(m) => {
                    if let Flow::Quit = self.on_mouse(m)? {
                        break;
                    }
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

    // -- background work -----------------------------------------------------------------

    /// Apply everything the background resolver produced since the last pass: fill in entry
    /// URLs, append them to mpv's playlist (in order), and grow the mpv->entries map.
    fn drain_resolver(&mut self) {
        let mut disconnected = false;
        let mut events = Vec::new();
        if let Some(rx) = &self.resolver {
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if events.is_empty() && !disconnected {
            return;
        }
        for event in events {
            match event {
                ResolveEvent::Resolved { index, url } => {
                    let _ = self.mpv.playlist_append(&url);
                    if let Some(map) = &mut self.playlist_map {
                        map.push(index);
                    }
                    if let Some(entry) = self.source.entries.get_mut(index) {
                        entry.url = Some(url);
                    }
                    self.resolved_count += 1;
                }
                ResolveEvent::Failed { index } => {
                    if let Some(flag) = self.missing.get_mut(index) {
                        *flag = true;
                    }
                }
                ResolveEvent::Done => self.resolver_done = true,
            }
        }
        if disconnected {
            self.resolver = None;
            self.resolver_done = true;
        }
        self.dirty = true;
    }

    /// Real titles landing from the background title resolver. Entries are matched by
    /// URL, not index alone: an add-next insert may have shifted indexes since spawn.
    fn drain_titles(&mut self) {
        let Some(rx) = &self.titles else { return };
        let mut got_any = false;
        loop {
            match rx.try_recv() {
                Ok(ev) => {
                    got_any = true;
                    let hit =
                        match self.source.entries.get(ev.index) {
                            Some(e) if e.url.as_deref() == Some(ev.url.as_str()) => Some(ev.index),
                            _ => self.source.entries.iter().position(|e| {
                                !e.titled && e.url.as_deref() == Some(ev.url.as_str())
                            }),
                        };
                    if let Some(i) = hit
                        && let Some(entry) = self.source.entries.get_mut(i)
                    {
                        entry.title = ev.title;
                        entry.titled = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.titles = None;
                    break;
                }
            }
        }
        if got_any {
            self.dirty = true;
        }
    }

    /// The one in-flight runtime open, when it lands.
    fn drain_opens(&mut self) {
        let Some(rx) = &self.opens else { return };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.opens = None;
                return;
            }
        };
        self.opens = None;
        match outcome {
            OpenOutcome::Launch(Ok(res)) => {
                self.apply_launch(res);
                self.input = None;
            }
            OpenOutcome::Queue(Ok(entries)) => {
                let n = entries.len();
                self.apply_queue(entries);
                self.input = None;
                self.show_toast(if n == 1 {
                    "⚡ queued next".to_string()
                } else {
                    format!("⚡ queued {n} tracks")
                });
            }
            OpenOutcome::Launch(Err(e)) | OpenOutcome::Queue(Err(e)) => match &mut self.input {
                Some(input) => {
                    input.busy = false;
                    input.error = Some(e);
                }
                None => self.show_toast(format!("✗ {e}")),
            },
        }
        self.dirty = true;
    }

    /// Clipboard links stream in from the watcher; each becomes an add-next (or a launch
    /// when nothing is loaded yet). One at a time: while an open is in flight, further
    /// copies are dropped - copying again later re-fires.
    fn drain_clipboard(&mut self) {
        while let Ok(url) = self.clipboard_rx.try_recv() {
            if !self.settings.clipboard_watch || self.opens.is_some() {
                continue;
            }
            self.show_toast("⌕ resolving clipboard link…".to_string());
            self.spawn_open(url);
        }
    }

    /// Drive the whole-playlist download queue: record the finished entry's outcome, then
    /// start the next queued entry that has a URL.
    fn pump_batch(&mut self) {
        if !self.batch_active {
            return;
        }
        if self.download.is_running() {
            if let (Some(current), DownloadState::Running { percent }) =
                (self.batch_current, self.download.snapshot())
            {
                let status = EntryStatus::Downloading(percent);
                if self.entry_status[current] != status {
                    self.entry_status[current] = status;
                    self.dirty = true;
                }
            }
            return;
        }

        if let Some(current) = self.batch_current.take() {
            self.entry_status[current] = match self.download.snapshot() {
                DownloadState::Done { .. } => EntryStatus::Done,
                DownloadState::Failed { .. } => EntryStatus::Failed,
                _ => EntryStatus::None,
            };
            self.download.set(DownloadState::Idle);
            self.dirty = true;
        }

        let next = self.entry_status.iter().enumerate().find_map(|(i, s)| {
            (*s == EntryStatus::Queued && self.source.entries[i].url.is_some()).then_some(i)
        });
        match next {
            Some(index) => {
                let url = self.source.entries[index].url.clone().expect("checked");
                self.entry_status[index] = EntryStatus::Downloading(0.0);
                self.batch_current = Some(index);
                self.download.start(url, self.download_spec());
                self.dirty = true;
            }
            None => {
                let waiting = !self.resolver_done
                    && self.entry_status.iter().enumerate().any(|(i, s)| {
                        *s == EntryStatus::Queued
                            && self.source.entries[i].url.is_none()
                            && !self.missing[i]
                    });
                if !waiting {
                    for status in &mut self.entry_status {
                        if *status == EntryStatus::Queued {
                            *status = EntryStatus::Failed;
                        }
                    }
                    self.batch_active = false;
                    self.dirty = true;
                }
            }
        }
    }

    // -- runtime opens ---------------------------------------------------------------------

    fn open_prompt(&mut self) {
        if self.input.is_none() {
            self.input = Some(PromptState::default());
        }
        self.dirty = true;
    }

    /// Kick a background resolution of `url`. Empty session -> full launch; otherwise the
    /// result inserts right after the current track.
    fn spawn_open(&mut self, url: String) {
        if self.opens.is_some() {
            return;
        }
        let launch = self.source.is_empty();
        let settings = self.settings;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let outcome = if launch {
                OpenOutcome::Launch(Source::resolve(url, &settings))
            } else {
                OpenOutcome::Queue(youtube::resolve_for_queue(&url))
            };
            let _ = tx.send(outcome);
        });
        self.opens = Some(rx);
    }

    fn submit_input(&mut self) {
        let Some(input) = &mut self.input else { return };
        if input.busy {
            return;
        }
        let text = input.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        input.busy = true;
        input.error = None;
        // `~/Music` should work like a shell would treat it.
        let expanded = expand_home(&text);
        self.spawn_open(expanded);
        self.dirty = true;
    }

    /// Replace the whole session with a freshly resolved source.
    fn apply_launch(&mut self, res: youtube::Resolution) {
        // Stop everything owned by the old session.
        if self.recorder.is_recording() {
            self.recorder.discard(&mut self.mpv);
        }
        if self.download.is_running() {
            self.download.cancel();
        }
        self.download.set(DownloadState::Idle);
        self.batch_active = false;
        self.batch_current = None;
        if self.video_mode {
            let _ = self.disable_video();
        }

        // Hand mpv the new media.
        match (res.source.resolved(), res.playlist_file.as_deref()) {
            (Some(track), _) => {
                let _ = self.mpv.set_ytdl_enabled(false);
                let _ = self.mpv.load_url(track.playback_url(), Some(&track.title));
            }
            (None, Some(file)) => {
                let _ = self.mpv.set_ytdl_enabled(true);
                let _ = self.mpv.set_ytdl_format(youtube::AUDIO_ONLY_FORMAT);
                let _ = self.mpv.load_playlist_file(file);
            }
            (None, None) => {}
        }

        // Adopt the new session state.
        self.video_url = res
            .source
            .resolved()
            .and_then(|t| t.deferred_video())
            .map(str::to_string);
        self.video_is_muxed = res.source.resolved().is_some() && self.video_url.is_none();
        self.video_height = self.settings.quality.height();
        self.video_track_loaded = false;
        self.video_track_id = None;
        let entry_count = res.source.entries.len();
        self.entry_status = vec![EntryStatus::None; entry_count];
        self.missing = vec![false; entry_count];
        self.resolved_count = res
            .source
            .entries
            .iter()
            .filter(|e| e.url.is_some())
            .count();
        self.resolver_done = res.resolver.is_none();
        self.resolver = res.resolver;
        self.playlist_map = res.playlist_map;
        self.playlist_tmp = Some(TempFile(res.playlist_file));
        self.source = res.source;
        self.selected = 0;
        self.edit_mode = false;
        self.view = View::Main;
        self.auto_exit = false;
        self.loading_until = Some(Instant::now() + LOADING_HINT);
        self.cancel_fade();
        self.snap = Snapshot::read(&mut self.mpv);
        self.attach_recorder();
        self.spawn_title_resolver();
        self.dirty = true;
    }

    /// Queue freshly resolved entries: right after the current track, or as the whole
    /// session when nothing is loaded yet.
    fn apply_queue(&mut self, entries: Vec<Entry>) {
        if entries.is_empty() {
            return;
        }
        let _ = self.mpv.set_ytdl_enabled(true);
        let _ = self.mpv.set_ytdl_format(youtube::AUDIO_ONLY_FORMAT);
        // A single directly-loaded stream may have a forced title; it is global and
        // would shadow every queued entry's real title (found the hard way in E2E).
        let _ = self.mpv.set_forced_title(None);

        if self.source.is_empty() {
            let first = entries[0].url.clone().unwrap_or_default();
            for entry in &entries {
                if let Some(url) = &entry.url {
                    let _ = self.mpv.playlist_append(url);
                }
            }
            self.source.url = first.clone();
            self.source.kind = youtube::kind_of(&first);
            self.source.is_playlist = entries.len() > 1;
            self.source.entries = entries;
            self.entry_status = vec![EntryStatus::None; self.source.entries.len()];
            self.missing = vec![false; self.source.entries.len()];
            self.resolved_count = self.source.entries.len();
            self.playlist_map = None;
            self.selected = 0;
            self.view = View::Main;
            self.snap = Snapshot::read(&mut self.mpv);
            self.attach_recorder();
        } else {
            self.insert_after_current(entries);
        }
        self.auto_exit = false;
        self.spawn_title_resolver();
        self.dirty = true;
    }

    /// Insert entries so they play right after the current track, keeping the on-screen
    /// order equal to the play order, and every index-keyed side table in step.
    fn insert_after_current(&mut self, new: Vec<Entry>) {
        // A single-track session has no entries; give the playing track one, so the
        // queue view shows it and the arithmetic below has a place to insert after.
        if self.source.entries.is_empty() {
            let title = if self.snap.title.is_empty() {
                self.source.url.clone()
            } else {
                self.snap.title.clone()
            };
            self.source.entries.push(Entry {
                title,
                url: Some(self.source.url.clone()),
                titled: !self.snap.title.is_empty(),
            });
            self.entry_status.push(EntryStatus::None);
            self.missing.push(false);
        }

        let at = self.entry_index() + 1;
        let cur_mpv = self.snap.playlist_pos.max(0);
        let n = new.len();

        // Entry-index fixups first: everything at or past the insertion point shifts.
        if let Some(map) = &mut self.playlist_map {
            for v in map.iter_mut() {
                if *v >= at {
                    *v += n;
                }
            }
        }
        if let Some(current) = &mut self.batch_current
            && *current >= at
        {
            *current += n;
        }
        if self.selected >= at {
            self.selected += n;
        }

        for (k, entry) in new.into_iter().enumerate() {
            let url = entry.url.clone();
            self.source.entries.insert(at + k, entry);
            self.entry_status.insert(at + k, EntryStatus::None);
            self.missing.insert(at + k, false);

            if let Some(url) = url {
                let _ = self.mpv.playlist_append(&url);
                let appended = self.mpv.playlist_count() - 1;
                let target = cur_mpv + 1 + k as i64;
                if appended > target {
                    let _ = self.mpv.playlist_move(appended, target);
                }
                if let Some(map) = &mut self.playlist_map {
                    map.insert((cur_mpv + 1 + k as i64) as usize, at + k);
                }
            }
        }
        self.source.is_playlist = self.source.entries.len() > 1;
    }

    /// Fetch real titles for any entry still showing a stand-in.
    fn spawn_title_resolver(&mut self) {
        let items: Vec<(usize, String)> = self
            .source
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.titled)
            .filter_map(|(i, e)| e.url.clone().map(|u| (i, u)))
            .collect();
        self.titles = (!items.is_empty()).then(|| youtube::spawn_title_resolver(items));
    }

    fn show_toast(&mut self, text: String) {
        self.toast = Some((text, Instant::now() + TOAST_LINGER));
        self.dirty = true;
    }

    // -- sampling and rendering ----------------------------------------------------------

    /// Sample mpv and re-evaluate the recording before drawing.
    fn refresh(&mut self) {
        let prev_pos = self.snap.playlist_pos;
        self.snap = Snapshot::read(&mut self.mpv);
        self.fade_tick(prev_pos);

        // A playlist advance invalidates the recording: it belongs to the previous track.
        if self.recorder.belongs_to_other_track(self.snap.playlist_pos) && self.source.is_playlist {
            self.recorder.discard(&mut self.mpv);
            self.attach_recorder();
            if self.video_mode {
                let _ = self.disable_video();
            }
            self.video_url = None;
            self.video_track_loaded = false;
            self.video_track_id = None;
        }
        self.recorder.refresh(&mut self.mpv);

        if let Some((_, deadline)) = &self.toast
            && Instant::now() >= *deadline
        {
            self.toast = None;
            self.dirty = true;
        }

        // A finished single download lingers a few seconds, then steps aside. Batch
        // outcomes are acknowledged by the queue instead.
        if !self.batch_active {
            let terminal = matches!(
                self.download.snapshot(),
                DownloadState::Done { .. }
                    | DownloadState::Failed { .. }
                    | DownloadState::Cancelled
            );
            match (terminal, self.msg_deadline) {
                (true, None) => self.msg_deadline = Some(Instant::now() + MSG_LINGER),
                (true, Some(deadline)) if Instant::now() >= deadline => {
                    self.download.set(DownloadState::Idle);
                    self.msg_deadline = None;
                    self.dirty = true;
                }
                (false, Some(_)) => self.msg_deadline = None,
                _ => {}
            }
        }
    }

    /// Current track as an index into `source.entries`.
    fn entry_index(&self) -> usize {
        let pos = self.snap.playlist_pos.max(0) as usize;
        match &self.playlist_map {
            Some(map) => map.get(pos).copied().unwrap_or(0),
            None => pos.min(self.source.entries.len().saturating_sub(1)),
        }
    }

    /// mpv playlist index for an entry, when the entry is actually in mpv's playlist.
    fn mpv_index_of(&self, entry: usize) -> Option<i64> {
        match &self.playlist_map {
            Some(map) => map.iter().position(|&e| e == entry).map(|i| i as i64),
            None => (entry < self.source.entries.len()).then_some(entry as i64),
        }
    }

    /// Title of whatever plays after the current track, if anything is known to.
    fn next_entry_title(&self) -> Option<&str> {
        let pos = self.snap.playlist_pos.max(0) as usize;
        let next = match &self.playlist_map {
            Some(map) => map.get(pos + 1).copied().or_else(|| {
                let current = self.entry_index();
                (current + 1 < self.source.entries.len()).then_some(current + 1)
            }),
            None => Some(pos + 1),
        }?;
        self.source.entries.get(next).map(|e| e.title.as_str())
    }

    fn download_enabled(&self) -> bool {
        self.source.kind != SourceKind::Local && !self.source.is_empty()
    }

    fn download_spec(&self) -> DownloadSpec {
        DownloadSpec::choose(&self.settings, self.source.kind)
    }

    /// Quality and format are locked when the source can't use them: audio-only sources
    /// always save MP3, and local files aren't downloaded at all. An empty session locks
    /// nothing - the user is setting up the next open.
    fn settings_locked(&self) -> bool {
        !self.source.is_empty()
            && (self.source.kind.audio_only() || self.source.kind == SourceKind::Local)
    }

    fn render(&mut self) -> io::Result<()> {
        let current = self.entry_index();
        let rows = entry_rows(&self.source, &self.missing, &self.entry_status, current);
        let download = if self.batch_active {
            DownloadState::Idle
        } else {
            self.download.snapshot()
        };
        let next_title = self.next_entry_title().unwrap_or_default().to_string();
        let batch = self.batch_active.then(|| {
            let saved = self
                .entry_status
                .iter()
                .filter(|s| matches!(s, EntryStatus::Done))
                .count();
            let percent = match self.download.snapshot() {
                DownloadState::Running { percent } => Some(percent),
                _ => None,
            };
            ui::BatchState {
                saved,
                total: self.source.entries.len(),
                percent,
            }
        });
        // What a download is pulling right now: the batch's current entry, else the
        // playing track.
        let download_title = self
            .batch_current
            .and_then(|i| self.source.entries.get(i))
            .map(|e| e.title.as_str())
            .or_else(|| (!self.snap.title.is_empty()).then_some(self.snap.title.as_str()));
        let resolving = (self.resolver.is_some() && !self.resolver_done)
            .then_some((self.resolved_count, self.source.entries.len()));
        let is_loading = self.loading_until.is_some_and(|t| Instant::now() < t);
        let download_enabled = self.download_enabled();
        let locked = self.settings_locked();
        let scope_ok = ui::scope_fits(self.term_cols, self.term_rows);
        let visualizer_name = self
            .visualizer
            .filter(|_| scope_ok)
            .map(|i| self.vizzers[i].name());
        let idle =
            self.source.is_empty() || (self.snap.title.is_empty() && self.snap.duration.is_none());
        let source_label = if self.source.is_empty() {
            "—"
        } else {
            self.source.kind.label()
        };
        let toast = self.toast.as_ref().map(|(text, _)| text.as_str());

        let state = ui::UiState {
            view: self.view,
            source_label,
            title: &self.snap.title,
            position: self.snap.position,
            duration: self.snap.duration,
            paused: self.snap.paused,
            idle,
            volume: self.fade.map(|fade| fade.base).or(self.snap.volume),
            is_playlist: self.source.is_playlist,
            entry_index: current,
            next_title: (!next_title.is_empty()).then_some(next_title.as_str()),
            is_loading,
            resolving,
            download: &download,
            download_title,
            cache: self.recorder.cache(),
            batch,
            download_enabled,
            entries: &rows,
            selected: self.selected,
            edit_mode: self.edit_mode,
            settings: &self.settings,
            settings_cursor: self.settings_cursor,
            effects_on: &self.effects_on,
            effects_cursor: self.effects_cursor,
            quality_locked: locked,
            format_locked: locked,
            visualizer: visualizer_name,
            prompt: self.input.as_ref(),
            toast,
            term_cols: self.term_cols,
            term_rows: self.term_rows,
        };

        if self.video_mode {
            let (ansi, map) = ui::video_bottom(&state);
            self.click_map = map;
            return self.term.paint(ansi.as_bytes());
        }
        let wave_len = (self.term_cols as usize) * 2;
        // Below the scope threshold the tap isn't even sampled - no cost, pane off.
        let vsnap = match (&self.tap, self.visualizer) {
            (Some(tap), Some(_)) if scope_ok => tap.snapshot(wave_len),
            _ => viz::VizSnapshot::default(),
        };
        let viz_pair = self
            .visualizer
            .filter(|_| scope_ok)
            .map(|i| (&mut *self.vizzers[i] as &mut dyn Visualizer, &vsnap));
        let map = ui::draw(&mut self.tui, &state, viz_pair)?;
        self.click_map = map;
        Ok(())
    }

    // -- input ---------------------------------------------------------------------------

    fn on_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.term_cols = cols;
        self.term_rows = rows;
        if self.video_mode && !ui::video_fits(cols, rows) {
            // mpv would keep streaming a picture with nowhere to go - switch it off.
            self.disable_video()?;
            self.show_toast("▣ video off: terminal too small".to_string());
        } else if self.video_mode {
            // A plain property change doesn't re-layout an already-streaming tct output - this
            // forces a reinit, which also drops and re-enters our alt screen and mouse capture,
            // so reassert those right after.
            let _ = self.mpv.resync_tct_geometry(cols, ui::video_rows(rows));
            ui::reassert_terminal(&self.term)?;
        }
        ui::clear_screen(&self.term)?;
        let _ = self.tui.clear();
        self.dirty = true;
        Ok(())
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> io::Result<Flow> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // One map for both modes: video mode's is rebuilt by `video_bottom`.
                let action = self.click_map.action_at(mouse.column, mouse.row);
                if let Some(action) = action {
                    return self.dispatch(action);
                }
            }
            MouseEventKind::ScrollUp => self.on_scroll(-1),
            MouseEventKind::ScrollDown => self.on_scroll(1),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// The wheel scrolls the playlist view; everywhere else it turns the volume.
    fn on_scroll(&mut self, delta: i64) {
        if self.view == View::Playlist && !self.video_mode {
            self.move_selection(delta);
        } else {
            let _ = self
                .mpv
                .add_volume(if delta < 0 { VOLUME_STEP } else { -VOLUME_STEP });
        }
        self.dirty = true;
    }

    fn move_selection(&mut self, delta: i64) {
        let len = self.source.entries.len();
        if len == 0 {
            return;
        }
        let selected = (self.selected as i64 + delta).clamp(0, len as i64 - 1);
        self.selected = selected as usize;
        self.dirty = true;
    }

    fn on_key(&mut self, key: KeyEvent) -> io::Result<Flow> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(Flow::Quit);
        }
        // An active prompt captures the keyboard: URLs contain every letter the views
        // would otherwise treat as a hotkey.
        if self.input.is_some() {
            return self.on_key_input(key);
        }
        let flow = match self.view {
            View::Main => self.on_key_main(key)?,
            View::Playlist => self.on_key_playlist(key)?,
            View::Settings => self.on_key_settings(key)?,
            View::Effects => self.on_key_effects(key)?,
            View::Open => Flow::Continue, // Open always has an input
        };
        Ok(flow)
    }

    fn on_key_input(&mut self, key: KeyEvent) -> io::Result<Flow> {
        let Some(input) = &mut self.input else {
            return Ok(Flow::Continue);
        };
        match key.code {
            KeyCode::Enter => self.submit_input(),
            KeyCode::Esc => {
                if self.view == View::Open {
                    // The URL bar is the whole session; backing out of it means leaving.
                    return Ok(Flow::Quit);
                }
                self.input = None;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.insert(c),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            _ => {}
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    fn on_key_main(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') => return Ok(Flow::Quit),
            KeyCode::Char(' ') => {
                let _ = self.mpv.toggle_pause();
                self.dirty = true;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let _ = self.mpv.seek(-SEEK_STEP);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let _ = self.mpv.seek(SEEK_STEP);
            }
            KeyCode::Char('j') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Char('k') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('n') if self.source.is_playlist => self.playlist_next(),
            KeyCode::Char('b') if self.source.is_playlist => self.playlist_prev(),
            KeyCode::Char('v') => self.toggle_video()?,
            KeyCode::Char('c') => self.cycle_visualizer(),
            KeyCode::Char('d') => self.save_or_cancel(),
            KeyCode::Char('o') => self.open_prompt(),
            KeyCode::Char('p') => self.open_view(View::Playlist)?,
            KeyCode::Char('s') => self.open_view(View::Settings)?,
            KeyCode::Char('e') => self.open_view(View::Effects)?,
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn on_key_playlist(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('p') => self.close_view(),
            KeyCode::Char(' ') => {
                let _ = self.mpv.toggle_pause();
            }
            KeyCode::Char('e') => {
                self.edit_mode = !self.edit_mode && self.source.entries.len() > 1;
            }
            KeyCode::Char('J') => self.move_entry(1),
            KeyCode::Char('K') => self.move_entry(-1),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Enter if !self.edit_mode => self.jump_to(self.selected),
            KeyCode::Char('d') if !self.edit_mode => self.toggle_batch(),
            KeyCode::Char('o') => self.open_prompt(),
            _ => {}
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    fn on_key_settings(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('s') => self.close_view(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.settings_cursor = (self.settings_cursor + 1).min(5);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.settings_cursor = self.settings_cursor.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => self.change_setting(true),
            KeyCode::Char('h') | KeyCode::Left => self.change_setting(false),
            _ => {}
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    fn on_key_effects(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('e') => self.close_view(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.effects_cursor = (self.effects_cursor + 1).min(effects::ALL.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.effects_cursor = self.effects_cursor.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Right => {
                self.toggle_effect(self.effects_cursor);
            }
            _ => {}
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    /// One dispatcher for every click, so a button can't do anything a key can't.
    fn dispatch(&mut self, action: Action) -> io::Result<Flow> {
        match action {
            Action::Quit => return Ok(Flow::Quit),
            Action::TogglePause => {
                let _ = self.mpv.toggle_pause();
            }
            Action::SeekTo(fraction) => {
                if let Some(duration) = self.snap.duration.filter(|d| *d > 0.0) {
                    let _ = self.mpv.seek_absolute(fraction * duration);
                }
            }
            Action::SeekBack => {
                let _ = self.mpv.seek(-SEEK_STEP);
            }
            Action::SeekForward => {
                let _ = self.mpv.seek(SEEK_STEP);
            }
            Action::VolumeDown => self.nudge_volume(-VOLUME_STEP),
            Action::VolumeUp => self.nudge_volume(VOLUME_STEP),
            Action::VolumeSet(volume) => self.set_user_volume(volume),
            Action::Next => self.playlist_next(),
            Action::Prev => self.playlist_prev(),
            Action::ToggleVideo => self.toggle_video()?,
            Action::CycleVisualizer => self.cycle_visualizer(),
            Action::Download => self.save_or_cancel(),
            Action::DownloadAll => self.toggle_batch(),
            Action::OpenPlaylist => self.open_view(View::Playlist)?,
            Action::OpenSettings => self.open_view(View::Settings)?,
            Action::OpenEffects => self.open_view(View::Effects)?,
            Action::OpenPrompt => self.open_prompt(),
            Action::Submit => self.submit_input(),
            Action::ToggleEdit => {
                self.edit_mode = !self.edit_mode && self.source.entries.len() > 1;
            }
            Action::MoveUp => self.move_entry(-1),
            Action::MoveDown => self.move_entry(1),
            Action::CloseView => self.close_view(),
            Action::JumpTo(entry) => self.jump_to(entry),
            Action::SettingsRow(row) => {
                if row < 6 {
                    self.settings_cursor = row;
                    self.change_setting(true);
                }
            }
            Action::EffectRow(index) => {
                if index < effects::ALL.len() {
                    self.effects_cursor = index;
                    self.toggle_effect(index);
                }
            }
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    // -- views ---------------------------------------------------------------------------

    /// Open a full-screen view. The views are text-mode; live ASCII video is torn down
    /// first, exactly as `v` off would.
    fn open_view(&mut self, view: View) -> io::Result<()> {
        if view == View::Playlist && !self.source.is_playlist {
            return Ok(());
        }
        if self.video_mode {
            self.disable_video()?;
        }
        if view == View::Playlist {
            self.selected = self.entry_index();
            self.edit_mode = false;
        }
        self.view = view;
        let _ = self.tui.clear();
        self.dirty = true;
        Ok(())
    }

    fn close_view(&mut self) {
        if self.view == View::Settings {
            self.settings.save();
        }
        self.edit_mode = false;
        self.view = View::Main;
        let _ = self.tui.clear();
        self.dirty = true;
    }

    /// Change the selected settings row. Locked rows (audio-only source, local files)
    /// stay put - the menu shows why instead.
    fn change_setting(&mut self, forward: bool) {
        match self.settings_cursor {
            0 if !self.settings_locked() => {
                self.settings.quality = self.settings.quality.cycle(forward);
            }
            1 if !self.settings_locked() => {
                self.settings.format = self.settings.format.cycle();
            }
            2 => self.settings.smart_loading = !self.settings.smart_loading,
            3 => {
                self.settings.clipboard_watch = !self.settings.clipboard_watch;
                self.clipboard_enabled
                    .store(self.settings.clipboard_watch, Ordering::Relaxed);
            }
            4 => {
                self.settings.crossfade = !self.settings.crossfade;
                self.sync_seamless();
                if !self.settings.crossfade {
                    self.cancel_fade();
                }
            }
            5 if self.settings.crossfade => self.settings.cycle_crossfade(forward),
            _ => return,
        }
        self.settings.save();
        self.dirty = true;
    }

    // -- effects -------------------------------------------------------------------------

    fn toggle_effect(&mut self, index: usize) {
        let Some(on) = self.effects_on.get_mut(index) else {
            return;
        };
        *on = !*on;
        self.apply_af();
        self.dirty = true;
    }

    /// The one writer of mpv's `af`: enabled effects first (the scopes measure what the
    /// ears get), then the visualizer tap.
    fn apply_af(&mut self) {
        let effects = effects::chain(&self.effects_on);
        let tap = self.tap.as_ref().map(|tap| tap.graph());
        let af = match (effects, tap) {
            (Some(effects), Some(tap)) => Some(format!("{effects},{tap}")),
            (Some(effects), None) => Some(effects),
            (None, Some(tap)) => Some(tap),
            (None, None) => None,
        };
        let _ = self.mpv.set_af(af.as_deref());
    }

    // -- crossfade -----------------------------------------------------------------------

    /// Follow the crossfade setting with mpv's gapless decode + playlist prefetch, so the
    /// seam the fade covers is actually free of silence.
    fn sync_seamless(&mut self) {
        let _ = self.mpv.set_seamless(self.settings.crossfade);
    }

    /// Whether anything is queued after the current entry - the fade-out gate: the last
    /// track always plays out clean.
    fn has_next(&self) -> bool {
        self.source.is_playlist && self.entry_index() + 1 < self.source.entries.len()
    }

    /// Volume changes route through here so a live crossfade scales the *user's* level
    /// instead of compounding with the fade multiplier.
    fn nudge_volume(&mut self, delta: f64) {
        match &mut self.fade {
            Some(fade) => {
                fade.base = (fade.base + delta).clamp(0.0, 150.0);
                fade.applied = f64::NAN; // force the next tick to rewrite at the new base
            }
            None => {
                let _ = self.mpv.add_volume(delta);
            }
        }
    }

    fn set_user_volume(&mut self, volume: f64) {
        match &mut self.fade {
            Some(fade) => {
                fade.base = volume.clamp(0.0, 150.0);
                fade.applied = f64::NAN;
            }
            None => {
                let _ = self.mpv.set_volume(volume);
            }
        }
    }

    /// Restore the user's level and stop shaping the volume.
    fn cancel_fade(&mut self) {
        if let Some(fade) = self.fade.take() {
            let _ = self.mpv.set_volume(fade.base);
        }
    }

    /// Drive the crossfade envelope, once per snapshot. Fade out approaching an
    /// auto-advance, ramp back in past the boundary; the user's level (`base`) is
    /// restored exactly when the fade ends. Runs on the redraw cadence (100-250 ms),
    /// which at 1%-quantized writes is a smooth 25+ step ramp per side.
    fn fade_tick(&mut self, prev_pos: i64) {
        if !self.settings.crossfade || !self.source.is_playlist {
            self.cancel_fade();
            return;
        }
        let half = f64::from(self.settings.crossfade_secs) / 2.0;
        let out = effects::fade_out(
            self.snap.position,
            self.snap.duration,
            half,
            self.has_next(),
        );
        match self.fade {
            None => {
                // `snap.volume` is unfaded here: the envelope only writes while `fade`
                // is set, and it restores `base` on its way out.
                if let (Some(mult), Some(volume)) = (out, self.snap.volume) {
                    self.fade = Some(Fade {
                        base: volume,
                        applied: f64::NAN,
                        fading_in: false,
                    });
                    self.apply_fade(mult);
                }
            }
            Some(mut fade) => {
                if self.snap.playlist_pos != prev_pos {
                    // Crossed into the next track: ramp back up from wherever the cut
                    // left us.
                    fade.fading_in = true;
                    self.fade = Some(fade);
                }
                if fade.fading_in {
                    if self.snap.position.is_none() && self.snap.duration.is_none() {
                        // Nothing loaded (slow resolve, or the queue ran out): restore
                        // the level and let the next track start clean.
                        self.cancel_fade();
                        return;
                    }
                    let mult = (self.snap.position.unwrap_or(0.0) / half).clamp(0.0, 1.0);
                    self.apply_fade(mult);
                    if mult >= 1.0 {
                        self.cancel_fade();
                    }
                } else {
                    match out {
                        Some(mult) => self.apply_fade(mult),
                        // Seeked back out of the tail window: fade off, level restored.
                        None => self.cancel_fade(),
                    }
                }
            }
        }
    }

    /// Write `base × mult` to mpv, skipping sub-1% repeats so a paused fade costs no IPC.
    fn apply_fade(&mut self, mult: f64) {
        let Some(fade) = &mut self.fade else { return };
        let target = fade.base * mult;
        if fade.applied.is_nan() || (target - fade.applied).abs() >= 1.0 {
            fade.applied = target;
            let _ = self.mpv.set_volume(target);
        }
    }

    // -- playback ------------------------------------------------------------------------

    fn begin_track_switch(&mut self) {
        // A manual jump takes the cut as-is: restore the level, no fade-in from silence.
        self.cancel_fade();
        self.loading_until = Some(Instant::now() + LOADING_HINT);
        self.dirty = true;
    }

    fn playlist_next(&mut self) {
        self.begin_track_switch();
        let _ = self.mpv.playlist_next();
    }

    fn playlist_prev(&mut self) {
        self.begin_track_switch();
        let _ = self.mpv.playlist_prev();
    }

    /// Play playlist entry `entry` (clicked or Enter-ed in the playlist view).
    fn jump_to(&mut self, entry: usize) {
        let Some(mpv_index) = self.mpv_index_of(entry) else {
            return;
        };
        if self
            .source
            .entries
            .get(entry)
            .is_none_or(|e| e.url.is_none())
        {
            return;
        }
        self.selected = entry;
        self.begin_track_switch();
        let _ = self.mpv.playlist_play_index(mpv_index);
    }

    /// Swap the selected entry with its neighbour, in the entries, in mpv, and in every
    /// index-keyed side table. Only when the two are also adjacent in mpv's playlist -
    /// with unresolved rows between them the move has no meaningful mpv counterpart.
    fn move_entry(&mut self, delta: i64) {
        if !self.edit_mode {
            return;
        }
        let i = self.selected;
        let j = i as i64 + delta;
        if j < 0 || j as usize >= self.source.entries.len() {
            return;
        }
        let j = j as usize;
        let (Some(mi), Some(mj)) = (self.mpv_index_of(i), self.mpv_index_of(j)) else {
            return;
        };
        if (mi - mj).abs() != 1 {
            return;
        }

        self.source.entries.swap(i, j);
        self.missing.swap(i, j);
        self.entry_status.swap(i, j);
        if let Some(map) = &mut self.playlist_map {
            // The two entries traded entry indexes; relabel, positions stay put.
            for v in map.iter_mut() {
                if *v == i {
                    *v = j;
                } else if *v == j {
                    *v = i;
                }
            }
        }
        if let Some(current) = &mut self.batch_current {
            if *current == i {
                *current = j;
            } else if *current == j {
                *current = i;
            }
        }
        // mpv: moving down means "insert before the one after the neighbour".
        let _ = self
            .mpv
            .playlist_move(mi, if delta > 0 { mj + 1 } else { mj });
        self.selected = j;
        self.dirty = true;
    }

    /// Start capturing the playing track, remembering where playback was at the time.
    /// Local files are already on disk - nothing to capture.
    fn attach_recorder(&mut self) {
        if self.source.kind == SourceKind::Local || self.source.is_empty() {
            return;
        }
        let position = self.snap.position.unwrap_or(0.0);
        self.recorder
            .attach(&mut self.mpv, self.snap.playlist_pos, position);
    }

    // -- downloads -----------------------------------------------------------------------

    /// `d` in the main view: cancel a running download (or the whole batch), save the cached
    /// copy instantly, or start a real one.
    fn save_or_cancel(&mut self) {
        if !self.download_enabled() {
            return;
        }
        if self.batch_active {
            self.toggle_batch();
            return;
        }
        let spec = self.download_spec();
        let cache_hit = spec == DownloadSpec::Audio && self.recorder.is_complete();

        if self.download.is_running() {
            self.download.cancel();
        } else if cache_hit {
            if let Some(temp) = self.recorder.take(&mut self.mpv) {
                self.download.set(
                    match youtube::save_recording(&temp, &self.snap.title, true) {
                        Ok(path) => DownloadState::Done { path },
                        Err(message) => DownloadState::Failed { message },
                    },
                );
            }
        } else {
            if self.recorder.is_recording() && !self.recorder.is_complete() {
                self.recorder.discard(&mut self.mpv);
            }
            self.download
                .start(self.source.track_url(self.entry_index()), spec);
        }
        self.dirty = true;
    }

    /// `d` in the playlist view: queue every entry for download, or cancel the whole batch.
    fn toggle_batch(&mut self) {
        if !self.download_enabled() || self.source.entries.is_empty() {
            return;
        }
        if self.batch_active {
            self.batch_active = false;
            if self.download.is_running() {
                self.download.cancel();
            }
            if let Some(current) = self.batch_current.take() {
                self.entry_status[current] = EntryStatus::None;
            }
            for status in &mut self.entry_status {
                if *status == EntryStatus::Queued {
                    *status = EntryStatus::None;
                }
            }
            self.download.set(DownloadState::Idle);
        } else {
            if self.download.is_running() {
                self.download.cancel();
            }
            self.download.set(DownloadState::Idle);
            self.msg_deadline = None;
            self.batch_active = true;
            for status in &mut self.entry_status {
                if *status != EntryStatus::Done {
                    *status = EntryStatus::Queued;
                }
            }
        }
        self.dirty = true;
    }

    // -- video (the tct output) and scopes -------------------------------------------------

    fn toggle_video(&mut self) -> io::Result<()> {
        if self.video_mode {
            return self.disable_video();
        }
        if self.source.is_empty() {
            return Ok(());
        }
        if !ui::video_fits(self.term_cols, self.term_rows) {
            self.show_toast("▣ terminal too small for video".to_string());
            return Ok(());
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
        let _ = self.tui.clear();
        self.dirty = true;
        Ok(())
    }

    /// `c`: step through the scopes and back to off. Pure rendering state - the tap is
    /// always measuring, so switching is instant. Turns ASCII video off first: the scopes
    /// live in the text UI.
    fn cycle_visualizer(&mut self) {
        if !ui::scope_fits(self.term_cols, self.term_rows) {
            self.show_toast("∿ terminal too small for scopes".to_string());
            return;
        }
        if self.video_mode {
            let _ = self.disable_video();
        }
        self.visualizer = match self.visualizer {
            None => Some(0),
            Some(i) if i + 1 < self.vizzers.len() => Some(i + 1),
            Some(_) => None,
        };
        self.dirty = true;
    }

    /// Bring the picture up: reserve the rows first so mpv's very first frame already
    /// respects them, make sure a track at the selected height exists, then select it.
    fn enable_video(&mut self) -> bool {
        let _ = self
            .mpv
            .set_tct_geometry(self.term_cols, ui::video_rows(self.term_rows));
        self.ensure_video_track() && self.mpv.set_video_enabled(true).is_ok()
    }

    /// Make sure mpv owns a video track at the selected height. False when there is no
    /// picture to show, which leaves the session audio-only rather than staring at a
    /// blank screen.
    fn ensure_video_track(&mut self) -> bool {
        if self.video_is_muxed {
            return self.source.kind != SourceKind::Local || self.mpv.has_video_track();
        }
        let wanted = self.settings.quality.height();
        if self.video_track_loaded && self.video_height == wanted {
            return true;
        }
        if self.video_url.is_none() || self.video_height != wanted {
            self.loading_until = Some(Instant::now() + LOADING_HINT);
            let _ = self.render();
            let url = self.source.track_url(self.entry_index());
            let Ok(track) = youtube::resolve_track(&url, &youtube::live_format(wanted)) else {
                return false;
            };
            self.video_url = track.deferred_video().map(str::to_string);
            self.video_height = wanted;
        }
        let Some(url) = self.video_url.clone() else {
            return false;
        };
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

    /// Stop and clean up any recording still in flight.
    fn shutdown(&mut self) {
        if self.recorder.is_recording() {
            self.recorder.discard(&mut self.mpv);
        }
    }
}

/// Rows for the playlist view. A free function over the specific fields (rather than a
/// `&self` method) so the caller can hold the result while borrowing `self.tui` mutably.
fn entry_rows<'a>(
    source: &'a Source,
    missing: &'a [bool],
    statuses: &'a [EntryStatus],
    current: usize,
) -> Vec<ui::EntryRow<'a>> {
    source
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| ui::EntryRow {
            title: &entry.title,
            current: i == current,
            resolved: entry.url.is_some(),
            missing: missing.get(i).copied().unwrap_or(false),
            titled: entry.titled,
            status: statuses.get(i).copied().unwrap_or_default(),
        })
        .collect()
}

/// `~` and `~/x` become absolute, everything else passes through.
fn expand_home(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest).display().to_string();
    }
    if text == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return home.display().to_string();
    }
    text.to_string()
}

/// Poll the system clipboard and send anything that looks like a playable link. The
/// watcher always runs; `enabled` gates it live so toggling the setting needs no thread
/// churn. The clipboard's content at startup is treated as already seen - only *new*
/// copies queue.
fn spawn_clipboard_watcher(enabled: Arc<AtomicBool>) -> Receiver<String> {
    const POLL: Duration = Duration::from_millis(1200);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let Some(read) = clipboard_reader() else {
            return; // no clipboard tool on this system; the setting is a quiet no-op
        };
        let mut last = read().unwrap_or_default();
        loop {
            std::thread::sleep(POLL);
            if !enabled.load(Ordering::Relaxed) {
                continue;
            }
            let Some(text) = read() else { continue };
            let text = text.trim().to_string();
            if text == last {
                continue;
            }
            last = text.clone();
            if youtube::is_media_link(&text) && tx.send(text).is_err() {
                return;
            }
        }
    });
    rx
}

/// The first clipboard tool that works here: Wayland, then X11 flavours.
/// Each candidate is probed once with a real read.
fn clipboard_reader() -> Option<impl Fn() -> Option<String>> {
    const CANDIDATES: &[&[&str]] = &[
        &["wl-paste", "-n"],
        &["xclip", "-o", "-selection", "clipboard"],
        &["xsel", "-ob"],
    ];
    let args = CANDIDATES
        .iter()
        .find(|args| read_clipboard(args).is_some())?;
    Some(move || read_clipboard(args))
}

fn read_clipboard(args: &[&str]) -> Option<String> {
    let output = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
