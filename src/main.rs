mod analysis;
mod artwork;
mod audio_tap;
mod effects;
mod equalizer;
mod library;
mod measure;
mod mixer;
mod mpris;
mod mpv;
mod preview;
mod recorder;
mod score;
mod settings;
mod spotify;
mod store;
mod transitions;
mod tty;
mod ui;
mod visualizer;
mod youtube;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use measure::{ProbeResult, TrackFacts};
use mixer::Crossfade;
use mpv::{Media, Mpv};
use recorder::Recorder;
use serde_json::Value;
use settings::Settings;
use std::io;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};
use ui::{Action, EntryStatus, Flow, Pane, PromptState, View};
use visualizer::Visualizer;
use youtube::{
    DownloadControl, DownloadSpec, DownloadState, Entry, ResolveEvent, Source, SourceKind,
    TitleEvent,
};

type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

/// How often the event loop wakes when nothing is happening.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Redraw intervals: video needs frames, the visualiser needs its own rate, and text can
/// wait. Drawing everything at the fastest of the three is how a terminal player ends up
/// costing more than the decoder it is driving.
const REDRAW_VIDEO: Duration = Duration::from_millis(40);
const REDRAW_VIZ: Duration = Duration::from_millis(50);
const REDRAW_TEXT: Duration = Duration::from_millis(200);

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

/// How often the resume point is written out. Often enough that a crash costs a few
/// seconds of a track, rarely enough that it is not writing to disk every frame.
const RESUME_INTERVAL: Duration = Duration::from_secs(5);

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

/// Everything the command line can say. Flags are matched exhaustively rather than
/// prefix-guessed, so an unrecognised one is an error instead of a silently ignored typo.
struct Args {
    /// A link, a path, or free text to search for. Words are rejoined, so
    /// `ytmplayer daft punk` searches rather than needing quotes.
    target: Option<String>,
    shuffle: bool,
    volume: Option<f64>,
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut args = Args {
        target: None,
        shuffle: false,
        volume: None,
    };
    let mut words: Vec<String> = Vec::new();
    let mut rest = argv.into_iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--version" | "-V" => {
                println!("ytmplayer {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--shuffle" => args.shuffle = true,
            "--volume" => {
                let value = rest.next().ok_or("--volume needs a number, 0-150")?;
                let level: f64 = value
                    .parse()
                    .map_err(|_| format!("--volume wants a number, not {value:?}"))?;
                args.volume = Some(level.clamp(0.0, 150.0));
            }
            // A lone `--` ends flag parsing, so a file really called `--shuffle` can play.
            "--" => {
                words.extend(rest);
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option {other}"));
            }
            other => words.push(other.to_string()),
        }
    }
    if !words.is_empty() {
        // One word that is a path or a link stays itself; several are a search phrase.
        let joined = words.join(" ");
        args.target = Some(youtube::as_target(&expand_home(&joined)));
    }
    Ok(Some(args))
}

fn run() -> AppResult<()> {
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "ytmplayer".to_string());
    let Some(args) = parse_args(std::env::args().skip(1).collect()).inspect_err(|_| {
        display_usage(&program);
    })?
    else {
        display_usage(&program);
        return Ok(());
    };
    let arg = args.target;

    // A full-screen interface needs a screen. Piped or redirected, the first thing to
    // fail is raw mode, which reports `os error 6` - true, and no help at all to someone
    // wondering why nothing appeared.
    if !std::io::stdout().is_terminal() {
        return Err(
            "ytmplayer draws a full-screen interface and needs a terminal; \
             stdout here is a pipe or a file."
                .into(),
        );
    }

    // Transitions are files. Read them before the settings, because a settings file names
    // one by key and the registry has to exist for that name to mean anything.
    let loaded = transitions::load(settings::transitions_dir().as_deref());
    for problem in &loaded.problems {
        // Printed here *and* said in the UI: this scrolls past a second later when the
        // alternate screen opens, and a transition file that will not read is exactly the
        // thing whose author needs telling.
        eprintln!("transition: {problem}");
    }

    let mut settings = Settings::load();
    if args.shuffle {
        settings.shuffle = true;
    }
    check_dependencies(arg.as_deref().map(youtube::kind_of))?;

    // With a URL argument, resolve before the UI comes up - exactly the old flow. A bare
    // launch skips this: mpv idles and the Open view asks for a link.
    let target = arg.clone().unwrap_or_default();
    let resolution = match arg {
        Some(url) => {
            // Resolution happens before the UI exists - it is what decides what the UI
            // will be showing - so until it lands the terminal has nothing on it at all.
            // For a local file that is a few milliseconds and this scrolls past unread;
            // for a sixty-track set behind a slow yt-dlp it is the difference between
            // "starting" and "broken".
            eprintln!("{}", resolving_notice(&url));
            Some(Source::resolve(url, &settings).map_err(|e| e.to_string())?)
        }
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

    // Yesterday's abandoned downloads are gigabytes nobody will ever resume.
    youtube::sweep_stale_parts();

    // The visualizer tap rides mpv's own audio chain; a failed tap costs nothing but the
    // scopes. Installed before anything plays so the very first track is measured.
    let tap = audio_tap::Tap::start().ok();
    if let Some(tap) = &tap {
        let _ = mpv.set_af(Some(&tap.graph()));
    }

    // From here on exactly one thing writes to this terminal. mpv's `tct` frames come to us
    // on a pipe and are forwarded by the same writer that paints the status bar.
    let term = tty::Terminal::new();
    // Installed before raw mode is ever enabled, so no panic path can leave the terminal
    // in the alternate screen with echo off.
    ui::install_panic_hook(term.clone());
    if let Some(video) = mpv.video_out.take() {
        let forwarder = term.clone();
        std::thread::spawn(move || forwarder.forward(video));
    }

    let quit = Arc::new(AtomicBool::new(false));
    {
        let quit = quit.clone();
        ctrlc::set_handler(move || quit.store(true, Ordering::SeqCst))?;
    }

    let mut player = Player::new(
        mpv,
        term.clone(),
        settings,
        Startup {
            resolution,
            video_url: deferred_video,
            tap,
            target,
            complaint: loaded
                .problems
                .first()
                // The line number is the useful half and the path is long, so lead with
                // the fact that something of theirs did not read.
                .map(|problem| format!("⚠ transition file: {problem}")),
        },
    )?;
    if let Some(level) = args.volume {
        player.set_user_volume(level);
    }
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

// ---------------------------------------------------------------------------
// Playlist index arithmetic
//
// mpv's playlist and ours are not always the same list: a Spotify track with no
// YouTube match never reaches mpv, so positions drift apart and `playlist_map` maps
// mpv's index onto ours (`None` when the two are identical). These are pure lookups,
// split out from `Player` so the arithmetic is testable without mpv or a terminal.
// ---------------------------------------------------------------------------

/// Our entry index for mpv playlist position `pos`.
fn entry_at(map: Option<&[usize]>, pos: usize, len: usize) -> usize {
    match map {
        Some(map) => map.get(pos).copied().unwrap_or(0),
        None => pos.min(len.saturating_sub(1)),
    }
}

/// mpv's playlist index for our entry `entry`, or `None` when mpv never got it.
fn mpv_index(map: Option<&[usize]>, entry: usize, len: usize) -> Option<i64> {
    match map {
        Some(map) => map.iter().position(|&e| e == entry).map(|i| i as i64),
        None => (entry < len).then_some(entry as i64),
    }
}

/// The entry that plays after mpv position `pos`.
///
/// Normally the next thing in mpv's own playlist, which skips the entries that never
/// resolved. Past the end of the map we fall back to the next entry in *our* list: the
/// resolver may still be appending, and naming the track the user can see beats naming
/// nothing.
fn next_entry(map: Option<&[usize]>, pos: usize, len: usize) -> Option<usize> {
    match map {
        Some(map) => map.get(pos + 1).copied().or_else(|| {
            let current = entry_at(Some(map), pos, len);
            (current + 1 < len).then_some(current + 1)
        }),
        None => (pos + 1 < len).then_some(pos + 1),
    }
}

/// The one line printed while a launch argument is being resolved.
///
/// Names what is actually happening, because the three cases take wildly different
/// times and a user who knows which one they are in will wait for it.
fn resolving_notice(target: &str) -> String {
    if let Some(query) = target.strip_prefix("ytsearch") {
        let query = query.split_once(':').map(|(_, q)| q).unwrap_or(query);
        return format!("Searching YouTube for {query}…");
    }
    if youtube::kind_of(target) == SourceKind::Local {
        return format!("Opening {target}…");
    }
    if youtube::is_playlist(target) {
        return "Resolving the playlist… (this one is yt-dlp's to answer)".to_string();
    }
    format!("Resolving {target}…")
}

fn display_usage(program: &str) {
    eprintln!("Usage: {program} [OPTIONS] [URL | PATH | SEARCH…]");
    eprintln!();
    eprintln!("Plays audio from YouTube, SoundCloud, or Spotify (Spotify tracks are matched");
    eprintln!("on YouTube), and local files. Accepts a single track or video, a playlist,");
    eprintln!("album, set, or SoundCloud station, a media file, a folder, or an .m3u/.pls list.");
    eprintln!("Anything that is neither a link nor a path is searched for on YouTube, and the");
    eprintln!("hits land in the queue. Started bare, it opens a URL bar - paste or type there.");
    eprintln!();
    eprintln!("Options: --shuffle          start with the queue in a random order");
    eprintln!("         --volume <0-150>   starting volume");
    eprintln!("         --version, --help");
    eprintln!();
    eprintln!("  {program} \"https://youtu.be/…\"      {program} ~/Music      {program} daft punk");
    eprintln!();
    eprintln!("Controls: [Space] play/pause  [h][l] seek 5s  [j][k] volume  [n][b] next/prev");
    eprintln!("          [v] view: queue, scope or ASCII video in the centre panel");
    eprintln!("          [p] queue  [o] add a link  [s] settings  [e] effects");
    eprintln!("          [d] download  [q] quit");
    eprintln!("          Queue panel: [↑][↓] select  [Enter] play  [E] reorder  [a] save all");
    eprintln!("          Every labelled (key) is clickable; clicking the panel changes it.");
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
    /// Real tags, when the file or stream carries them. mpv reads the container, so this
    /// works for a local FLAC and for a YouTube upload that bothered to fill them in;
    /// what it cannot invent stays empty rather than being guessed at.
    artist: String,
    album: String,
    paused: bool,
    volume: Option<f64>,
    playlist_pos: i64,
}

/// Sampled together in one round trip; the order matches the destructuring in [`Snapshot::read`].
const SNAPSHOT_PROPS: [&str; 8] = [
    "time-pos",
    "duration",
    "media-title",
    "metadata/by-key/artist",
    "metadata/by-key/album",
    "pause",
    "volume",
    "playlist-pos",
];

impl Snapshot {
    fn read(mpv: &mut Mpv) -> Snapshot {
        let [
            position,
            duration,
            title,
            artist,
            album,
            paused,
            volume,
            playlist_pos,
        ] = mpv.get_properties(SNAPSHOT_PROPS);
        Snapshot {
            position: position.as_ref().and_then(Value::as_f64),
            duration: duration.as_ref().and_then(Value::as_f64),
            title: mpv::owned_string(title).unwrap_or_default(),
            artist: mpv::owned_string(artist).unwrap_or_default(),
            album: mpv::owned_string(album).unwrap_or_default(),
            paused: paused.as_ref().and_then(Value::as_bool).unwrap_or(false),
            volume: volume.as_ref().and_then(Value::as_f64),
            playlist_pos: playlist_pos.as_ref().and_then(Value::as_i64).unwrap_or(0),
        }
    }
}

/// Everything the session was started with, bundled because a constructor with eight
/// positional arguments is a constructor whose call site nobody can read.
struct Startup {
    resolution: Option<youtube::Resolution>,
    /// The video stream URL resolved alongside the audio, when there was one.
    video_url: Option<String>,
    tap: Option<audio_tap::Tap>,
    /// What to reopen to get back here, for the resume point.
    target: String,
    /// Something that went wrong before the UI existed and needs saying once it does.
    complaint: Option<String>,
}

/// Where something picked out of the library goes.
///
/// Two intentions, not one: Enter means "this, now", `+` means "and this later" - the
/// difference between picking a track and building an evening.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Queueing {
    PlayNext,
    Append,
}

/// What the shared input bar is currently asking for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum PromptWants {
    /// A link, a path, or something to search for.
    #[default]
    Media,
    /// A folder to add to the library index.
    LibraryRoot,
}

/// What a background open produced. `Launch` replaces the session; `Queue` inserts
/// after the current track (or seeds an empty session).
enum OpenOutcome {
    Launch(Result<youtube::Resolution, String>),
    Queue(Result<Vec<Entry>, String>),
}

/// Everything the event loop reads and mutates.
struct Player {
    /// The one writer for this terminal; mpv's forwarder holds the other end of the same lock.
    term: tty::Terminal,
    /// The second decoder a real crossfade needs. mpv plays one track at a time, so an
    /// overlap takes two of them: this one is parked (stopped, silent, holding the same
    /// playlist) until a transition cues it, then it takes over as `mpv` and the old
    /// deck parks here in its place. `None` when crossfade is off - the process is not
    /// spawned at all until the setting asks for it.
    spare: Option<Mpv>,
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
    /// What the main view's centre panel is showing.
    pane: Pane,
    /// The view chooser's cursor, an index into [`Pane::ALL`].
    menu_cursor: usize,
    /// Queue cursor.
    selected: usize,
    /// Queue reorder mode.
    edit_mode: bool,
    settings_cursor: usize,
    /// Per-entry download state, driving the playlist view's status column.
    entry_status: Vec<EntryStatus>,
    /// A whole-playlist download is in flight (or waiting on resolution).
    batch_active: bool,
    /// The entry the queue is currently downloading.
    batch_current: Option<usize>,
    /// The URL a lone (non-batch) download is for, so the outcome can be recorded
    /// against the right track even if the selection has since moved on.
    download_target: Option<String>,
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
    tap: Option<audio_tap::Tap>,
    /// The visualizer registry; `scope` indexes into it. There is no "off" any more -
    /// not showing a visualizer is [`Pane::Queue`] or [`Pane::Video`], not a fourth
    /// state of this.
    vizzers: Vec<Box<dyn Visualizer>>,
    scope: usize,
    /// Which of [`effects::ALL`] are live; composed into `af` alongside the tap.
    effects_on: Vec<bool>,
    /// Effects view cursor.
    effects_cursor: usize,
    /// The equalizer view's cursor. Separate from `settings.equalizer`, the preset
    /// actually in use: moving reads a shape, Enter commits to it.
    equalizer_cursor: usize,
    /// Where the beat is, for the automix. Fed from the same tap the scopes read, so a
    /// beat-aligned transition costs no extra measurement - only the redraw rate needed
    /// to sample it finely enough, which the run-up to a transition raises on purpose.
    beats: analysis::BeatTracker,
    /// `Some` while two decks are audible at once.
    cross: Option<Crossfade>,
    /// The speed the arriving deck was set to so its tempo matched the leaving one, and
    /// the speed last written to each deck. `1.0` is the track's own tempo.
    tempo_ratio: f64,
    tempo_applied: (f64, f64),
    /// What is known so far about entries that have been probed or read ahead - tempo,
    /// downbeat, decoded frames, resolved stream URL. See `TrackFacts`.
    facts: Vec<(usize, TrackFacts)>,
    /// Tempo and downbeat measured in this session or a previous one, so a track played
    /// before is ready to mix at t=0 instead of waiting on a fresh probe.
    analysis: store::AnalysisCache,
    /// When the arriving track's measurement was started, so the fact that it happened is
    /// shown for long enough to be read.
    analysis_since: Option<Instant>,
    /// How long the arriving deck takes to actually start, once told to.
    ///
    /// Telling a paused deck to seek and then to play is not free: the seek has to decode
    /// to land on the sample asked for, and the pause being let go has its own cost before
    /// a sample reaches the card. Whatever that comes to, the arriving track starts that
    /// much later than the moment its position was chosen for - so it starts that much
    /// behind, and the correction loop spends its first seconds pulling back a lag it
    /// could have been told about instead.
    ///
    /// It cannot be known in advance, because it belongs to the machine rather than to the
    /// music. It can be *measured*, though: the first phase error after a release is very
    /// nearly it. So each transition leaves the next one a better estimate, and by the
    /// second or third the arriving track lands in time rather than arriving at it.
    release_lag: f64,
    /// Whether this transition has yet contributed its measurement of the above.
    lag_learned: bool,
    /// The picture for what is playing, prepared for the box the layout gives it.
    ///
    /// Preparing one is an ffmpeg run, so it happens on its own thread and is kept until
    /// the track changes. `None` covers both "there is no picture" and "not looked yet",
    /// which the interface treats the same way: no box, and the panel keeps its full width.
    cover: Option<artwork::Art>,
    /// What `cover` was loaded for, so a track change is noticed and a repeat is not.
    cover_for: String,
    /// A cover being prepared, on its own thread.
    cover_loading: Option<Receiver<Option<artwork::Art>>>,
    /// Where the picture was last drawn, so it is only re-emitted when it has to be.
    cover_drawn: Option<(u16, u16)>,
    /// A picture is on the screen that should not be, and the frame has to be redrawn in
    /// full to get rid of it.
    ///
    /// Nothing else will. A picture is painted over cells the frame left blank, so as far
    /// as the frame is concerned those cells already hold what they should and there is
    /// nothing to redraw - which is exactly what keeps the picture on screen between
    /// frames, and exactly what strands it there when the track changes to one without.
    cover_needs_clear: bool,
    /// Effects this transition switched on, by index into [`effects::ALL`].
    ///
    /// A score is required to switch off whatever it switches on, and that is checked when
    /// the file is read - but the check only covers scores that run to the end. A seek out
    /// of the tail abandons a transition wherever it happens to be, which may be after the
    /// hook that turned something on and before the one that would have turned it off, and
    /// an effect left on is left on for the rest of the night.
    effects_engaged: Vec<usize>,
    /// Where the arriving deck was parked at the cue, so its grid can be predicted.
    sync_cued: Option<f64>,
    /// Whether the playing track's grid has been taken again close to the start.
    grid_refreshed: bool,
    /// Where the playing track was when its grid was last read, so staleness is visible.
    grid_taken: Option<f64>,
    /// The running speed correction holding the two grids together. See `hold_the_sync`.
    sync_trim: f64,
    /// This tick's correction, measured once and used by whichever half of the ramp runs.
    pending_trim: f64,
    /// Measurements in flight, on their own threads.
    probing: Vec<(usize, Receiver<ProbeResult>)>,
    /// The transition filter last sent to each deck, so a chain that has not changed is
    /// not re-sent. mpv accepts a filter string it cannot parse without complaint, so
    /// rebuilding the graph needlessly is not harmless - it is a chance to break it.
    cross_af: (Option<String>, Option<String>),
    /// A bar line has already been waited for in this transition, so the wait is not
    /// announced again on every tick.
    beat_waited: bool,
    /// Which of the running transition's hooks have already fired, one bit each. Reset per
    /// transition, so a score that repeats a move across two tracks fires it twice and a
    /// score that is re-entered after an abort does not fire the first half again.
    hooks_fired: u64,
    /// A crossfade already refused to start for this track (no spare deck, or the
    /// incoming entry has no URL); remembered so the attempt is not retried every tick.
    cross_refused: bool,
    /// Everything on disk, indexed. Loaded from the cached index at startup; a scan is
    /// only ever started by the user opening the pane.
    library: library::Library,
    /// A scan in flight.
    scan: Option<library::ScanHandle>,
    /// Library pane cursor and its live filter. `Some("")` is an empty filter being
    /// typed into, which is not the same as no filter at all.
    browse_selected: usize,
    browse_filter: Option<String>,
    /// What the input bar is asking for. One bar, two questions - a URL to play, or a
    /// folder to index - because a second bar would be a second set of edit keys.
    prompt_wants: PromptWants,
    /// Everything that has actually been listened to, kept across sessions.
    history: store::History,
    /// Where a track already saved to disk actually is, so mpv and the analysis probe
    /// both reach for the file instead of the network once there is one to reach for.
    downloads: store::DownloadIndex,
    /// The track already written to the history, so a re-render does not write it again.
    logged: Option<String>,
    /// What this session was opened with, so it can be offered back next time.
    target: String,
    /// When the resume point was last written. It is rewritten as playback moves, but a
    /// disk write per frame would be absurd.
    resume_written: Instant,
    /// The previous session's stopping point, offered on the Open view until something
    /// else is opened. Dropped once used, so it cannot be resumed twice.
    resume: Option<store::Resume>,
    /// `(entry, seconds)` a resume is waiting to apply. Held until mpv actually has the
    /// track open: seeking a file that has not loaded yet does nothing at all.
    seek_on_open: Option<(usize, f64)>,
    /// A queued open should be jumped to as soon as it lands - what picking a track out
    /// of the library means, as opposed to adding it to the end of the evening.
    pending_jump: bool,
    /// The desktop's view of the player: media keys, the shell's now-playing popup,
    /// `playerctl`. `None` when there is no session bus, which is most of the time on a
    /// server and never a reason to fail.
    mpris: Option<mpris::Mpris>,
    /// Bumped on every track change so MPRIS clients see a new `mpris:trackid` and
    /// redraw rather than assuming the metadata is a correction to the same track.
    ///
    /// Deliberately not derived from the history marker: that one is only set once a
    /// track has played for twenty seconds, so it would report a new track on every
    /// frame until then.
    track_seq: u64,
    /// The title `track_seq` was last bumped for.
    published_title: String,
    /// The video URL `v` hands to mpv, and the height it was resolved at.
    video_url: Option<String>,
    video_height: Option<u16>,
    /// True once mpv owns the added track; from then on `v` is just a `vid` flip, no network.
    video_track_loaded: bool,
    /// Set when yt-dlp had no separate streams to merge (or the file is local), so the playing
    /// file already carries any video and the video pane never has anything to add.
    video_is_muxed: bool,
    /// This track has already been asked for a picture and had none. Remembered so the
    /// panel stops offering the video pane for it - resolving a miss costs a yt-dlp run,
    /// and one click should not be able to buy it over and over.
    video_unavailable: bool,
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
        term: tty::Terminal,
        settings: Settings,
        start: Startup,
    ) -> io::Result<Player> {
        let Startup {
            resolution,
            video_url,
            tap,
            target,
            complaint,
        } = start;
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
        let source_is_playlist = source.is_playlist;
        let entry_count = source.entries.len();
        let resolved_count = source.entries.iter().filter(|e| e.url.is_some()).count();
        let downloads = store::DownloadIndex::load();
        // A track already saved from an earlier session is already "done" - the queue
        // should say so from the first frame, not only once this session saves it again.
        let entry_status: Vec<EntryStatus> = source
            .entries
            .iter()
            .map(|entry| match &entry.url {
                Some(url) if downloads.get(url).is_some() => EntryStatus::Done,
                _ => EntryStatus::None,
            })
            .collect();
        let tui = ui::make_tui(term.clone())?;

        let clipboard_enabled = Arc::new(AtomicBool::new(settings.clipboard_watch));
        let clipboard_rx = spawn_clipboard_watcher(clipboard_enabled.clone());
        // The equalizer view opens on whatever is already in use rather than at the top,
        // so the first thing it shows is the shape currently being heard.
        let settings_equalizer = settings.equalizer;
        // Loaded once and split two ways: the lag estimate is worth carrying into every
        // session, but the offer to resume the exact track and position is only worth
        // making when this session was not itself launched with a target already.
        let saved_resume = store::Resume::load();
        let saved_release_lag = saved_resume.as_ref().and_then(|r| r.release_lag);

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
            // A queue is worth looking at; a single track is not, so that session opens
            // on the scope instead of a one-row list.
            pane: if source_is_playlist {
                Pane::Queue
            } else {
                Pane::Scope
            },
            menu_cursor: 0,
            selected: 0,
            edit_mode: false,
            settings_cursor: 0,
            entry_status,
            batch_active: false,
            batch_current: None,
            download_target: None,
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
            scope: 0,
            effects_on: vec![false; effects::ALL.len()],
            effects_cursor: 0,
            equalizer_cursor: settings_equalizer,
            spare: None,
            beats: analysis::BeatTracker::new(),
            cross: None,
            cross_af: (None, None),
            tempo_ratio: 1.0,
            tempo_applied: (1.0, 1.0),
            facts: Vec::new(),
            analysis: store::AnalysisCache::load(),
            cover: None,
            cover_for: String::new(),
            cover_loading: None,
            cover_drawn: None,
            cover_needs_clear: false,
            analysis_since: None,
            release_lag: saved_release_lag.unwrap_or(0.0),
            lag_learned: false,
            effects_engaged: Vec::new(),
            sync_cued: None,
            grid_refreshed: false,
            grid_taken: None,
            sync_trim: 0.0,
            pending_trim: 0.0,
            probing: Vec::new(),
            beat_waited: false,
            hooks_fired: 0,
            cross_refused: false,
            library: store::library_index_path()
                .map(|path| library::Library::load(&path))
                .unwrap_or_default(),
            scan: None,
            browse_selected: 0,
            browse_filter: None,
            prompt_wants: PromptWants::Media,
            history: store::History::load(),
            downloads,
            logged: None,
            target,
            // Only worth offering when there is nothing to play: a launch with a URL has
            // already said what it wants.
            resume: (!launched_with_url).then_some(saved_resume).flatten(),
            seek_on_open: None,
            pending_jump: false,
            mpris: mpris::Mpris::start(),
            track_seq: 0,
            published_title: String::new(),
            resume_written: Instant::now(),
            video_url,
            video_height: settings.quality.height(),
            video_track_loaded: false,
            video_is_muxed,
            video_unavailable: false,
            video_track_id: None,
            term_cols,
            term_rows,
            loading_until: None,
            dirty: true,
            source,
        };
        // The live stream is audio-only, which is exactly what an MP3 download wants:
        // capture it from track start so `d` is instant instead of re-fetching what played.
        if let Some(complaint) = complaint {
            player.show_toast(complaint);
        }
        player.attach_recorder();
        player.spawn_title_resolver();
        // Also pushes repeat, loudness matching and the crossfade deck into place.
        player.sync_seamless();
        if player.settings.shuffle && player.source.is_playlist {
            player.sync_shuffle();
        }
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
            self.drain_scan();
            self.drain_tempo_probe();
            self.maybe_probe_ahead();
            self.look_for_cover();
            self.drain_cover();
            self.pump_batch();

            let interval = if self.video_mode {
                REDRAW_VIDEO
            } else if self.pane == Pane::Scope && self.view == View::Main && !self.snap.paused {
                REDRAW_VIZ
            } else if self.listening_for_beats() {
                // The beat tracker reads the tap once per frame, so the redraw rate *is*
                // its sample rate. At the text cadence a beat is four or five samples
                // wide and the tracker rightly refuses to commit; at the visualizer
                // cadence it has enough to work with. Paid for half a minute a track,
                // and only when a style that wants bar lines is selected.
                REDRAW_VIZ
            } else {
                REDRAW_TEXT
            };
            if self.dirty || last_render.elapsed() >= interval {
                self.refresh();
                if let Flow::Quit = self.pump_mpris()? {
                    break;
                }
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
                    // The queue ran out rather than being left: there is nothing to
                    // come back to, so do not offer to.
                    store::Resume::clear();
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
                    self.append_to_playlist(&url);
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
                        match ev.title {
                            Some(title) => {
                                entry.title = title;
                                entry.titled = true;
                            }
                            // Keep the stand-in - it is all there is - but stop implying
                            // that a better one is on the way.
                            None => entry.title_failed = true,
                        }
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
                let at = self.entry_index() + 1;
                self.apply_queue(entries);
                self.input = None;
                if std::mem::take(&mut self.pending_jump) {
                    self.jump_to(at);
                } else {
                    self.show_toast(if n == 1 {
                        "⚡ queued next".to_string()
                    } else {
                        format!("⚡ queued {n} tracks")
                    });
                }
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
                DownloadState::Done { path } => {
                    if let Some(url) = &self.source.entries[current].url {
                        self.downloads.record(url, &path);
                    }
                    EntryStatus::Done
                }
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
                let spec = self.download_spec();
                self.download.start(url, spec, self.settings.tag_downloads);
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
        self.prompt_wants = PromptWants::Media;
        self.dirty = true;
    }

    /// The same input bar, asking for a folder to index instead of something to play.
    ///
    /// Library roots live in the index rather than in the settings file: the index
    /// already remembers what it was built from, so adding one is a rescan and there is
    /// no second copy of the list to disagree with the first.
    fn open_folder_prompt(&mut self) {
        self.input = Some(PromptState::default());
        self.prompt_wants = PromptWants::LibraryRoot;
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

    /// Reopen the previous session where it stopped.
    ///
    /// Resolution runs on the worker like any other open; the entry and position are
    /// remembered here and applied once mpv actually has the queue, because seeking a
    /// track that has not loaded yet does nothing.
    fn resume_last(&mut self) {
        let Some(resume) = self.resume.take() else {
            return;
        };
        if let Some(input) = &mut self.input {
            input.busy = true;
            input.error = None;
        }
        self.seek_on_open = Some((resume.entry, resume.position));
        self.spawn_open(resume.target);
        self.dirty = true;
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
        let expanded = expand_home(&text);
        if self.prompt_wants == PromptWants::LibraryRoot {
            let root = PathBuf::from(&expanded);
            if !root.is_dir() {
                input.error = Some(format!("{expanded} is not a folder"));
                self.dirty = true;
                return;
            }
            self.input = None;
            self.prompt_wants = PromptWants::Media;
            let mut roots = self.library.roots().to_vec();
            if !roots.contains(&root) {
                roots.push(root);
            }
            self.scan_roots(roots);
            return;
        }
        input.busy = true;
        input.error = None;
        // `~/Music` should work like a shell would treat it; anything that is neither a
        // link nor a path is a search.
        let target = youtube::as_target(&expanded);
        self.spawn_open(target);
        self.dirty = true;
    }

    /// Replace the whole session with a freshly resolved source.
    fn apply_launch(&mut self, res: youtube::Resolution) {
        // What the next session would have to reopen to get back here.
        self.target = res.source.url.clone();
        self.logged = None;
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
        self.entry_status = res
            .source
            .entries
            .iter()
            .map(|entry| match &entry.url {
                Some(url) if self.downloads.get(url).is_some() => EntryStatus::Done,
                _ => EntryStatus::None,
            })
            .collect();
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
        // The panel starts where a fresh launch would put it: a queue is worth looking
        // at, anything else opens on the scope. The old pane may have nothing left to
        // show, and a new session starts audio-only whatever the last one was doing.
        self.video_unavailable = false;
        self.pane = if self.source.is_playlist {
            Pane::Queue
        } else {
            Pane::Scope
        };
        self.auto_exit = false;
        self.loading_until = Some(Instant::now() + LOADING_HINT);
        self.abort_crossfade();
        self.cross_refused = false;
        self.snap = Snapshot::read(&mut self.mpv);
        self.attach_recorder();
        self.spawn_title_resolver();
        // The spare deck is still holding the queue that just went away.
        self.sync_seamless();
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
            self.entry_status = self
                .source
                .entries
                .iter()
                .map(|entry| match &entry.url {
                    Some(url) if self.downloads.get(url).is_some() => EntryStatus::Done,
                    _ => EntryStatus::None,
                })
                .collect();
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
                title_failed: false,
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
            let status = url
                .as_deref()
                .filter(|u| self.downloads.get(u).is_some())
                .map_or(EntryStatus::None, |_| EntryStatus::Done);
            self.source.entries.insert(at + k, entry);
            self.entry_status.insert(at + k, status);
            self.missing.insert(at + k, false);

            if let Some(url) = url {
                self.append_to_playlist(&url);
                let appended = self.mpv.playlist_count() - 1;
                let target = cur_mpv + 1 + k as i64;
                if appended > target {
                    self.move_in_playlist(appended, target);
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
        if self.snap.playlist_pos != prev_pos {
            // A plain advance (a track too short to overlap, or a transition that never
            // got its deck open) clears the "already tried" flag for the new track.
            self.cross_refused = false;
        }
        self.crossfade_tick();

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
            // A new track gets a fresh chance at a picture.
            self.video_unavailable = false;
        }
        self.recorder.refresh(&mut self.mpv);

        self.apply_pending_seek();
        self.remember();

        if let Some((_, deadline)) = &self.toast
            && Instant::now() >= *deadline
        {
            self.toast = None;
            self.dirty = true;
        }

        // A finished single download lingers a few seconds, then steps aside. Batch
        // outcomes are acknowledged by the queue instead.
        if !self.batch_active {
            let snapshot = self.download.snapshot();
            let terminal = matches!(
                snapshot,
                DownloadState::Done { .. }
                    | DownloadState::Failed { .. }
                    | DownloadState::Cancelled
            );
            match (terminal, self.msg_deadline) {
                (true, None) => {
                    if let DownloadState::Done { path } = snapshot
                        && let Some(url) = self.download_target.take()
                    {
                        self.mark_downloaded(&url, &path);
                    }
                    self.download_target = None;
                    self.msg_deadline = Some(Instant::now() + MSG_LINGER);
                }
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

    /// Record a save against the URL it was made from - the analysis probe and mpv itself
    /// both read this back, so a track paid for once is never paid for again - and mark
    /// whichever queue entry it belongs to, if any, so the row agrees with the fact
    /// immediately rather than after the next scan.
    fn mark_downloaded(&mut self, url: &str, path: &str) {
        self.downloads.record(url, path);
        if let Some(status) = self
            .source
            .entries
            .iter()
            .position(|entry| entry.url.as_deref() == Some(url))
            .and_then(|i| self.entry_status.get_mut(i))
        {
            *status = EntryStatus::Done;
        }
        self.dirty = true;
    }

    // -- the desktop's view of the player -------------------------------------------------

    /// Publish what is playing, and act on anything the desktop asked for.
    ///
    /// Called once per refresh. Both halves are non-blocking: the socket lives on its own
    /// thread, `publish` diffs internally so an unchanged frame costs nothing, and a bus
    /// that has gone away degrades to doing nothing at all.
    fn pump_mpris(&mut self) -> io::Result<Flow> {
        if self.mpris.is_none() {
            return Ok(Flow::Continue);
        }
        // A new track means a new `mpris:trackid`; clients treat a repeat of the same id
        // as a correction to the same track and will not redraw.
        if self.published_title != self.snap.title {
            self.published_title = self.snap.title.clone();
            self.track_seq = self.track_seq.wrapping_add(1);
        }
        let idle =
            self.source.is_empty() || (self.snap.title.is_empty() && self.snap.duration.is_none());
        let entry = self.entry_index();
        let art = self
            .source
            .entries
            .get(entry)
            .and_then(|e| e.url.as_deref())
            .filter(|url| url.starts_with('/'))
            .map(|path| format!("file://{path}"));
        let now = mpris::NowPlaying {
            title: &self.snap.title,
            // Real tags where the track has them. Publishing the source label as the
            // artist - which this did - puts "YouTube" on every lock screen in the house.
            artist: &self.snap.artist,
            album: &self.snap.album,
            art_url: art.as_deref(),
            position_us: (self.snap.position.unwrap_or(0.0) * 1e6) as i64,
            length_us: (self.snap.duration.unwrap_or(0.0) * 1e6) as i64,
            playing: !self.snap.paused && !idle,
            idle,
            can_next: self.source.is_playlist,
            can_prev: self.source.is_playlist,
            volume: self
                .cross
                .map_or(self.snap.volume.unwrap_or(100.0), |c| c.base)
                / 100.0,
            track_seq: self.track_seq,
        };
        // Commands first, then the frame: acting on a keypress and publishing the result
        // one tick later beats publishing state the desktop is about to change.
        let commands = {
            let Some(bus) = &self.mpris else {
                return Ok(Flow::Continue);
            };
            let commands = bus.take_commands();
            bus.publish(&now);
            commands
        };
        for command in commands {
            match command {
                mpris::MprisCommand::PlayPause => self.toggle_pause(),
                mpris::MprisCommand::Play if self.snap.paused => self.toggle_pause(),
                mpris::MprisCommand::Pause if !self.snap.paused => self.toggle_pause(),
                mpris::MprisCommand::Play | mpris::MprisCommand::Pause => {}
                mpris::MprisCommand::Stop => {
                    if !self.snap.paused {
                        self.toggle_pause();
                    }
                }
                mpris::MprisCommand::Next => self.playlist_next(),
                mpris::MprisCommand::Previous => self.playlist_prev(),
                mpris::MprisCommand::Seek(delta) => {
                    let _ = self.mpv.seek(delta as f64 / 1e6);
                }
                mpris::MprisCommand::SetPosition(at) => {
                    let _ = self.mpv.seek_absolute(at as f64 / 1e6);
                }
                mpris::MprisCommand::SetVolume(level) => {
                    self.set_user_volume((level * 100.0).clamp(0.0, 150.0));
                }
                mpris::MprisCommand::OpenUri(uri) => {
                    let target = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
                    self.queue_target(target, Queueing::PlayNext);
                }
                // Nothing to raise: the window is whatever terminal we were started in.
                mpris::MprisCommand::Raise => {}
                mpris::MprisCommand::Quit => return Ok(Flow::Quit),
            }
            self.dirty = true;
        }
        Ok(Flow::Continue)
    }

    // -- the library pane ----------------------------------------------------------------

    fn browse_rows(&self) -> Vec<ui::BrowseRow<'_>> {
        browse_rows(
            &self.library,
            &self.history,
            self.browse_filter.as_deref().unwrap_or_default(),
            &self.snap.title,
        )
    }

    /// What the pane's title says it is showing.
    fn library_status(&self) -> String {
        if let Some(scan) = &self.scan {
            let p = scan.progress();
            return format!("SCANNING {}/{}", p.done, p.total.max(p.done));
        }
        if self.library.is_empty() {
            if self.history.is_empty() {
                return "EMPTY · (R) SCAN".to_string();
            }
            return format!("{} PLAYED", self.history.len());
        }
        // Artists, not just tracks: it says something about the shape of a collection
        // that a file count does not.
        let artists: std::collections::HashSet<&str> = self
            .library
            .tracks()
            .iter()
            .map(|t| t.artist.as_str())
            .filter(|a| !a.is_empty())
            .collect();
        match artists.len() {
            0 => format!("{} TRACKS", self.library.len()),
            1 => format!("{} TRACKS · 1 ARTIST", self.library.len()),
            n => format!("{} TRACKS · {n} ARTISTS", self.library.len()),
        }
    }

    /// Index the usual places. Started only when the user asks - walking a music
    /// collection is an ffprobe per file, which is not something to do behind their back.
    fn start_scan(&mut self) {
        let mut roots: Vec<PathBuf> = self.library.roots().to_vec();
        if roots.is_empty() {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            roots = ["Music", "music", "Downloads/music"]
                .iter()
                .filter_map(|rel| home.as_ref().map(|h| h.join(rel)))
                .filter(|p| p.is_dir())
                .collect();
            // The player's own downloads are music too, and always exist by now.
            let downloads = PathBuf::from("downloads");
            if downloads.is_dir() {
                roots.push(downloads);
            }
        }
        if roots.is_empty() {
            self.show_toast("⌕ nothing to index - ( A ) adds a folder".to_string());
            return;
        }
        self.scan_roots(roots);
    }

    /// Index `roots`, replacing whatever the library was built from.
    ///
    /// Started only when the user asks: walking a music collection is an ffprobe per
    /// file, which is not something to do behind their back.
    fn scan_roots(&mut self, roots: Vec<PathBuf>) {
        if self.scan.is_some() {
            self.show_toast("⌕ already indexing".to_string());
            return;
        }
        self.show_toast(format!("⌕ indexing {} folder(s)…", roots.len()));
        self.scan = Some(self.library.rescan(roots));
        self.dirty = true;
    }

    /// Adopt a finished scan and cache it, so the next launch starts indexed.
    fn drain_scan(&mut self) {
        let Some(scan) = &self.scan else { return };
        let progress = scan.progress();
        if !progress.finished {
            // The count moves; the pane's title shows it.
            self.dirty = true;
            return;
        }
        if let Some(library) = scan.take() {
            if let Some(path) = store::library_index_path() {
                let _ = library.save(&path);
            }
            let known = self.seed_analysis_from_library(&library);
            self.show_toast(match known {
                0 => format!("⌕ indexed {} tracks", library.len()),
                n => format!(
                    "⌕ indexed {} tracks · {n} already know their tempo",
                    library.len()
                ),
            });
            self.library = library;
            self.browse_selected = 0;
        }
        self.scan = None;
        self.dirty = true;
    }

    /// Take every tempo the scan found in a file's own tags into the analysis cache.
    ///
    /// A `TBPM` tag is a measurement somebody has already paid for - by this player on a
    /// previous run, or by whatever wrote the file. Reading it costs the scan nothing (it
    /// is in the ffprobe output either way) and it means a local track is ready to mix
    /// the moment it is loaded, instead of being decoded from end to end again to learn
    /// what it already says on the tin.
    ///
    /// Keyed by path, which is what `Source::track_url` returns for a local file, so
    /// `Player::probe_tempo` finds it without knowing where it came from. The downbeat is
    /// not in the tag and is not invented: the tempo alone is a beat match, and the bar
    /// line is re-read cheaply from the frames when a sync actually wants one.
    fn seed_analysis_from_library(&mut self, library: &library::Library) -> usize {
        let mut seeded = 0;
        for track in library.tracks() {
            let Some(bpm) = track.bpm else { continue };
            let path = track.path.to_string_lossy().to_string();
            if self.analysis.get(&path).is_some() {
                continue;
            }
            self.analysis.record(&path, bpm, None);
            seeded += 1;
        }
        seeded
    }

    /// Enter or leave filter mode. In it the pane owns the keyboard, because a search
    /// box that ignores the letter `q` is not a search box.
    fn toggle_browse_filter(&mut self) {
        self.browse_filter = match self.browse_filter {
            Some(_) => None,
            None => Some(String::new()),
        };
        self.browse_selected = 0;
        self.dirty = true;
    }

    fn move_browse(&mut self, delta: i64) {
        let len = self.browse_rows().len();
        if len == 0 {
            return;
        }
        let at = (self.browse_selected as i64 + delta).clamp(0, len as i64 - 1);
        self.browse_selected = at as usize;
        self.dirty = true;
    }

    /// Put row `row` of the library pane into the queue.
    ///
    /// Two ways, because they are different intentions: Enter means "this, now", and `+`
    /// means "and this later" - the difference between picking a track and building an
    /// evening.
    fn play_browse_row(&mut self, row: usize, how: Queueing) {
        let rows = self.browse_rows();
        let Some(entry) = rows.get(row) else { return };
        let title = entry.title.to_string();
        drop(rows);

        let target = if self.library.is_empty() {
            self.history
                .recent()
                .find(|play| play.title == title)
                .map(|play| play.url.clone())
        } else {
            self.library
                .search(self.browse_filter.as_deref().unwrap_or_default(), 500)
                .get(row)
                .map(|track| track.path.display().to_string())
        };
        let Some(target) = target.filter(|t| !t.is_empty()) else {
            self.show_toast("⌕ nothing playable on that row".to_string());
            return;
        };
        self.browse_selected = row;
        self.queue_target(target, how);
    }

    /// Resolve `target` into the queue, jumping to it or leaving it for later. Falls back
    /// to opening it as a whole session when nothing is loaded.
    fn queue_target(&mut self, target: String, how: Queueing) {
        self.pending_jump = how == Queueing::PlayNext;
        self.spawn_open(target);
        self.show_toast(
            match how {
                Queueing::PlayNext => "⚡ playing next",
                Queueing::Append => "⚡ added to the queue",
            }
            .to_string(),
        );
        self.dirty = true;
    }

    // -- what was played, and where we were ----------------------------------------------

    /// Land a resumed session on the track and second it stopped at.
    ///
    /// Waits for mpv to have opened *something* - a `duration` is the cheapest proof
    /// that it has - because a jump or a seek issued before that is silently dropped.
    fn apply_pending_seek(&mut self) {
        let Some((entry, position)) = self.seek_on_open else {
            return;
        };
        if self.snap.duration.is_none() {
            return;
        }
        if entry != self.entry_index() && self.mpv_index_of(entry).is_some() {
            // Jump first; the seek lands on the next pass, once that track is open.
            self.jump_to(entry);
            return;
        }
        self.seek_on_open = None;
        if position > 1.0 {
            let _ = self.mpv.seek_absolute(position);
            self.show_toast(format!("↺ resumed at {}", ui::clock(position)));
        }
        self.dirty = true;
    }

    /// Write the history entry and the resume point for what is playing.
    ///
    /// Called every refresh, so both writes are gated: the history takes a track once it
    /// has actually been listened to (see [`store::worth_recording`]), and the resume
    /// point is rewritten at walking pace rather than at the redraw rate.
    fn remember(&mut self) {
        if self.source.is_empty() || self.snap.title.is_empty() {
            return;
        }
        let entry = self.entry_index();
        let url = self.source.track_url(entry);

        if self.logged.as_deref() != Some(self.snap.title.as_str())
            && store::worth_recording(self.snap.position, self.snap.duration)
        {
            self.history
                .record(&self.snap.title, &url, self.source.kind.label());
            self.logged = Some(self.snap.title.clone());
        }

        if self.resume_written.elapsed() >= RESUME_INTERVAL && !self.target.is_empty() {
            self.resume_written = Instant::now();
            store::Resume {
                target: self.target.clone(),
                title: self.snap.title.clone(),
                entry,
                position: self.snap.position.unwrap_or(0.0),
                // Whatever the estimate is worth so far - it only ever moves toward a
                // real measurement (see `learn_release_lag`), never away from one, so
                // even an unrefined 0.0 is exactly the prior a fresh session starts with.
                release_lag: Some(self.release_lag),
                at: 0,
            }
            .save();
        }
    }

    /// Current track as an index into `source.entries`.
    fn entry_index(&self) -> usize {
        entry_at(
            self.playlist_map.as_deref(),
            self.snap.playlist_pos.max(0) as usize,
            self.source.entries.len(),
        )
    }

    /// mpv playlist index for an entry, when the entry is actually in mpv's playlist.
    fn mpv_index_of(&self, entry: usize) -> Option<i64> {
        mpv_index(
            self.playlist_map.as_deref(),
            entry,
            self.source.entries.len(),
        )
    }

    /// The entry that plays after the current one, if anything does.
    ///
    /// Skipping anything that will not play. mpv's own playlist already leaves out the
    /// entries that never resolved, so most of the time there is nothing to skip - but the
    /// fallback past the end of the map walks *our* list, which still holds them, and a
    /// transition cued onto one of those is a transition into silence. The automix asks
    /// this what is coming next; it should be told what will actually be heard.
    fn next_entry_index(&self) -> Option<usize> {
        let len = self.source.entries.len();
        let mut pos = self.snap.playlist_pos.max(0) as usize;
        for _ in 0..len.max(1) {
            let candidate = next_entry(self.playlist_map.as_deref(), pos, len)?;
            if !self.unplayable(candidate) {
                return Some(candidate);
            }
            // Step past it in whichever space we are walking, and look again.
            match mpv_index(self.playlist_map.as_deref(), candidate, len) {
                Some(at) => pos = at.max(0) as usize,
                None => pos = pos.saturating_add(1),
            }
        }
        None
    }

    /// Whether `entry` is known to be unable to play at all.
    fn unplayable(&self, entry: usize) -> bool {
        self.missing.get(entry).copied().unwrap_or(false)
            || self
                .source
                .entries
                .get(entry)
                .is_some_and(|e| e.url.as_deref().unwrap_or_default().is_empty())
                && self.resolver_done
    }

    /// Title of whatever plays after the current track, if anything is known to.
    fn next_entry_title(&self) -> Option<&str> {
        let next = self.next_entry_index()?;
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
        let scope_live = self.pane == Pane::Scope
            && ui::scope_fits(self.term_cols, self.term_rows)
            && ui::pane_fits(self.term_cols, self.term_rows);
        let scope_name = self.vizzers[self.scope].name();
        let idle =
            self.source.is_empty() || (self.snap.title.is_empty() && self.snap.duration.is_none());
        let source_label = if self.source.is_empty() {
            "—"
        } else {
            self.source.kind.label()
        };
        let toast = self.toast.as_ref().map(|(text, _)| text.as_str());
        // Built from the fields rather than through `&self`, so the terminal can still
        // be borrowed mutably to draw with - the same reason `entry_rows` is free.
        let browse = if self.pane == Pane::Library {
            browse_rows(
                &self.library,
                &self.history,
                self.browse_filter.as_deref().unwrap_or_default(),
                &self.snap.title,
            )
        } else {
            Vec::new()
        };
        let library_status = self.library_status();
        // Only while something is actually coming out. A meter pinned at the bottom during
        // a pause reads as silence, and an absent one reads as not listening; those are
        // different things to be told, so the distinction is kept rather than flattened.
        let meter = self.tap.as_ref().and_then(|tap| {
            let (peak, hold, live) = tap.levels();
            live.then_some([(peak[0], hold[0]), (peak[1], hold[1])])
        });

        let state = ui::UiState {
            view: self.view,
            pane: self.pane,
            menu_cursor: self.menu_cursor,
            pane_ready: Pane::ALL.map(|pane| self.pane_available(pane)),
            browse: &browse,
            browse_selected: self.browse_selected,
            browse_filter: self.browse_filter.as_deref(),
            library_status: &library_status,
            source_label,
            title: &self.snap.title,
            artist: &self.snap.artist,
            album: &self.snap.album,
            position: self.snap.position,
            duration: self.snap.duration,
            // Mid-overlap the outgoing deck parks itself at its own end; that is not the
            // player being paused, and the chip must not say it is.
            paused: match self.cross {
                Some(cross) if cross.started.is_some() => cross.frozen,
                _ => self.snap.paused,
            },
            idle,
            volume: self.cross.map(|cross| cross.base).or(self.snap.volume),
            meter,
            cover: self.cover.is_some(),
            is_playlist: self.source.is_playlist,
            entry_index: current,
            next_title: (!next_title.is_empty()).then_some(next_title.as_str()),
            is_loading,
            resolving,
            crossfading: self.cross.is_some_and(|cross| cross.started.is_some()),
            analysis: self.analysis_state(),
            on_the_beat: self.cross.is_some_and(|cross| cross.on_the_beat),
            transition: self.settings.transition.label(),
            bpm: self
                .listening_for_beats()
                .then(|| self.beats.grid())
                .flatten()
                .filter(|grid| grid.confidence >= 0.5)
                .map(|grid| grid.bpm),
            stalled: self.mpv.stalled(),
            download: &download,
            download_title,
            cache: self.recorder.cache(),
            downloaded: self
                .downloads
                .get(&self.source.track_url(current))
                .is_some(),
            batch,
            download_enabled,
            entries: &rows,
            selected: self.selected,
            edit_mode: self.edit_mode,
            settings: &self.settings,
            settings_cursor: self.settings_cursor,
            effects_on: &self.effects_on,
            effects_cursor: self.effects_cursor,
            equalizer_cursor: self.equalizer_cursor,
            quality_locked: locked,
            format_locked: locked,
            scope_name,
            prompt: self.input.as_ref(),
            prompt_tag: match self.prompt_wants {
                PromptWants::Media => "ADD NEXT",
                PromptWants::LibraryRoot => "MUSIC FOLDER",
            },
            toast,
            resume: self
                .resume
                .as_ref()
                .map(|r| (r.title.as_str(), store::ago(r.at))),
            term_cols: self.term_cols,
            term_rows: self.term_rows,
        };

        if self.video_mode {
            let (ansi, map) = ui::video_bottom(&state);
            self.click_map = map;
            return self.term.paint(ansi.as_bytes());
        }
        let wave_len = (self.term_cols as usize) * 2;
        // The tap is sampled for the scope, and also for the beat tracker in the run-up
        // to a transition - the one other thing that needs to know what the audio is
        // doing. Outside those two cases it is not sampled at all.
        let vsnap = match &self.tap {
            Some(tap) if scope_live || self.listening_for_beats() => tap.snapshot(wave_len),
            _ => audio_tap::VizSnapshot::default(),
        };
        if vsnap.live && self.listening_for_beats() {
            self.beats.feed(
                &vsnap.bands,
                (vsnap.rms[0] + vsnap.rms[1]) / 2.0,
                self.snap.position.unwrap_or(0.0),
            );
        }
        let viz_pair = scope_live.then(|| {
            (
                &mut *self.vizzers[self.scope] as &mut dyn Visualizer,
                &vsnap,
            )
        });
        if self.cover_needs_clear {
            self.cover_needs_clear = false;
            self.tui.clear()?;
        }
        let map = ui::draw(&mut self.tui, &state, viz_pair)?;
        self.paint_cover(&map);
        self.click_map = map;
        Ok(())
    }

    /// Put the cover picture in the box the layout left for it.
    ///
    /// After the frame, because a picture is not made of cells: it is an escape sequence
    /// addressed to absolute coordinates, and it has to land on a rectangle that has
    /// already been painted. The layout leaves that rectangle blank, so the frame diffing
    /// that follows has nothing to say about those cells and will not paint over it -
    /// which is also why this only re-emits when the box moves rather than every frame.
    fn paint_cover(&mut self, map: &ui::ClickMap) {
        let Some(art) = &self.cover else {
            self.cover_drawn = None;
            return;
        };
        let Some((col, row, _, _)) = map.cover_area() else {
            self.cover_drawn = None;
            return;
        };
        if self.cover_drawn == Some((col, row)) {
            return;
        }
        // Only if the box is still the shape the picture was prepared for. A terminal that
        // was resized between the two would otherwise get a picture scaled for the old one,
        // stretched across cells it was never meant to cover.
        if art.size() != (ui::COVER_COLS, ui::COVER_ROWS) {
            return;
        }
        // One-based and absolute, which is what the escape wants.
        let _ = self
            .term
            .paint(art.escape_sequence(col + 1, row + 1).as_bytes());
        self.cover_drawn = Some((col, row));
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
            MouseEventKind::ScrollUp => self.on_scroll(-1, mouse.column, mouse.row),
            MouseEventKind::ScrollDown => self.on_scroll(1, mouse.column, mouse.row),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// The wheel scrolls the queue when the pointer is over it; everywhere else - and
    /// with the queue panel closed - it turns the volume.
    fn on_scroll(&mut self, delta: i64, col: u16, row: u16) {
        if !self.video_mode && self.click_map.scrolls_queue(col, row) {
            // The same region, whichever list is in it.
            match self.pane {
                Pane::Library => self.move_browse(delta),
                _ => self.move_selection(delta),
            }
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
            View::Menu => self.on_key_menu(key)?,
            View::Settings => self.on_key_settings(key)?,
            View::Effects => self.on_key_effects(key)?,
            View::Equalizer => self.on_key_equalizer(key)?,
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
            KeyCode::Tab if self.resume.is_some() => self.resume_last(),
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
        // The library pane's filter takes the whole keyboard while it is open: a search
        // box that treats `q` as quit is not a search box.
        if self.pane == Pane::Library && !self.video_mode && self.browse_filter.is_some() {
            match key.code {
                KeyCode::Esc => self.toggle_browse_filter(),
                KeyCode::Enter => {
                    let row = self.browse_selected;
                    self.play_browse_row(row, Queueing::PlayNext);
                }
                KeyCode::Down => self.move_browse(1),
                KeyCode::Up => self.move_browse(-1),
                KeyCode::PageDown => self.move_browse(10),
                KeyCode::PageUp => self.move_browse(-10),
                KeyCode::Backspace => {
                    if let Some(filter) = &mut self.browse_filter {
                        filter.pop();
                        self.browse_selected = 0;
                        self.dirty = true;
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(filter) = &mut self.browse_filter {
                        filter.push(c);
                        self.browse_selected = 0;
                        self.dirty = true;
                    }
                }
                _ => {}
            }
            return Ok(Flow::Continue);
        }
        if self.pane == Pane::Library && !self.video_mode {
            match key.code {
                KeyCode::Char('/') => {
                    self.toggle_browse_filter();
                    return Ok(Flow::Continue);
                }
                KeyCode::Char('R') => {
                    self.start_scan();
                    return Ok(Flow::Continue);
                }
                KeyCode::Char('A') => {
                    self.open_folder_prompt();
                    return Ok(Flow::Continue);
                }
                KeyCode::Down => {
                    self.move_browse(1);
                    return Ok(Flow::Continue);
                }
                KeyCode::Up => {
                    self.move_browse(-1);
                    return Ok(Flow::Continue);
                }
                KeyCode::PageDown => {
                    self.move_browse(10);
                    return Ok(Flow::Continue);
                }
                KeyCode::PageUp => {
                    self.move_browse(-10);
                    return Ok(Flow::Continue);
                }
                KeyCode::Enter => {
                    let row = self.browse_selected;
                    self.play_browse_row(row, Queueing::PlayNext);
                    return Ok(Flow::Continue);
                }
                KeyCode::Char('+') => {
                    let row = self.browse_selected;
                    self.play_browse_row(row, Queueing::Append);
                    return Ok(Flow::Continue);
                }
                _ => {}
            }
        }
        // The queue lives in the centre panel now, so its cursor shares the main view's
        // keyboard. It takes the arrows and Enter; h/l/j/k stay seek and volume, which
        // is what they mean everywhere else in this view.
        if self.pane == Pane::Queue && !self.video_mode {
            match key.code {
                KeyCode::Down => {
                    self.move_selection(1);
                    return Ok(Flow::Continue);
                }
                KeyCode::Up => {
                    self.move_selection(-1);
                    return Ok(Flow::Continue);
                }
                KeyCode::PageDown => {
                    self.move_selection(10);
                    return Ok(Flow::Continue);
                }
                KeyCode::PageUp => {
                    self.move_selection(-10);
                    return Ok(Flow::Continue);
                }
                KeyCode::Enter if !self.edit_mode => {
                    self.jump_to(self.selected);
                    return Ok(Flow::Continue);
                }
                KeyCode::Char('J') => {
                    self.move_entry(1);
                    return Ok(Flow::Continue);
                }
                KeyCode::Char('K') => {
                    self.move_entry(-1);
                    return Ok(Flow::Continue);
                }
                KeyCode::Char('E') => {
                    self.toggle_edit();
                    return Ok(Flow::Continue);
                }
                KeyCode::Char('a') => {
                    self.toggle_batch();
                    return Ok(Flow::Continue);
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('q') => return Ok(Flow::Quit),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('h') | KeyCode::Left => {
                let _ = self.mpv.seek(-SEEK_STEP);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let _ = self.mpv.seek(SEEK_STEP);
            }
            KeyCode::Char('j') | KeyCode::Down => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Char('k') | KeyCode::Up => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('n') if self.source.is_playlist => self.playlist_next(),
            KeyCode::Char('b') if self.source.is_playlist => self.playlist_prev(),
            KeyCode::Char('v') => self.open_view_menu()?,
            KeyCode::Char('p') => self.show_pane(Pane::Queue)?,
            KeyCode::Char('d') => self.save_or_cancel(),
            KeyCode::Char('o') => self.open_prompt(),
            KeyCode::Char('s') => self.open_view(View::Settings)?,
            KeyCode::Char('e') => self.open_view(View::Effects)?,
            KeyCode::Char('g') => self.open_view(View::Equalizer)?,
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// The view chooser: a popup over the main view, so it only owns the keys it needs.
    fn on_key_menu(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') => return Ok(Flow::Quit),
            KeyCode::Esc | KeyCode::Char('v') => self.close_menu()?,
            KeyCode::Char('j') | KeyCode::Down => {
                self.menu_cursor = (self.menu_cursor + 1) % Pane::ALL.len();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.menu_cursor = (self.menu_cursor + Pane::ALL.len() - 1) % Pane::ALL.len();
            }
            // Restyling the scope is the only variant a row has, and it reads the same
            // way the settings rows do.
            KeyCode::Char('l') | KeyCode::Right => self.cycle_scope(true),
            KeyCode::Char('h') | KeyCode::Left => self.cycle_scope(false),
            KeyCode::Enter | KeyCode::Char(' ') => self.choose_menu_row(self.menu_cursor)?,
            _ => {}
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    fn on_key_settings(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('s') => self.close_view()?,
            KeyCode::Char('j') | KeyCode::Down => {
                let last = ui::SETTING_ROWS - 1;
                self.settings_cursor = (self.settings_cursor + 1).min(last);
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
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('e') => self.close_view()?,
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

    fn on_key_equalizer(&mut self, key: KeyEvent) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('g') => self.close_view()?,
            KeyCode::Char('j') | KeyCode::Down => {
                self.equalizer_cursor = (self.equalizer_cursor + 1).min(equalizer::ALL.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.equalizer_cursor = self.equalizer_cursor.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Right => {
                self.choose_equalizer(self.equalizer_cursor);
            }
            _ => {}
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    /// Switch to the preset at `index`: rebuild both decks' filter chains and remember it.
    ///
    /// Only when it is actually a change. Re-selecting what is already on would rebuild
    /// mpv's filter graph mid-playback for no audible reason, which is the one thing the
    /// whole `af` path is careful not to do.
    fn choose_equalizer(&mut self, index: usize) {
        let index = index.min(equalizer::ALL.len() - 1);
        self.equalizer_cursor = index;
        if self.settings.equalizer == index {
            return;
        }
        self.settings.equalizer = index;
        self.settings.save();
        // The transition owns the chain while one is running, so the change has to go
        // through the shape rather than around it - same as an effect switched mid-mix.
        self.cross_af = (None, None);
        self.apply_af();
        self.show_toast(format!("▤ EQ · {}", equalizer::at(index).name));
        self.dirty = true;
    }

    /// One dispatcher for every click, so a button can't do anything a key can't.
    fn dispatch(&mut self, action: Action) -> io::Result<Flow> {
        match action {
            Action::Quit => return Ok(Flow::Quit),
            Action::TogglePause => self.toggle_pause(),
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
            Action::CyclePane => self.cycle_pane()?,
            Action::CycleScope => self.cycle_scope(true),
            Action::OpenViewMenu => self.open_view_menu()?,
            Action::MenuRow(row) => {
                if row < Pane::ALL.len() {
                    self.menu_cursor = row;
                    self.choose_menu_row(row)?;
                }
            }
            Action::Download => self.save_or_cancel(),
            Action::DownloadAll => self.toggle_batch(),
            Action::OpenSettings => self.open_view(View::Settings)?,
            Action::OpenEffects => self.open_view(View::Effects)?,
            Action::OpenPrompt => self.open_prompt(),
            Action::Submit => self.submit_input(),
            Action::ResumeLast => self.resume_last(),
            Action::ToggleEdit => self.toggle_edit(),
            Action::MoveUp => self.move_entry(-1),
            Action::MoveDown => self.move_entry(1),
            Action::CloseView => self.close_view()?,
            Action::JumpTo(entry) => self.jump_to(entry),
            Action::BrowseRow(row) => self.play_browse_row(row, Queueing::PlayNext),
            Action::BrowseAppend(row) => self.play_browse_row(row, Queueing::Append),
            Action::BrowseFilter => self.toggle_browse_filter(),
            Action::AddLibraryRoot => self.open_folder_prompt(),
            Action::SettingsRow(row) => {
                if row < ui::SETTING_ROWS {
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
            Action::OpenEqualizer => self.open_view(View::Equalizer)?,
            Action::EqualizerRow(index) => {
                if index < equalizer::ALL.len() {
                    self.choose_equalizer(index);
                }
            }
        }
        self.dirty = true;
        Ok(Flow::Continue)
    }

    // -- views ---------------------------------------------------------------------------

    /// Open a full-screen view. The views are text-mode; live ASCII video is torn down
    /// first, exactly as leaving the video pane would.
    fn open_view(&mut self, view: View) -> io::Result<()> {
        if self.video_mode {
            self.disable_video()?;
        }
        self.view = view;
        let _ = self.tui.clear();
        self.dirty = true;
        Ok(())
    }

    fn close_view(&mut self) -> io::Result<()> {
        if self.view == View::Settings {
            self.settings.save();
        }
        if self.view == View::Menu {
            // Backing out of the chooser is not a choice: whatever the panel was
            // showing when it opened comes back, picture included.
            return self.close_menu();
        }
        self.view = View::Main;
        let _ = self.tui.clear();
        self.dirty = true;
        // Settings and effects tear the picture down on the way in; put it back.
        self.apply_pane()
    }

    // -- the centre panel ----------------------------------------------------------------

    /// Open the view chooser. The picture cannot survive a popup drawn over it, so
    /// video is torn down here and put back by [`Player::close_menu`] if the user backs
    /// out without choosing something else.
    fn open_view_menu(&mut self) -> io::Result<()> {
        if self.video_mode {
            self.disable_video()?;
        }
        self.menu_cursor = self.pane.index();
        self.view = View::Menu;
        let _ = self.tui.clear();
        self.dirty = true;
        Ok(())
    }

    /// Leave the chooser, re-applying the pane it opened on - which restarts the
    /// picture when that pane is video.
    fn close_menu(&mut self) -> io::Result<()> {
        self.view = View::Main;
        let _ = self.tui.clear();
        self.dirty = true;
        self.apply_pane()
    }

    fn choose_menu_row(&mut self, row: usize) -> io::Result<()> {
        let Some(&pane) = Pane::ALL.get(row) else {
            return Ok(());
        };
        // Re-picking the scope restyles it instead of doing nothing.
        if pane == Pane::Scope && self.pane == Pane::Scope {
            self.cycle_scope(true);
            return Ok(());
        }
        if let Some(why) = self.pane_blocked(pane) {
            // Say why and stay in the chooser, so the next pick is one keystroke away
            // rather than one keystroke plus reopening the menu.
            self.show_toast(why.to_string());
            return Ok(());
        }
        self.set_pane(pane);
        self.close_menu()
    }

    /// Step the panel to the next pane that can actually be shown here. Panes with
    /// nothing to say (an empty queue, video on a terminal too small for a picture)
    /// are skipped rather than selected and apologised for.
    fn cycle_pane(&mut self) -> io::Result<()> {
        let start = self.pane.index();
        for step in 1..=Pane::ALL.len() {
            let pane = Pane::ALL[(start + step) % Pane::ALL.len()];
            if self.pane_available(pane) {
                return self.show_pane(pane);
            }
        }
        Ok(())
    }

    /// Why `pane` cannot be shown right now, or `None` when it can.
    fn pane_blocked(&self, pane: Pane) -> Option<&'static str> {
        match pane {
            Pane::Queue if self.source.entries.is_empty() => Some("≡ nothing queued yet"),
            Pane::Scope if !ui::scope_fits(self.term_cols, self.term_rows) => {
                Some("∿ terminal too small for scopes")
            }
            Pane::Video if self.source.is_empty() => Some("▣ nothing to show"),
            Pane::Video if !ui::video_fits(self.term_cols, self.term_rows) => {
                Some("▣ terminal too small for video")
            }
            Pane::Video if self.video_unavailable => Some("▣ no video for this track"),
            _ => None,
        }
    }

    fn pane_available(&self, pane: Pane) -> bool {
        self.pane_blocked(pane).is_none()
    }

    /// Show `pane`, reporting why when it cannot be shown at all.
    fn show_pane(&mut self, pane: Pane) -> io::Result<()> {
        if let Some(why) = self.pane_blocked(pane) {
            self.show_toast(why.to_string());
            return Ok(());
        }
        self.set_pane(pane);
        self.apply_pane()
    }

    /// Pure state: which pane, plus the bookkeeping that pane needs on arrival.
    fn set_pane(&mut self, pane: Pane) {
        if pane == Pane::Queue && self.pane != Pane::Queue {
            // Land on what is playing, not on wherever the cursor was left.
            self.selected = self.entry_index();
        }
        if pane != Pane::Queue {
            self.edit_mode = false;
        }
        self.pane = pane;
        self.dirty = true;
    }

    /// Make the renderer match the selected pane: video owns the whole screen, the
    /// other two are drawn inside the text UI.
    fn apply_pane(&mut self) -> io::Result<()> {
        match (self.pane, self.video_mode) {
            (Pane::Video, false) => {
                self.video_mode = true;
                if !self.enable_video() {
                    // Nothing to show - fall back to the scope rather than blanking the UI,
                    // and stop offering this track a picture it does not have.
                    self.video_mode = false;
                    self.video_unavailable = true;
                    self.pane = Pane::Scope;
                    // Resolving a picture takes long enough to be worth a "loading" frame,
                    // and that frame went out through the video-mode painter - rows ratatui
                    // has no record of. The text UI has to start from a clean screen or it
                    // leaves half a video bar stranded under itself.
                    ui::clear_screen(&self.term)?;
                    let _ = self.tui.clear();
                    self.show_toast("▣ no video for this track".to_string());
                    self.dirty = true;
                    return Ok(());
                }
                ui::clear_screen(&self.term)?;
                self.dirty = true;
            }
            (pane, true) if pane != Pane::Video => self.disable_video()?,
            _ => {}
        }
        Ok(())
    }

    fn toggle_edit(&mut self) {
        self.edit_mode = !self.edit_mode && self.source.entries.len() > 1;
        self.dirty = true;
    }

    /// Change the selected settings row. Locked rows (audio-only source, local files)
    /// stay put - the menu shows why instead.
    fn change_setting(&mut self, forward: bool) {
        let Some(row) = ui::Setting::at(self.settings_cursor) else {
            return;
        };
        match row {
            ui::Setting::Quality if !self.settings_locked() => {
                self.settings.quality = self.settings.quality.cycle(forward);
                // A height we have not tried yet may well exist.
                self.video_unavailable = false;
            }
            ui::Setting::Format if !self.settings_locked() => {
                self.settings.format = self.settings.format.cycle();
            }
            ui::Setting::TagDownloads => {
                self.settings.tag_downloads = !self.settings.tag_downloads;
            }
            ui::Setting::Shuffle if self.source.is_playlist => {
                self.settings.shuffle = !self.settings.shuffle;
                self.sync_shuffle();
            }
            ui::Setting::Repeat => {
                self.settings.repeat = self.settings.repeat.cycle(forward);
                self.sync_playback_modes();
            }
            ui::Setting::Normalize => {
                self.settings.normalize = self.settings.normalize.cycle(forward);
                self.sync_playback_modes();
            }
            ui::Setting::SmartLoading => {
                self.settings.smart_loading = !self.settings.smart_loading;
            }
            ui::Setting::Clipboard => {
                self.settings.clipboard_watch = !self.settings.clipboard_watch;
                self.clipboard_enabled
                    .store(self.settings.clipboard_watch, Ordering::Relaxed);
            }
            ui::Setting::Crossfade => {
                self.settings.crossfade = !self.settings.crossfade;
                // Spawns or drops the second decoder, and takes prefetch with it.
                self.sync_seamless();
            }
            ui::Setting::FadeLength if self.settings.crossfade => {
                self.settings.cycle_crossfade(forward);
            }
            ui::Setting::FadeCurve if self.settings.crossfade => {
                self.settings.fade_curve = self.settings.fade_curve.cycle(forward);
            }
            ui::Setting::BeatMixing if self.settings.crossfade => {
                self.settings.beat_mixing = !self.settings.beat_mixing;
                self.switch_mixing_engine();
            }
            ui::Setting::Transition if self.settings.crossfade && self.settings.beat_mixing => {
                self.settings.transition = self.settings.transition.cycle(forward);
            }
            _ => return,
        }
        self.settings.save();
        self.dirty = true;
    }

    /// Move between the two mixing engines, which is not a free switch.
    ///
    /// Beat mixing needs the whole of the audio path: the tap running so the beat is being
    /// followed, and no video, because reading the next track while this one plays is a
    /// different job from showing a picture. Turning it on therefore drops out of video,
    /// which mpv can only do by reloading the file - so the track stops for as long as that
    /// takes and resumes where it was. Better a visible half-second than a setting that
    /// silently does nothing, which is what it did before.
    fn switch_mixing_engine(&mut self) {
        if self.settings.beat_mixing && self.video_mode {
            // The pane goes with it: leaving video mode while the video pane is still
            // selected would put it straight back on the next redraw.
            self.pane = Pane::Scope;
            let _ = self.disable_video();
            self.show_toast("⇄ beat mixing on · video off".to_string());
        } else if self.settings.beat_mixing {
            self.show_toast("⇄ beat mixing on · tracks measured and matched".to_string());
        } else {
            // Nothing measured, nothing read ahead, and the fade is the plain one.
            self.facts.clear();
            self.probing.clear();
            self.show_toast("⇄ beat mixing off · plain fade".to_string());
        }
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
        // Tone first, effects on top of it, the tap last so the scopes measure what the
        // ears get. One order, here and in `Player::af_chain` - the same chain either
        // side of a transition, or turning one on would change how the EQ sounds.
        let shaped = self.shaping_chain();
        let tap = self.tap.as_ref().map(|tap| tap.graph());
        let af = match (shaped.clone(), tap) {
            (Some(shaped), Some(tap)) => Some(format!("{shaped},{tap}")),
            (Some(shaped), None) => Some(shaped),
            (None, Some(tap)) => Some(tap),
            (None, None) => None,
        };
        let _ = self.mpv.set_af(af.as_deref());
        // The parked deck gets the shaping but never the tap: two writers would
        // interleave garbage into its FIFOs. The tap moves across with the swap.
        if let Some(deck) = self.spare.as_mut() {
            let _ = deck.set_af(shaped.as_deref());
        }
    }

    /// The user's own shaping, in the order it is heard: the equalizer, then whatever of
    /// the effects rack is switched on. Everything both decks always get, whether or not
    /// a transition is running - which is why it is one function rather than two lists
    /// assembled in two places.
    fn shaping_chain(&self) -> Option<String> {
        let parts: Vec<String> = [
            equalizer::chain(self.settings.equalizer),
            effects::chain(&self.effects_on),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(","))
    }

    // -- cover art -----------------------------------------------------------------------

    /// Start looking for a picture for whatever is playing, if this is a new track.
    ///
    /// A file on disk carries its picture or has one sitting beside it; a stream may carry
    /// one too, and ffmpeg will read an http(s) URL directly to find out. Either way it is
    /// an ffmpeg run and possibly a network round trip, so it happens on its own thread and
    /// the answer is kept until the track changes.
    fn look_for_cover(&mut self) {
        let target = self.playing_target();
        if target == self.cover_for {
            return;
        }
        self.cover_for = target.clone();
        self.cover = None;
        self.cover_needs_clear |= self.cover_drawn.is_some();
        self.cover_drawn = None;
        self.dirty = true;
        if target.is_empty() {
            self.cover_loading = None;
            return;
        }
        // A stream's own picture, when analysis already found one - a free read from the
        // shared cache, the same blob the tempo probe fills in.
        let known_thumbnail = self
            .facts
            .iter()
            .find(|(entry, _)| *entry == self.entry_index())
            .and_then(|(_, facts)| facts.thumbnail.clone());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let picture_url = match known_thumbnail {
                Some(url) => url,
                // Not cached - beat mixing off, or this track not probed yet. Resolved
                // here instead, off this thread: the same call the analysis pipeline
                // makes, just not gated on that setting.
                None if youtube::needs_media_resolution(&target) => {
                    match youtube::stream_media_url(&target) {
                        youtube::MediaUrl::Direct {
                            thumbnail: Some(url),
                            ..
                        } => url,
                        _ => target.clone(),
                    }
                }
                None => target.clone(),
            };
            let art = if picture_url.contains("://") {
                artwork::Art::for_url(&picture_url, ui::COVER_COLS, ui::COVER_ROWS)
            } else {
                artwork::Art::for_file(Path::new(&picture_url), ui::COVER_COLS, ui::COVER_ROWS)
            };
            let _ = tx.send(art);
        });
        self.cover_loading = Some(rx);
    }

    /// What is playing, as something ffmpeg can open.
    fn playing_target(&self) -> String {
        let entry = self.entry_index();
        self.source
            .entries
            .get(entry)
            .and_then(|e| e.url.clone())
            .unwrap_or_else(|| self.target.clone())
    }

    /// Collect a finished cover, if one landed.
    fn drain_cover(&mut self) {
        let Some(rx) = &self.cover_loading else {
            return;
        };
        match rx.try_recv() {
            Ok(art) => {
                self.cover = art;
                self.cover_loading = None;
                self.cover_drawn = None;
                self.dirty = true;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.cover_loading = None,
        }
    }

    // -- playback ------------------------------------------------------------------------

    fn begin_track_switch(&mut self) {
        // A manual jump takes the cut as-is: whatever overlap was building is dropped.
        self.abort_crossfade();
        self.cross_refused = false;
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
        self.move_in_playlist(mi, if delta > 0 { mj + 1 } else { mj });
        self.selected = j;
        self.dirty = true;
    }

    /// Start capturing the playing track, remembering where playback was at the time.
    /// Local files are already on disk - nothing to capture.
    fn attach_recorder(&mut self) {
        if self.source.kind == SourceKind::Local || self.source.is_empty() {
            return;
        }
        // Nothing to capture for a track already on disk. The recorder exists so that
        // pressing `d` on a stream is instant instead of a fresh download; a track that
        // has already been saved is past that - re-capturing it would spend a temp file
        // and a write per play to arrive at a copy of a file we are holding already.
        if self
            .downloads
            .get(&self.source.track_url(self.entry_index()))
            .is_some()
        {
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
            self.download_target = Some(self.source.track_url(self.entry_index()));
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
            let url = self.source.track_url(self.entry_index());
            self.download_target = Some(url.clone());
            self.download.start(url, spec, self.settings.tag_downloads);
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

    /// Step to the next visualizer style. Pure rendering state - the tap is always
    /// measuring, so switching is instant - and it no longer has its own key: the
    /// chooser owns it, next to the pane it restyles.
    fn cycle_scope(&mut self, forward: bool) {
        let n = self.vizzers.len();
        self.scope = if forward {
            (self.scope + 1) % n
        } else {
            (self.scope + n - 1) % n
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

/// Rows for the library pane.
///
/// Two sources behind one list: an index of what is on disk, and - while that index is
/// empty - what has actually been played. A first run with nothing indexed is therefore
/// still a useful pane rather than an empty box.
///
/// A free function over the specific fields, like [`entry_rows`], so the caller can hold
/// the result while borrowing the terminal mutably.
fn browse_rows<'a>(
    library: &'a library::Library,
    history: &'a store::History,
    filter: &str,
    playing: &str,
) -> Vec<ui::BrowseRow<'a>> {
    const LIMIT: usize = 500;
    if !library.is_empty() {
        // Ranked by the library's own match quality first, then nudged by what has
        // actually been played: between two equally good matches, the one you know.
        let plays = history.play_counts();
        let mut hits = library.search(filter, LIMIT);
        if filter.trim().is_empty() {
            hits.sort_by_key(|track| {
                std::cmp::Reverse(
                    plays
                        .get(track.path.to_string_lossy().as_ref())
                        .copied()
                        .unwrap_or(0),
                )
            });
        }
        return hits
            .into_iter()
            .map(|track| ui::BrowseRow {
                title: &track.title,
                detail: match (track.artist.is_empty(), track.album.is_empty()) {
                    (true, true) => String::new(),
                    (false, true) => track.artist.clone(),
                    (true, false) => track.album.clone(),
                    (false, false) => format!("{} · {}", track.artist, track.album),
                },
                duration: Some(track.duration),
                current: track.title == playing,
            })
            .collect();
    }

    let needle = filter.to_lowercase();
    history
        .recent()
        .filter(|play| needle.is_empty() || play.title.to_lowercase().contains(&needle))
        .take(LIMIT)
        .map(|play| ui::BrowseRow {
            title: &play.title,
            detail: store::ago(play.at),
            duration: None,
            current: play.title == playing,
        })
        .collect()
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
            title_failed: entry.title_failed,
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

#[cfg(test)]
mod tests {
    use super::*;

    // A Spotify playlist of five tracks where entries 1 and 3 never matched: mpv only
    // ever saw 0, 2 and 4, so mpv position 1 is our entry 2.
    const SPARSE: &[usize] = &[0, 2, 4];
    const LEN: usize = 5;

    #[test]
    fn identity_mapping_is_position_for_position() {
        assert_eq!(entry_at(None, 0, LEN), 0);
        assert_eq!(entry_at(None, 3, LEN), 3);
        // Past the end (mpv briefly reports a stale position) clamps instead of panicking.
        assert_eq!(entry_at(None, 99, LEN), LEN - 1);
        assert_eq!(entry_at(None, 0, 0), 0);

        assert_eq!(mpv_index(None, 2, LEN), Some(2));
        assert_eq!(mpv_index(None, LEN, LEN), None);

        assert_eq!(next_entry(None, 0, LEN), Some(1));
        assert_eq!(
            next_entry(None, LEN - 1, LEN),
            None,
            "no next after the last"
        );
    }

    #[test]
    fn a_sparse_map_translates_both_ways() {
        for (pos, entry) in [(0, 0), (1, 2), (2, 4)] {
            assert_eq!(entry_at(Some(SPARSE), pos, LEN), entry);
            assert_eq!(mpv_index(Some(SPARSE), entry, LEN), Some(pos as i64));
        }
        // Unmatched entries are in no mpv playlist, so they have no mpv index.
        assert_eq!(mpv_index(Some(SPARSE), 1, LEN), None);
        assert_eq!(mpv_index(Some(SPARSE), 3, LEN), None);
    }

    #[test]
    fn next_skips_the_entries_mpv_never_got() {
        // Playing entry 0: the next thing mpv will play is 2, not the unmatched 1.
        assert_eq!(next_entry(Some(SPARSE), 0, LEN), Some(2));
        assert_eq!(next_entry(Some(SPARSE), 1, LEN), Some(4));
    }

    #[test]
    fn next_past_the_end_of_the_map_falls_back_to_our_own_list() {
        // Last mapped position: nothing follows in mpv's playlist, and entry 4 is the
        // last entry we have either.
        assert_eq!(next_entry(Some(SPARSE), 2, LEN), None);
        // Same map, but the resolver has since appended two more entries: the next one
        // is nameable even though mpv has not been told about it yet.
        assert_eq!(next_entry(Some(SPARSE), 2, LEN + 2), Some(5));
        // A position past the map entirely must not wrap around or panic.
        assert_eq!(next_entry(Some(SPARSE), 99, LEN), Some(1));
    }
}
