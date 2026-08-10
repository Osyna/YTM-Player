//! Terminal rendering.
//!
//! Text mode is ratatui: four full-screen views (Main, Playlist, Settings, Open) where
//! every control is also a click target. Each frame is drawn into a buffer and handed to
//! the shared [`tty::Terminal`] writer as one atomic paint, so nothing can interleave
//! with it. The main view hosts the live visualizer pane ([`crate::visualizer`]) and a
//! full-height volume rail on the right edge.
//!
//! Video mode shares the same widgets: mpv's `tct` renderer owns the top rows, and the
//! bottom [`RESERVED_ROWS`] hold the same title/progress/buttons/status block text mode
//! draws - rendered into an off-screen [`Buffer`] and serialized to ANSI positioned over
//! those rows only ([`video_bottom`]). One builder set, one [`ClickMap`], no drift.
//!
//! Both paths dispatch the same [`Action`] vocabulary, so a key and a click can't drift.

use crate::audio_tap::VizSnapshot;
use crate::effects;
use crate::equalizer;
use crate::recorder::CacheState;
use crate::settings::Settings;
use crate::tty;
use crate::visualizer::Visualizer;
use crate::youtube::DownloadState;
use crossterm::Command as CtCommand;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use std::fmt::Write as _;
use std::io;
use unicode_width::UnicodeWidthStr;

/// Bottom rows reserved for the transport block while ASCII video is playing:
/// title, progress, two button rows, status bar.
pub const RESERVED_ROWS: u16 = 6;
/// Width of the volume rail on the main view's right edge, borders included.
const VOL_RAIL_WIDTH: u16 = 6;
/// The cover box, in cells. Three rows is the NOW panel's whole inside, and six columns is
/// about square once a terminal cell's two-to-one shape is taken into account.
pub const COVER_COLS: u16 = 6;
pub const COVER_ROWS: u16 = 3;
/// `--volume-max` in `mpv.rs`: the rail and click math scale to it.
const VOLUME_MAX: f64 = 150.0;
/// Smallest terminal that still renders a scope worth looking at. Under it the visualizer
/// is switched off rather than squeezed into two rows - see [`scope_fits`].
const MIN_SCOPE_COLS: u16 = 44;
const MIN_SCOPE_ROWS: u16 = 20;
/// Smallest terminal that still gets a centre panel at all. The queue survives a much
/// tighter box than a visualizer does - a two-row list is still a usable list - so the
/// panel and the scope have separate thresholds.
const MIN_PANE_COLS: u16 = 24;
const MIN_PANE_ROWS: u16 = 14;
/// Rows ASCII video needs for a picture, on top of the transport block below it.
const MIN_PICTURE_ROWS: u16 = 8;
const MIN_VIDEO_COLS: u16 = 44;

/// Whether the visualizer is worth running at this terminal size. False switches the
/// feature off - no sampling, no scope pane - instead of drawing a sliver.
pub fn scope_fits(cols: u16, rows: u16) -> bool {
    cols >= MIN_SCOPE_COLS && rows >= MIN_SCOPE_ROWS
}

/// Whether the main view has room for its centre panel (the queue / scope / video pane).
/// False gives those rows to NOW instead of drawing a one-line box.
pub fn pane_fits(cols: u16, rows: u16) -> bool {
    cols >= MIN_PANE_COLS && rows >= MIN_PANE_ROWS
}

/// Whether ASCII video can produce a picture here. False disables video entirely: mpv is
/// never asked for a `tct` output that would have nowhere to land.
pub fn video_fits(cols: u16, rows: u16) -> bool {
    cols >= MIN_VIDEO_COLS && rows >= RESERVED_ROWS + MIN_PICTURE_ROWS
}

// ---------------------------------------------------------------------------
// Theme: one palette, so the look is decided in exactly one place
// ---------------------------------------------------------------------------

/// Primary accent: electric cyan. Keys, live values, the filled bar.
const ACCENT: Color = Color::Rgb(0, 220, 255);
/// Secondary accent: hot magenta. Edit mode, peaks, the brand glyph.
const ACCENT2: Color = Color::Rgb(255, 80, 220);
/// Good news: phosphor green.
const OK: Color = Color::Rgb(80, 250, 150);
const WARN: Color = Color::Rgb(WARN_RGB.0, WARN_RGB.1, WARN_RGB.2);
const WARN_RGB: (u8, u8, u8) = (255, 200, 90);
const ERR: Color = Color::Rgb(255, 95, 110);
/// Readable secondary text.
const DIM: Color = Color::Rgb(130, 140, 150);
/// Structure: borders, rules, dead pixels. Barely-there grey-blue.
const FAINT: Color = Color::Rgb(58, 66, 76);
/// A track that will never play. Dark enough to read as absence rather than as an alarm -
/// [`ERR`] is for something that has gone wrong and wants attention, and a dead entry in a
/// long queue wants none: it wants to be skipped over by the eye as it is by the player.
const DEAD: Color = Color::Rgb(DEAD_RGB.0, DEAD_RGB.1, DEAD_RGB.2);
const DEAD_RGB: (u8, u8, u8) = (122, 48, 56);
/// Titles and loud text.
const BRIGHT: Color = Color::Rgb(235, 240, 245);

fn dim() -> Style {
    Style::new().fg(DIM)
}

fn faint() -> Style {
    Style::new().fg(FAINT)
}

fn accent() -> Style {
    Style::new().fg(ACCENT)
}

/// A bordered panel with a `╸TAG╺`-style title. The one chrome builder every view uses.
fn panel(tag: &str) -> Block<'_> {
    Block::bordered()
        .border_style(faint())
        .title(Line::from(vec![
            Span::styled("╸", Style::new().fg(ACCENT2)),
            Span::styled(
                tag.to_string(),
                Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("╺", Style::new().fg(ACCENT2)),
        ]))
}

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Everything a key or a click can do. One vocabulary for both, so what the user sees
/// labelled `(d) Download` and what a click on it does can't drift apart.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    TogglePause,
    /// Seek to this 0.0..=1.0 fraction of the track.
    SeekTo(f64),
    SeekBack,
    SeekForward,
    VolumeUp,
    VolumeDown,
    /// Set the volume to an absolute percentage (0..=150), from the volume rail.
    VolumeSet(f64),
    Next,
    Prev,
    /// Step the centre panel to the next pane - what a click on it does.
    CyclePane,
    /// Step the scope pane to the next visualizer style.
    CycleScope,
    /// Open the view chooser (`v`).
    OpenViewMenu,
    /// Move the view chooser's cursor to row `i` and show that pane.
    MenuRow(usize),
    /// Main view: download (or cancel downloading) the current track.
    Download,
    /// Queue pane: download every entry (or cancel the batch).
    DownloadAll,
    OpenSettings,
    /// Open the effects menu.
    OpenEffects,
    /// Reopen whatever the last session was playing, where it left off.
    ResumeLast,
    /// Open the "add a link" prompt.
    OpenPrompt,
    /// Submit the active prompt (the `(Enter) Play` button).
    Submit,
    /// Toggle queue reorder mode.
    ToggleEdit,
    /// Move the selected queue entry up/down (edit mode).
    MoveUp,
    MoveDown,
    CloseView,
    /// Play this entry of the queue.
    JumpTo(usize),
    /// Play row `i` of the library pane next.
    BrowseRow(usize),
    /// Add row `i` to the end of the queue instead.
    BrowseAppend(usize),
    /// Start typing a filter into the library pane.
    BrowseFilter,
    /// Ask for a folder to add to the library index.
    AddLibraryRoot,
    /// Select settings row `i` and cycle its value forward.
    SettingsRow(usize),
    /// Select effects row `i` and toggle it.
    EffectRow(usize),
    /// Open the equalizer's view.
    OpenEqualizer,
    /// Select equalizer preset `i` and switch to it.
    EqualizerRow(usize),
    Quit,
}

/// What the main view's centre panel is showing. One panel, three tenants: the queue
/// used to be a full-screen view of its own, the scope used to be the only thing the
/// panel could hold, and video used to be a mode with no relation to either. They are
/// now the same choice, made in one place ([`Action::OpenViewMenu`]) and cycled by
/// clicking the panel.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Pane {
    /// The play queue, inline: rows, cursor, reorder mode.
    Queue,
    /// The live visualizer.
    #[default]
    Scope,
    /// mpv's true-colour ASCII video. Takes the whole screen above the transport
    /// block, so selecting it switches the renderer, not just the panel's contents.
    Video,
    /// Everything on disk and everything played before: one searchable list.
    Library,
}

impl Pane {
    /// The panes in the order the panel cycles through them.
    pub const ALL: [Pane; 4] = [Pane::Queue, Pane::Scope, Pane::Video, Pane::Library];

    pub fn label(self) -> &'static str {
        match self {
            Pane::Queue => "QUEUE",
            Pane::Scope => "SCOPE",
            Pane::Video => "VIDEO",
            Pane::Library => "LIBRARY",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Pane::Queue => "≡",
            Pane::Scope => "∿",
            Pane::Video => "▣",
            Pane::Library => "⌕",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Pane::Queue => 0,
            Pane::Scope => 1,
            Pane::Video => 2,
            Pane::Library => 3,
        }
    }
}

/// One row of the library pane: a track on disk, or something played before.
pub struct BrowseRow<'a> {
    pub title: &'a str,
    /// `artist · album`, or how long ago it was played.
    pub detail: String,
    pub duration: Option<f64>,
    /// Already in the queue, or currently playing.
    pub current: bool,
}

/// Which full-screen view the text mode is showing.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum View {
    #[default]
    Main,
    /// The view chooser: a popup over the main view, so what it is choosing between
    /// stays visible behind it.
    Menu,
    Settings,
    /// The audio effects menu (`e`).
    Effects,
    /// The equalizer's own view (`g`): the preset list, and the shape of the one under
    /// the cursor drawn as bars.
    Equalizer,
    /// The URL bar: a bare launch lands here until something is opened.
    Open,
}

/// Whether the event loop carries on after handling an event.
pub enum Flow {
    Continue,
    Quit,
}

/// Per-entry download state shown in the playlist view.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum EntryStatus {
    #[default]
    None,
    Queued,
    Downloading(f32),
    Done,
    Failed,
}

/// One row of the playlist view.
pub struct EntryRow<'a> {
    pub title: &'a str,
    /// The entry currently playing.
    pub current: bool,
    /// A playable URL is known. False while a Spotify entry is still being matched.
    pub resolved: bool,
    /// Resolution finished and found nothing - the entry will never play.
    pub missing: bool,
    /// False while the shown title is a stand-in and the real one is being fetched.
    pub titled: bool,
    /// The name could not be looked up. The entry may still play perfectly well.
    pub title_failed: bool,
    pub status: EntryStatus,
}

/// A whole-playlist download in flight, as the status bar reports it.
#[derive(Clone, Copy)]
pub struct BatchState {
    /// Entries already saved.
    pub saved: usize,
    pub total: usize,
    /// Progress of the entry currently downloading, when one is.
    pub percent: Option<f32>,
}

/// The URL prompt: either the fullscreen Open view or the inline add-next bar.
#[derive(Default)]
pub struct PromptState {
    pub text: String,
    /// Cursor as a char offset into `text`.
    pub cursor: usize,
    pub error: Option<String>,
    /// A submitted URL is resolving on the worker; input is parked until it lands.
    pub busy: bool,
}

impl PromptState {
    pub fn insert(&mut self, c: char) {
        let at = byte_of(&self.text, self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = byte_of(&self.text, self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = byte_of(&self.text, self.cursor);
            self.text.remove(at);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.chars().count() {
            let at = byte_of(&self.text, self.cursor);
            self.text.remove(at);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }
}

fn byte_of(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// Everything the renderer reads, sampled once per frame by the player.
pub struct UiState<'a> {
    pub view: View,
    /// What the main view's centre panel is showing.
    pub pane: Pane,
    /// The view chooser's cursor, as an index into [`Pane::ALL`].
    pub menu_cursor: usize,
    /// Which panes can actually be shown right now, indexed alongside [`Pane::ALL`].
    /// Decided by the player (an empty queue, a terminal too small for a scope, a track
    /// with no picture), so the chooser greys out exactly what it cannot deliver.
    pub pane_ready: [bool; 4],
    /// Rows for the library pane, already searched and ranked by the player.
    pub browse: &'a [BrowseRow<'a>],
    /// Library pane cursor.
    pub browse_selected: usize,
    /// The live filter, when the pane is in filter mode. `Some("")` is an empty filter
    /// being typed into, which is not the same as no filter at all.
    pub browse_filter: Option<&'a str>,
    /// What the library pane is showing under the hood, for its title: how many tracks
    /// are indexed, or that a scan is running.
    pub library_status: &'a str,
    pub source_label: &'a str,
    pub title: &'a str,
    /// Real tags, empty when the track carries none.
    pub artist: &'a str,
    pub album: &'a str,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub paused: bool,
    /// Nothing is loaded (bare launch, or the queue ran out under `--idle`).
    pub idle: bool,
    pub volume: Option<f64>,
    /// Live level and held peak per channel, `(now, held)` for left then right.
    ///
    /// `None` when there is nothing to meter - paused, idle, or no tap - rather than zeros,
    /// because a meter reading zero says "silence" and an absent one says "not listening",
    /// and those are different things to be told.
    pub meter: Option<[(f32, f32); 2]>,
    /// A picture for what is playing is ready to draw.
    pub cover: bool,
    pub is_playlist: bool,
    /// Current track as an index into `entries`; 0 when there is no playlist.
    pub entry_index: usize,
    pub next_title: Option<&'a str>,
    pub is_loading: bool,
    /// `(resolved, total)` while a background resolver is still matching entries.
    pub resolving: Option<(usize, usize)>,
    /// Two tracks are audible at once: the transition is happening right now.
    pub crossfading: bool,
    /// What is known about the track that is coming next, and how it was found out.
    ///
    /// A transition is built from a measurement taken before anyone can hear the track it
    /// was taken from, which makes it the one part of the automix with no outward sign
    /// that it happened. Saying so is not decoration: it is the difference between a mix
    /// that will land on a bar and one that will not, and it is knowable several seconds
    /// beforehand.
    pub analysis: Option<Analysis>,
    /// That transition was placed on a bar line rather than on the clock.
    pub on_the_beat: bool,
    /// Which style it is running.
    pub transition: &'a str,
    /// The tempo the automix has found, while it is listening for one. Shown because a
    /// feature that silently decides whether to engage is a feature nobody can tell is
    /// working.
    pub bpm: Option<f32>,
    /// mpv has stopped answering. Outranks everything in the status bar except a toast:
    /// a progress bar drawn for a wedged process is a lie, and this is the only place
    /// the user could ever find that out.
    pub stalled: Option<&'a str>,
    pub download: &'a DownloadState,
    /// Title of the track a download or batch is currently pulling, for the status bar.
    pub download_title: Option<&'a str>,
    pub cache: CacheState,
    /// The track playing right now is already saved to disk - outranks the cache line,
    /// which is about to become uninteresting trivia once this is true.
    pub downloaded: bool,
    /// A whole-playlist download in flight.
    pub batch: Option<BatchState>,
    /// False for local sources: the file is already on disk.
    pub download_enabled: bool,
    pub entries: &'a [EntryRow<'a>],
    /// Playlist view cursor.
    pub selected: usize,
    /// Playlist reorder mode.
    pub edit_mode: bool,
    pub settings: &'a Settings,
    pub settings_cursor: usize,
    /// Which of [`effects::ALL`] are enabled, indexed alongside it.
    pub effects_on: &'a [bool],
    pub effects_cursor: usize,
    /// Where the equalizer view's cursor is. Separate from the preset actually in use
    /// (`settings.equalizer`) on purpose: moving down the list previews a shape without
    /// committing to it, so the curve can be read before it is heard.
    pub equalizer_cursor: usize,
    pub quality_locked: bool,
    pub format_locked: bool,
    /// The selected visualizer's name. Always set - "no scope" is now the queue or the
    /// video pane, not a fourth state of this one.
    pub scope_name: &'a str,
    /// The inline add-next prompt (Main) or the fullscreen Open input.
    pub prompt: Option<&'a PromptState>,
    /// What that prompt is asking for, as its title says it.
    pub prompt_tag: &'a str,
    /// A transient one-liner (clipboard queue notices and friends).
    pub toast: Option<&'a str>,
    /// What the last session was playing when it ended, offered on the Open view.
    /// `(title, how long ago)`.
    pub resume: Option<(&'a str, String)>,
    pub term_cols: u16,
    pub term_rows: u16,
}

// ---------------------------------------------------------------------------
// Terminal lifecycle (raw crossterm; shared by both rendering paths)
// ---------------------------------------------------------------------------

/// Restores the terminal on the way out, whatever happened in between.
pub struct TerminalGuard(tty::Terminal);

impl TerminalGuard {
    pub fn new(term: tty::Terminal) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut f = String::new();
        push(&mut f, EnterAlternateScreen);
        push(&mut f, Hide);
        push(&mut f, EnableMouseCapture);
        push(&mut f, EnableBracketedPaste);
        push(&mut f, DisableLineWrap);
        push(&mut f, Clear(ClearType::All));
        term.paint(f.as_bytes())?;
        Ok(TerminalGuard(term))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.0.paint(restore_sequence().as_bytes());
        let _ = disable_raw_mode();
    }
}

/// Everything [`TerminalGuard`] undoes, as one frame.
fn restore_sequence() -> String {
    let mut f = String::new();
    push(&mut f, DisableBracketedPaste);
    push(&mut f, DisableMouseCapture);
    push(&mut f, Show);
    push(&mut f, LeaveAlternateScreen);
    push(&mut f, EnableLineWrap);
    f
}

/// Restore the terminal from *any* thread's panic, then panic normally.
///
/// [`TerminalGuard`]'s `Drop` only covers an unwind on the main thread. The player runs
/// several background threads (tap reader, title/search resolvers, downloads, video
/// forwarder); a panic in one of those - or anywhere before the guard exists - would
/// otherwise leave raw mode and the alternate screen on, and the panic message itself
/// unreadable.
pub fn install_panic_hook(term: tty::Terminal) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = term.paint(restore_sequence().as_bytes());
        previous(info);
    }));
}

/// Re-apply our terminal state after mpv's `tct` output tears itself down.
///
/// Destroying the tct video output makes mpv emit `ESC[?1049l` (leave alt screen),
/// `ESC[?1003l` (disable mouse reporting) and `ESC[?25h` (show cursor), which would
/// otherwise silently break the UI and click handling.
pub fn reassert_terminal(term: &tty::Terminal) -> io::Result<()> {
    let mut f = String::new();
    push(&mut f, EnterAlternateScreen);
    push(&mut f, Hide);
    push(&mut f, EnableMouseCapture);
    push(&mut f, EnableBracketedPaste);
    push(&mut f, DisableLineWrap);
    term.paint(f.as_bytes())
}

pub fn clear_screen(term: &tty::Terminal) -> io::Result<()> {
    let mut f = String::new();
    push(&mut f, MoveTo(0, 0));
    push(&mut f, Clear(ClearType::All));
    term.paint(f.as_bytes())
}

/// Append a crossterm command's ANSI to the frame under construction.
fn push(frame: &mut String, cmd: impl CtCommand) {
    let _ = cmd.write_ansi(frame);
}

// ---------------------------------------------------------------------------
// Ratatui plumbing (text mode)
// ---------------------------------------------------------------------------

/// Buffers one ratatui frame and hands it to the shared terminal writer as a single
/// atomic paint on flush - the same indivisibility contract the video path has.
pub struct FrameWriter {
    term: tty::Terminal,
    buf: Vec<u8>,
}

impl io::Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let frame = std::mem::take(&mut self.buf);
        if !frame.is_empty() {
            self.term.paint(&frame)?;
        }
        Ok(())
    }
}

pub type Tui = ratatui::Terminal<ratatui::backend::CrosstermBackend<FrameWriter>>;

pub fn make_tui(term: tty::Terminal) -> io::Result<Tui> {
    ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(FrameWriter {
        term,
        buf: Vec::new(),
    }))
}

/// What a click lands on. `Bar` resolves to a [`Action::SeekTo`] fraction from the x
/// offset; `VolRail` resolves to a [`Action::VolumeSet`] level from the y offset.
#[derive(Clone, Copy)]
enum Zone {
    Act(Action),
    Bar,
    VolRail,
}

/// The clickable regions of the last drawn frame. Rebuilt on every draw, so hit-testing
/// always matches exactly what is on screen.
#[derive(Default)]
pub struct ClickMap {
    zones: Vec<(Rect, Zone)>,
    /// The queue pane's list area, when one is on screen: where the wheel scrolls rows
    /// instead of turning the volume.
    queue_area: Option<Rect>,
    /// Where the cover picture goes, when there is one to put there.
    ///
    /// The layout decides this and the caller needs it: a picture is not drawn into the
    /// frame like everything else here, it is an escape sequence addressed to absolute
    /// screen coordinates. Handing the rectangle back is what keeps the geometry in one
    /// place - the alternative is the caller working out where the box *should* be, which
    /// is the same sum written twice and wrong the first time either one changes.
    cover_area: Option<Rect>,
}

impl ClickMap {
    fn add(&mut self, rect: Rect, zone: Zone) {
        if rect.width > 0 && rect.height > 0 {
            self.zones.push((rect, zone));
        }
    }

    /// Where the cover picture belongs this frame, in cells, `(col, row, cols, rows)`.
    pub fn cover_area(&self) -> Option<(u16, u16, u16, u16)> {
        self.cover_area.map(|r| (r.x, r.y, r.width, r.height))
    }

    /// True when `(col, row)` is inside a live queue list - the wheel scrolls it there.
    pub fn scrolls_queue(&self, col: u16, row: u16) -> bool {
        self.queue_area
            .is_some_and(|rect| rect.contains(Position { x: col, y: row }))
    }

    pub fn action_at(&self, col: u16, row: u16) -> Option<Action> {
        let at = Position { x: col, y: row };
        // Last drawn wins: panels register their own whole-body zone before the rows and
        // popups they hold, so hit-testing has to read as z-order, not insertion order.
        self.zones
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(at))
            .map(|(rect, zone)| match zone {
                Zone::Act(action) => *action,
                Zone::Bar => {
                    let span = rect.width.saturating_sub(1).max(1);
                    Action::SeekTo(f64::from(col - rect.x) / f64::from(span))
                }
                Zone::VolRail => {
                    let span = rect.height.saturating_sub(1).max(1);
                    let from_bottom = rect.y + rect.height - 1 - row;
                    Action::VolumeSet(VOLUME_MAX * f64::from(from_bottom) / f64::from(span))
                }
            })
    }
}

/// Draw the current text-mode view and hand back its click map. `viz` is the live
/// visualizer to paint into the main view's scope pane, when one is selected.
pub fn draw<B: Backend>(
    tui: &mut ratatui::Terminal<B>,
    state: &UiState,
    viz: Option<(&mut dyn Visualizer, &VizSnapshot)>,
) -> Result<ClickMap, B::Error> {
    let mut map = ClickMap::default();
    let mut viz = viz;
    tui.draw(|frame| render(frame, state, viz.take(), &mut map))?;
    Ok(map)
}

fn render(
    frame: &mut Frame,
    state: &UiState,
    viz: Option<(&mut dyn Visualizer, &VizSnapshot)>,
    map: &mut ClickMap,
) {
    match state.view {
        View::Main => draw_main(frame, state, viz, map),
        View::Menu => {
            // The chooser is a popup, not a replacement: what it is choosing between
            // keeps rendering behind it, so the preview *is* the menu's description.
            draw_main(frame, state, viz, map);
            draw_view_menu(frame, state, map);
        }
        View::Settings => draw_settings(frame, state, map),
        View::Effects => draw_effects(frame, state, map),
        View::Equalizer => draw_equalizer(frame, state, map),
        View::Open => draw_open(frame, state, map),
    }
}

/// One `icon Label (key)` control. Disabled buttons render dim and get no click zone.
struct Btn {
    key: &'static str,
    icon: &'static str,
    label: String,
    action: Action,
    enabled: bool,
    hot: bool,
    /// How readily this control gives up its place when the row is too narrow for
    /// everything. `0` never does; higher goes first. See [`fit_row`].
    drop_order: u8,
    /// A thin break between two groups of related controls, not a control itself: no
    /// key, no icon, no click zone. [`Btn::sep`] is the only way to get one.
    sep: bool,
}

impl Btn {
    fn new(key: &'static str, label: impl Into<String>, action: Action) -> Btn {
        Btn {
            key,
            icon: "",
            label: label.into(),
            action,
            enabled: true,
            hot: false,
            drop_order: 0,
            sep: false,
        }
    }

    /// A visual break between two groups of related controls in the same row - so
    /// "playback" and "volume" and "track" read as three clusters rather than seven
    /// evenly-spaced buttons with nothing to say where one idea ends and the next
    /// starts. Never dropped on a narrow row: a lone divider with nothing on one side
    /// of it is worse clutter than the row staying crowded a little longer.
    fn sep() -> Btn {
        Btn {
            key: "",
            icon: "",
            label: String::new(),
            // Never read: `sep` buttons are never `enabled`, and `button_row` only
            // looks at `action` for an enabled button's click zone.
            action: Action::Quit,
            enabled: false,
            hot: false,
            drop_order: 0,
            sep: true,
        }
    }

    /// Mark this control droppable on a narrow row; higher goes first.
    fn drop_order(mut self, order: u8) -> Btn {
        self.drop_order = order;
        self
    }

    fn icon(mut self, icon: &'static str) -> Btn {
        self.icon = icon;
        self
    }

    fn enabled(mut self, enabled: bool) -> Btn {
        self.enabled = enabled;
        self
    }

    /// Highlight the whole control (an active mode, e.g. edit).
    fn hot(mut self, hot: bool) -> Btn {
        self.hot = hot;
        self
    }

    /// Rendered width in display cells (not chars - `⚡` is two): `icon label (key)`.
    fn width(&self) -> usize {
        if self.sep {
            return 1;
        }
        let icon = if self.icon.is_empty() {
            0
        } else {
            self.icon.width() + 1
        };
        icon + self.label.as_str().width() + 1 + self.key.width() + 2
    }
}

/// The transport row: pause, seek, volume, and track skip on a playlist.
///
/// Shared by both rendering paths - text mode's first keybar row and the block
/// [`video_bottom`] pins under the picture - so the two can't drift apart in which
/// controls they offer or what they're labelled.
fn transport_buttons(state: &UiState) -> Vec<Btn> {
    let pause_label = if state.paused { "Play" } else { "Pause" };
    let pause_icon = if state.paused { "▶" } else { "⏸" };
    // Four zones, playback-first: play/pause and track skip are one idea (what is
    // audible right now), seeking and volume are each their own axis and read better
    // apart from it and from each other.
    let mut transport = vec![
        Btn::new("Space", pause_label, Action::TogglePause)
            .icon(pause_icon)
            .enabled(!state.idle),
    ];
    if state.is_playlist {
        transport.push(Btn::sep());
        transport.push(Btn::new("b", "Prev", Action::Prev).icon("⇤"));
        transport.push(Btn::new("n", "Next", Action::Next).icon("⇥"));
    }
    transport.push(Btn::sep());
    transport.push(
        Btn::new("h", "-5s", Action::SeekBack)
            .icon("«")
            .enabled(!state.idle),
    );
    transport.push(
        Btn::new("l", "+5s", Action::SeekForward)
            .icon("»")
            .enabled(!state.idle),
    );
    transport.push(Btn::sep());
    transport.push(Btn::new("j", "Vol-", Action::VolumeDown).icon("▾"));
    transport.push(Btn::new("k", "Vol+", Action::VolumeUp).icon("▴"));
    transport
}

/// The feature row, built from what the centre panel is currently showing.
///
/// Shared by both rendering paths. The controls that belong to the panel come first,
/// so the row reads left to right as "this panel, then the rest of the player" - and
/// switching the panel switches the controls with it rather than leaving a queue's
/// Edit/Save-all sitting over a visualizer.
fn feature_buttons(state: &UiState, pane_on: bool, width: u16) -> Vec<Btn> {
    let mut features = Vec::new();
    if pane_on && state.pane == Pane::Queue && state.edit_mode {
        // Reordering is a mode, not a mood: its three controls are the row, and none of
        // them is worth dropping while it is on.
        features.push(
            Btn::new("K", "Up", Action::MoveUp)
                .icon("▲")
                .enabled(state.selected > 0),
        );
        features.push(
            Btn::new("J", "Down", Action::MoveDown)
                .icon("▼")
                .enabled(state.selected + 1 < state.entries.len()),
        );
        features.push(
            Btn::new("E", "Done", Action::ToggleEdit)
                .icon("✓")
                .hot(true),
        );
    } else {
        // Zone one: what belongs to the panel on screen right now, so the row reads
        // left to right as "this panel, then the rest of the player".
        features.push(
            Btn::new("o", "Add", Action::OpenPrompt)
                .icon("+")
                .drop_order(50),
        );
        if pane_on && state.pane == Pane::Library {
            features.push(
                Btn::new("/", "Search", Action::BrowseFilter)
                    .icon("⌕")
                    .hot(state.browse_filter.is_some()),
            );
            features.push(
                Btn::new("+", "Queue", Action::BrowseAppend(state.browse_selected))
                    .icon("≡")
                    .enabled(!state.browse.is_empty())
                    .drop_order(12),
            );
            features.push(
                Btn::new("A", "Folder", Action::AddLibraryRoot)
                    .icon("⌂")
                    .drop_order(15),
            );
        }
        if pane_on && state.pane == Pane::Queue {
            features.push(
                Btn::new("E", "Edit", Action::ToggleEdit)
                    .icon("✎")
                    .enabled(state.entries.len() > 1)
                    .drop_order(10),
            );
        }
        // Zone two: downloading, whichever shape it takes here - one track always,
        // the whole queue when this is the pane that has one. Kept together rather
        // than one of them living next to Add and the other next to Quit.
        features.push(Btn::sep());
        if pane_on && state.pane == Pane::Queue {
            // Terse next to `Save`, which is the same verb for one track.
            let (icon, label) = if state.batch.is_some() {
                ("⊘", "Stop")
            } else {
                ("⬇", "All")
            };
            features.push(
                Btn::new("a", label, Action::DownloadAll)
                    .icon(icon)
                    .enabled(state.download_enabled && !state.entries.is_empty())
                    .drop_order(20),
            );
        }
        features.push(download_button(state));
    }
    // Zone three: the panels themselves, always in the same order they cycle in.
    features.push(Btn::sep());
    features.push(view_button(state));
    features.push(
        Btn::new("s", "Setup", Action::OpenSettings)
            .icon("⚙")
            .drop_order(30),
    );
    features.push(
        Btn::new("e", "FX", Action::OpenEffects)
            .icon("♪")
            .hot(state.effects_on.iter().any(|&on| on))
            .drop_order(40),
    );
    features.push(
        Btn::new("g", "EQ", Action::OpenEqualizer)
            // Lit whenever a preset other than `Flat` is on, the same way `FX` is lit
            // for a live effect: what is shaping the sound should be visible without
            // opening the view that shapes it.
            .icon("▤")
            .hot(state.settings.equalizer != 0)
            // Between `FX` and `Setup`: the settings menu has a key everybody already
            // knows, so on a row too narrow for all three it is the one worth losing
            // the button for.
            .drop_order(35),
    );
    // Zone four: leaving. On its own so it is never read as one more feature.
    features.push(Btn::sep());
    features.push(Btn::new("q", "Quit", Action::Quit).icon("✕"));
    fit_row(features, width)
}

/// `(v) ∿ View` - the one way into the chooser, wearing the icon of the pane it is on.
fn view_button(state: &UiState) -> Btn {
    Btn::new("v", "View", Action::OpenViewMenu).icon(state.pane.icon())
}

/// Width of a whole row: the controls plus the minimum two-cell gaps between them.
fn row_width(buttons: &[Btn]) -> usize {
    buttons.iter().map(Btn::width).sum::<usize>() + 2 * buttons.len().saturating_sub(1)
}

/// Drop the least important controls until the row fits `width`.
///
/// [`button_row`] clips on the right when it runs out of room, and the right of a
/// feature row is `(q) ✕ Quit`. Deciding here what to give up - the extras first, the
/// controls that name the panel last - beats letting the terminal width decide it by
/// truncation.
fn fit_row(mut buttons: Vec<Btn>, width: u16) -> Vec<Btn> {
    while row_width(&buttons) > width as usize {
        let victim = buttons
            .iter()
            .enumerate()
            .filter(|(_, b)| b.drop_order > 0)
            // Ties break rightwards, so a row sheds from the tail it would have clipped.
            .max_by_key(|(i, b)| (b.drop_order, *i))
            .map(|(i, _)| i);
        match victim {
            Some(at) => {
                buttons.remove(at);
            }
            None => break,
        }
    }
    drop_stray_separators(buttons)
}

/// A separator with nothing real on one side of it - the edge of the row, or another
/// separator sitting right next to it - is not marking a division between two groups
/// any more, it is clutter [`fit_row`] left behind by dropping everything that side of
/// it had.
fn drop_stray_separators(buttons: Vec<Btn>) -> Vec<Btn> {
    let mut out: Vec<Btn> = Vec::with_capacity(buttons.len());
    for b in buttons {
        if b.sep && out.last().is_none_or(|last| last.sep) {
            continue;
        }
        out.push(b);
    }
    while out.last().is_some_and(|b| b.sep) {
        out.pop();
    }
    out
}

/// Render a row of `(key) icon Label` buttons justified across the whole of `area` -
/// the gaps stretch so the first button starts on the left edge and the last ends on
/// the right - registering a click zone per enabled button.
fn button_row(map: &mut ClickMap, area: Rect, buttons: &[Btn]) -> Line<'static> {
    let total: usize = buttons.iter().map(Btn::width).sum();
    let n = buttons.len();
    let slack = (area.width as usize).saturating_sub(total);
    // Crowded rows fall back to the minimum gap and clip on the right.
    let (gap, extra) = if n > 1 && slack / (n - 1) >= 2 {
        (slack / (n - 1), slack % (n - 1))
    } else {
        (2, 0)
    };

    let mut spans = Vec::new();
    let mut x = area.x;
    for (i, b) in buttons.iter().enumerate() {
        if i > 0 {
            // Hand the division remainder out one cell at a time, left to right.
            let g = gap + usize::from(i <= extra);
            spans.push(Span::raw(" ".repeat(g)));
            x += g as u16;
        }
        if b.sep {
            spans.push(Span::styled("│", faint()));
            x += b.width() as u16;
            continue;
        }
        let (key_st, icon_st, label_st) = if b.hot {
            (
                Style::new().fg(ACCENT2).add_modifier(Modifier::BOLD),
                Style::new().fg(ACCENT2).add_modifier(Modifier::BOLD),
                Style::new().fg(ACCENT2),
            )
        } else if b.enabled {
            (
                accent(),
                Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
                Style::new().fg(BRIGHT),
            )
        } else {
            (faint(), faint(), faint())
        };
        if !b.icon.is_empty() {
            spans.push(Span::styled(b.icon, icon_st));
            spans.push(Span::styled(format!(" {}", b.label), label_st));
        } else {
            spans.push(Span::styled(b.label.clone(), label_st));
        }
        spans.push(Span::styled(format!(" ({})", b.key), key_st));
        if b.enabled {
            map.add(
                Rect {
                    x,
                    y: area.y,
                    width: b.width() as u16,
                    height: 1,
                },
                Zone::Act(b.action),
            );
        }
        x += b.width() as u16;
    }
    Line::from(spans)
}

/// `03:45` for a known number of seconds. The same clock the progress row shows, so a
/// message about a position and the bar under it agree to the second.
pub fn clock(seconds: f64) -> String {
    format_time(Some(seconds))
}

fn format_time(seconds: Option<f64>) -> String {
    let seconds = seconds
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(0.0)
        .round() as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn bar_cells(position: Option<f64>, duration: Option<f64>, width: usize) -> usize {
    match (position, duration) {
        (Some(p), Some(d)) if d > 0.0 => {
            ((width as f64) * p / d).round().clamp(0.0, width as f64) as usize
        }
        _ => 0,
    }
}

/// `01:23 ▰▰▰▰▱▱▱▱ 03:45` - the clock and the clickable bar.
fn progress_row(map: &mut ClickMap, area: Rect, state: &UiState) -> Line<'static> {
    let pos = format_time(state.position);
    let dur = format_time(state.duration);
    let overhead = pos.chars().count() + 1 + 1 + dur.chars().count();
    let width = (area.width as usize).saturating_sub(overhead).max(4);
    let filled = bar_cells(state.position, state.duration, width);

    let bar_x = area.x + pos.chars().count() as u16 + 1;
    map.add(
        Rect {
            x: bar_x,
            y: area.y,
            width: width as u16,
            height: 1,
        },
        Zone::Bar,
    );

    Line::from(vec![
        Span::styled(pos, dim()),
        Span::raw(" "),
        Span::styled("▰".repeat(filled), accent()),
        Span::styled("▱".repeat(width - filled), faint()),
        Span::raw(" "),
        Span::styled(dur, dim()),
    ])
}

/// A ten-cell `▰▰▰▱▱▱▱▱▱▱` gauge for the status bar.
fn mini_bar(percent: f32) -> [Span<'static>; 2] {
    let filled = ((percent / 100.0) * 10.0).round().clamp(0.0, 10.0) as usize;
    [
        Span::styled("▰".repeat(filled), accent()),
        Span::styled("▱".repeat(10 - filled), faint()),
    ]
}

/// Left side of the always-on status bar, in priority order: an explicit toast beats a
/// batch download beats a single download beats the loading hint beats resolver progress
/// beats the cache/stream state. Never empty - the bar is a fixture, not a message line.
fn status_spans(state: &UiState) -> Vec<Span<'static>> {
    if let Some(toast) = state.toast {
        return vec![Span::styled(toast.to_string(), Style::new().fg(ACCENT2))];
    }
    if let Some(why) = state.stalled {
        return vec![
            Span::styled(
                "⚠ MPV NOT RESPONDING",
                Style::new().fg(ERR).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {why}"), dim()),
        ];
    }
    if let Some(batch) = &state.batch {
        return match batch.percent {
            Some(pct) => {
                let mut spans = vec![Span::styled(
                    format!(
                        "⬇ QUEUE {}/{}",
                        (batch.saved + 1).min(batch.total),
                        batch.total
                    ),
                    accent().add_modifier(Modifier::BOLD),
                )];
                spans.push(Span::styled(format!(" · {pct:>3.0}% "), accent()));
                spans.extend(mini_bar(pct));
                if let Some(title) = state.download_title {
                    spans.push(Span::styled(format!(" {title}"), Style::new().fg(BRIGHT)));
                }
                spans
            }
            None => vec![
                Span::styled(
                    format!("⬇ QUEUE {}/{}", batch.saved, batch.total),
                    accent().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · waiting…", dim()),
            ],
        };
    }
    match state.download {
        DownloadState::Running { percent } => {
            let mut spans = vec![Span::styled(
                format!("⬇ {percent:>3.0}% "),
                accent().add_modifier(Modifier::BOLD),
            )];
            spans.extend(mini_bar(*percent));
            if let Some(title) = state.download_title {
                spans.push(Span::styled(format!(" {title}"), Style::new().fg(BRIGHT)));
            }
            return spans;
        }
        DownloadState::Done { path } => {
            return vec![Span::styled(format!("✓ SAVED {path}"), Style::new().fg(OK))];
        }
        DownloadState::Failed { message } => {
            return vec![Span::styled(
                format!("✗ FAILED: {message}"),
                Style::new().fg(ERR),
            )];
        }
        DownloadState::Cancelled => {
            return vec![Span::styled("⊘ CANCELLED", Style::new().fg(WARN))];
        }
        DownloadState::Idle => {}
    }
    if state.crossfading {
        // The one moment two decks are playing at once - worth saying out loud, since
        // the whole point of the setting is audible only for a few seconds a track, and
        // worth naming the style, since the user chose it and cannot otherwise tell
        // whether the beat grid was trusted this time.
        let next = state.next_title.unwrap_or_default();
        let mut spans = vec![Span::styled(
            format!("⇄ {}", state.transition.to_uppercase()),
            accent().add_modifier(Modifier::BOLD),
        )];
        if state.on_the_beat {
            spans.push(Span::styled(" ♪ ON THE BEAT", Style::new().fg(OK)));
        }
        if let Some(bpm) = state.bpm {
            spans.push(Span::styled(format!(" · {bpm:.0} BPM"), dim()));
        }
        spans.push(Span::styled(format!(" ▸ {next}"), Style::new().fg(BRIGHT)));
        return spans;
    }
    if let Some(analysis) = state
        .analysis
        .filter(|a| !matches!(a, Analysis::Ready { .. }))
    {
        let next = state.next_title.unwrap_or_default();
        return match analysis {
            Analysis::Reading => vec![
                Span::styled("◈ ANALYSING ", Style::new().fg(ACCENT2)),
                Span::styled(next.to_string(), Style::new().fg(BRIGHT)),
                Span::styled(" · reading its beat with ffmpeg", dim()),
            ],
            Analysis::Matched { bpm, stretch } if stretch.abs() >= 0.001 => vec![
                Span::styled("◈ ", Style::new().fg(OK)),
                Span::styled(format!("{bpm:.1} BPM"), Style::new().fg(OK)),
                Span::styled(format!(" ▸ {next}"), Style::new().fg(BRIGHT)),
                Span::styled(
                    format!(" · pulled {:+.1}% and locked", stretch * 100.0),
                    dim(),
                ),
            ],
            Analysis::Matched { bpm, .. } => vec![
                Span::styled("◈ ", Style::new().fg(OK)),
                Span::styled(format!("{bpm:.1} BPM"), Style::new().fg(OK)),
                Span::styled(format!(" ▸ {next}"), Style::new().fg(BRIGHT)),
                Span::styled(" · already in time", dim()),
            ],
            Analysis::Free { bpm } => vec![
                Span::styled("◈ ", dim()),
                Span::styled(format!("{bpm:.1} BPM"), Style::new().fg(OK)),
                Span::styled(format!(" ▸ {next}"), Style::new().fg(BRIGHT)),
                Span::styled(" · this transition does not match tempo", dim()),
            ],
            Analysis::Unreadable => vec![
                Span::styled("◈ ", Style::new().fg(WARN)),
                Span::styled(next.to_string(), Style::new().fg(BRIGHT)),
                Span::styled(
                    " · no steady beat found; the mix will run on a timer",
                    dim(),
                ),
            ],
            Analysis::Ready { .. } => unreachable!("filtered above"),
        };
    }
    if state.is_loading {
        let next = state.next_title.unwrap_or_default();
        return vec![Span::styled(
            format!("⇥ LOADING {next}"),
            Style::new().fg(ACCENT2),
        )];
    }
    if let Some((done, total)) = state.resolving {
        return vec![Span::styled(format!("⌕ MATCHING {done}/{total}"), dim())];
    }
    // The automix listening for a tempo in the run-up to a transition. Worth showing:
    // it is the difference between "it will land on the beat" and "it will not", and
    // there is otherwise no way to know which until it happens.
    if let Some(bpm) = state.bpm {
        return vec![
            Span::styled("♪ ", Style::new().fg(OK)),
            Span::styled(format!("{bpm:.0} BPM"), Style::new().fg(OK)),
            Span::styled(" · the next transition can land on a bar", dim()),
        ];
    }
    // The next track was measured long before anything needs it. Below the live line,
    // because the run-up owns the moments that matter; the rest of the track this is the
    // answer to "did the analysis work", visible while there is still time to care.
    if let Some(Analysis::Ready { bpm }) = state.analysis {
        let next = state.next_title.unwrap_or_default();
        return vec![
            Span::styled("◈ ", Style::new().fg(OK)),
            Span::styled(format!("{bpm:.1} BPM"), Style::new().fg(OK)),
            Span::styled(format!(" ▸ {next}"), Style::new().fg(BRIGHT)),
            Span::styled(" · analysed, ready to mix", dim()),
        ];
    }
    if state.downloaded {
        return vec![Span::styled(
            "⬇ DOWNLOADED",
            Style::new().fg(OK).add_modifier(Modifier::BOLD),
        )];
    }
    match state.cache {
        CacheState::Ready => vec![Span::styled("⚡ CACHED", Style::new().fg(OK))],
        CacheState::Buffering => vec![Span::styled("⋯ CACHING", dim())],
        CacheState::Partial => vec![Span::styled("◐ PARTIAL CACHE", Style::new().fg(WARN))],
        CacheState::Off if state.idle => vec![Span::styled("◌ IDLE", faint())],
        CacheState::Off => vec![Span::styled("∿ STREAMING", dim())],
    }
}

/// One line spanning `width` cells: `left`, a styled filler, `right` flush against the
/// right edge. When the parts don't fit side by side, `right` is dropped and `left`
/// clips at the edge.
fn justified(
    width: u16,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    fill: &str,
    fill_style: Style,
) -> Line<'static> {
    let cells = |spans: &[Span]| {
        spans
            .iter()
            .map(|s| s.content.as_ref().width())
            .sum::<usize>()
    };
    let (lw, rw) = (cells(&left), cells(&right));
    let mut spans = left;
    if lw + rw + 4 <= width as usize {
        let n = width as usize - lw - rw - 2;
        spans.push(Span::styled(format!(" {} ", fill.repeat(n)), fill_style));
        spans.extend(right);
    }
    Line::from(spans)
}

/// The always-on bottom status bar: transfer state on the left, source and track count
/// on the right, a faint rule between so the row reaches both edges.
fn status_bar(width: u16, state: &UiState) -> Line<'static> {
    let mut right = vec![Span::styled(
        state.source_label.to_uppercase(),
        dim().add_modifier(Modifier::BOLD),
    )];
    if state.is_playlist {
        right.push(Span::styled(
            format!(" · {} TRACKS", state.entries.len()),
            dim(),
        ));
    }
    justified(width, status_spans(state), right, "─", faint())
}

/// The rule that separates the keybar - controls, something a hand reaches for - from
/// the status line under it, which is read, not pressed. Same weight and the same
/// character as every other line in the chrome, `BRIGHT` rather than `faint()` because
/// this is the one border that is not a panel's edge: it marks the whole player's own
/// split between "do something" and "here is what is happening".
fn status_rule(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::new().fg(BRIGHT),
    ))
}

fn download_button(state: &UiState) -> Btn {
    let busy = state.batch.is_some() || matches!(state.download, DownloadState::Running { .. });
    let (icon, label) = if busy {
        ("⊘", "Cancel")
    } else {
        ("⬇", "Save")
    };
    Btn::new("d", label, Action::Download)
        .icon(icon)
        .enabled(state.download_enabled && !state.idle)
}

// ---------------------------------------------------------------------------
// Main view
// ---------------------------------------------------------------------------

/// The transport state chip: `▶ PLAYING` / `⏸ PAUSED` / `◌ IDLE`.
/// Text for the "now playing" row.
///
/// mpv publishes no `media-title` until it has actually opened the stream, so a track
/// that is still being cached would otherwise render as a lone `▸` - and in video mode,
/// with no picture above it yet, that leaves a blank pane between the border and the
/// status bar. Fall back to the state we do know.
fn now_playing_text(state: &UiState) -> String {
    if !state.title.trim().is_empty() {
        return state.title.to_string();
    }
    match state.cache {
        CacheState::Buffering => "caching…".to_string(),
        _ => "loading…".to_string(),
    }
}

/// `♪ Artist · Album`, or nothing when the track carries no tags.
///
/// A music player that knows who made the thing it is playing and does not say so is an
/// odd object; one that invents an answer is a worse one, so an untagged track gets
/// silence here rather than a guess.
fn credit_spans(state: &UiState) -> Vec<Span<'static>> {
    let credit = match (state.artist.trim(), state.album.trim()) {
        ("", "") => return Vec::new(),
        (artist, "") => artist.to_string(),
        ("", album) => album.to_string(),
        (artist, album) => format!("{artist} · {album}"),
    };
    vec![Span::styled("♪ ", faint()), Span::styled(credit, dim())]
}

fn state_chip(state: &UiState) -> Span<'static> {
    if state.idle {
        Span::styled("◌ IDLE", faint().add_modifier(Modifier::BOLD))
    } else if state.paused {
        Span::styled(
            "⏸ PAUSED",
            Style::new().fg(WARN).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "▶ PLAYING",
            Style::new().fg(OK).add_modifier(Modifier::BOLD),
        )
    }
}

/// `▛▞ YTM://PLAYER ── [SOURCE]` on the left, transport state chip on the right.
fn header_line(state: &UiState) -> Line<'static> {
    Line::from(vec![
        Span::styled("▛▞ ", Style::new().fg(ACCENT2).add_modifier(Modifier::BOLD)),
        Span::styled(
            "YTM://PLAYER",
            Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ── ", faint()),
        Span::styled(format!("[{}]", state.source_label.to_uppercase()), accent()),
        Span::raw("  "),
        state_chip(state),
    ])
}

/// The full-height volume rail: a segmented gauge, clickable and wheel-scrollable,
/// with the 100% line marked. Returns nothing; paints straight into the buffer.
fn draw_vol_rail(frame: &mut Frame, area: Rect, state: &UiState, map: &mut ClickMap) {
    // A plain bordered block: the rail is 5 cells wide, so the title has exactly
    // three cells - "VOL" fits, the decorated `panel()` tag would not.
    let block = Block::bordered().border_style(faint()).title(Span::styled(
        "VOL",
        Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 3 || inner.width == 0 {
        return;
    }
    // Bottom row is the number; the gauge lives above it.
    let gauge = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height - 1,
    };
    map.add(gauge, Zone::VolRail);

    let vol = state.volume.unwrap_or(0.0).clamp(0.0, VOLUME_MAX);
    let level = vol / VOLUME_MAX;
    let h = f64::from(gauge.height);
    let filled_rows = (level * h).round() as u16;
    let hundred_row = ((100.0 / VOLUME_MAX) * h).round() as u16;

    // The rail carries two different things and they must not be confused for each other:
    // on the left what the volume is set to, which only moves when it is moved, and on the
    // right what is actually coming out, which moves constantly. A mixer keeps them side by
    // side for the same reason - one is an instruction and the other is a consequence, and
    // the interesting moments are the ones where they disagree.
    let meter = state.meter.filter(|_| gauge.width >= 4);
    let bar_width = if meter.is_some() { 1 } else { 2 };

    let buf = frame.buffer_mut();
    for row in 0..gauge.height {
        let y = gauge.y + gauge.height - 1 - row;
        let filled = row < filled_rows;
        let frac = f32::from(row) / f32::from(gauge.height.max(1));
        let color = if !filled {
            FAINT
        } else if vol > 100.0 && f64::from(row) >= f64::from(hundred_row) - 0.5 {
            ERR
        } else {
            // Reuse the accent, warmer toward the top.
            Color::Rgb(0, (200.0 + 55.0 * frac) as u8, 255)
        };
        let ch = if filled {
            "██"
        } else if row + 1 == hundred_row {
            "──" // the 100% line
        } else {
            "╌╌"
        };
        for (k, c) in ch
            .chars()
            .take(bar_width.min(gauge.width as usize))
            .enumerate()
        {
            buf[(gauge.x + k as u16, y)].set_char(c).set_fg(color);
        }
    }
    if let Some(meter) = meter {
        draw_meter(
            buf,
            Rect {
                x: gauge.x + gauge.width - 2,
                width: 2,
                ..gauge
            },
            meter,
        );
    }
    let label = format!("{vol:>3.0}");
    let y = inner.y + inner.height - 1;
    for (k, c) in label.chars().take(inner.width as usize).enumerate() {
        frame.buffer_mut()[(inner.x + k as u16, y)]
            .set_char(c)
            .set_fg(DIM);
    }
}

/// Two columns of peak meter, left channel then right.
///
/// Read from the bottom like every meter on every mixer: a solid column to where the level
/// is now, and a single mark left behind at the highest it has been lately. The colour
/// changes where it matters rather than gradually - green while there is headroom, amber
/// where a mix starts to crowd, red at the top - because a meter is read at a glance and a
/// gradient carries no threshold to glance at.
fn draw_meter(buf: &mut Buffer, area: Rect, meter: [(f32, f32); 2]) {
    /// Where amber starts and where red does, as a fraction of the scale. Chosen against
    /// what the tap reports rather than a dB figure: it normalises to a floor, so these are
    /// positions on that scale and not decibels.
    const WARM: f32 = 0.72;
    const HOT: f32 = 0.90;

    for (channel, (level, hold)) in meter.iter().copied().enumerate() {
        let x = area.x + channel as u16;
        if x >= area.x + area.width {
            break;
        }
        let rows = f32::from(area.height);
        let lit = (level.clamp(0.0, 1.0) * rows).round() as u16;
        // Half a row up, so the mark sits *on* the level it is reporting rather than under
        // it - a peak hold that reads low is worse than none, because it is believed.
        let mark = ((hold.clamp(0.0, 1.0) * rows).round() as u16).clamp(1, area.height);
        for row in 0..area.height {
            let y = area.y + area.height - 1 - row;
            let frac = f32::from(row) / rows;
            let hue = if frac >= HOT {
                ERR
            } else if frac >= WARM {
                WARN
            } else {
                OK
            };
            let (ch, fg) = if row + 1 == mark {
                ('━', hue)
            } else if row < lit {
                ('█', hue)
            } else {
                ('·', FAINT)
            };
            buf[(x, y)].set_char(ch).set_fg(fg);
        }
    }
}

/// A resting centre panel: a dotted grid with a centred hint, so an empty panel still
/// looks like an instrument, not a bug.
fn draw_pane_idle(frame: &mut Frame, inner: Rect, hint: &str) {
    let buf = frame.buffer_mut();
    for row in 0..inner.height {
        for col in 0..inner.width {
            if row % 2 == 1 && col % 4 == 2 {
                buf[(inner.x + col, inner.y + row)]
                    .set_char('·')
                    .set_fg(FAINT);
            }
        }
    }
    if inner.width as usize > hint.chars().count() && inner.height >= 1 {
        let x = inner.x + (inner.width - hint.chars().count() as u16) / 2;
        let y = inner.y + inner.height / 2;
        for (k, c) in hint.chars().enumerate() {
            buf[(x + k as u16, y)].set_char(c).set_fg(DIM);
        }
    }
}

fn draw_main(
    frame: &mut Frame,
    state: &UiState,
    viz: Option<(&mut dyn Visualizer, &VizSnapshot)>,
    map: &mut ClickMap,
) {
    let area = frame.area();
    if area.width < 20 || area.height < 8 {
        frame.render_widget(Paragraph::new("terminal too small"), area);
        return;
    }
    let [main_col, rail_col] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(VOL_RAIL_WIDTH)]).areas(area);

    draw_vol_rail(frame, rail_col, state, map);

    // Prompt swaps in above the keybar; everything else keeps its place.
    let prompt_rows = if state.prompt.is_some() { 3 } else { 0 };
    // Below the threshold there is no centre panel at all, squeezed to nothing: its rows
    // go to NOW.
    let pane_on = pane_fits(state.term_cols, state.term_rows);
    let (now_c, pane_c) = if pane_on {
        (Constraint::Length(5), Constraint::Min(4))
    } else {
        (Constraint::Min(5), Constraint::Length(0))
    };
    let [header_a, now_a, pane_a, prompt_a, keys_a, rule_a, act_a] = Layout::vertical([
        Constraint::Length(1),
        now_c,
        pane_c,
        Constraint::Length(prompt_rows),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(main_col);

    frame.render_widget(Paragraph::new(header_line(state)), header_a);
    map.add(header_a, Zone::Act(Action::TogglePause));

    // -- NOW panel: title, meta, progress --------------------------------------------
    let now = panel("NOW");
    let mut now_inner = now.inner(now_a);
    frame.render_widget(now, now_a);
    // A cover, when there is one and there is room. Everything to the right of it lays
    // itself out in what is left, so a track with no picture gets the full width it has
    // always had rather than a permanent empty square.
    if state.cover && now_inner.height >= COVER_ROWS && now_inner.width > COVER_COLS + 24 {
        let [art_a, rest] =
            Layout::horizontal([Constraint::Length(COVER_COLS), Constraint::Min(20)])
                .areas(now_inner);
        map.cover_area = Some(Rect {
            height: COVER_ROWS,
            ..art_a
        });
        // Left blank for the picture to land on. Nothing is drawn here, so the frame
        // diffing that follows will not paint over it either.
        now_inner = Rect {
            x: rest.x + 1,
            width: rest.width.saturating_sub(1),
            ..rest
        };
    }
    if now_inner.height >= 2 {
        let [title_a, bar_a] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)])
            .areas(Rect {
                height: now_inner.height.min(2),
                ..now_inner
            });
        let title = if state.idle {
            Line::from(Span::styled("─ nothing loaded · (o) open a link ─", dim()))
        } else {
            let mut spans = vec![
                Span::styled("▸ ", Style::new().fg(ACCENT2)),
                Span::styled(
                    now_playing_text(state),
                    Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
                ),
            ];
            if state.is_playlist {
                spans.push(Span::styled(
                    format!("  {}/{}", state.entry_index + 1, state.entries.len()),
                    dim(),
                ));
            }
            Line::from(spans)
        };
        frame.render_widget(Paragraph::new(title), title_a);
        frame.render_widget(Paragraph::new(progress_row(map, bar_a, state)), bar_a);

        // Meta line: who it is by, and what plays next. Deliberately *not* a click
        // target - it sits one row under the seek bar, and a mis-aimed seek that swapped
        // the centre panel out from under the pointer is worse than no shortcut at all.
        if now_inner.height >= 3 {
            let meta_a = Rect {
                y: now_inner.y + 2,
                height: 1,
                ..now_inner
            };
            let mut left = credit_spans(state);
            let next = match state.next_title {
                Some(next) => Some(next.to_string()),
                None if state.is_playlist => Some("─ end of queue ─".to_string()),
                None => None,
            };
            match (left.is_empty(), next) {
                (true, None) => {}
                (_, next) => {
                    let right = next
                        .map(|next| {
                            vec![Span::styled("NEXT ▸ ", faint()), Span::styled(next, dim())]
                        })
                        .unwrap_or_default();
                    if left.is_empty() {
                        left = right;
                        frame.render_widget(Paragraph::new(Line::from(left)), meta_a);
                    } else {
                        // Both: the credit on the left, what follows flush right, so the
                        // two never run into each other on a narrow terminal.
                        frame.render_widget(
                            Paragraph::new(justified(meta_a.width, left, right, " ", faint())),
                            meta_a,
                        );
                    }
                }
            }
        }
    }

    // -- centre panel: queue, scope or video (dropped when the terminal is too small) ---
    if pane_on {
        draw_pane(frame, pane_a, state, viz, map);
    }

    // -- prompt (add a link) -------------------------------------------------------------
    if let Some(prompt) = state.prompt {
        draw_prompt_box(frame, prompt_a, prompt, state.prompt_tag);
    }

    // -- keybar ---------------------------------------------------------------------------
    let [keys1_a, keys2_a] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(keys_a);
    frame.render_widget(
        Paragraph::new(button_row(map, keys1_a, &transport_buttons(state))),
        keys1_a,
    );

    frame.render_widget(
        Paragraph::new(button_row(
            map,
            keys2_a,
            &feature_buttons(state, pane_on, keys2_a.width),
        )),
        keys2_a,
    );

    // -- always-on status bar ---------------------------------------------------------
    frame.render_widget(Paragraph::new(status_rule(rule_a.width)), rule_a);
    frame.render_widget(Paragraph::new(status_bar(act_a.width, state)), act_a);
}

// ---------------------------------------------------------------------------
// The centre panel: one box, three tenants
// ---------------------------------------------------------------------------

/// The main view's centre panel.
///
/// The queue used to be a full-screen view you left the player to visit, the scope used
/// to be the only thing this box could hold, and video used to be an unrelated mode.
/// They are one choice now: the box is titled `VIEW ▸ …`, its whole body cycles to the
/// next pane on a click, and anything the pane draws on top of that (queue rows) wins by
/// z-order.
fn draw_pane(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    viz: Option<(&mut dyn Visualizer, &VizSnapshot)>,
    map: &mut ClickMap,
) {
    let mut tag = format!("VIEW ▸ {}", state.pane.label());
    match state.pane {
        Pane::Queue => {
            let _ = write!(tag, " · {} TRACKS", state.entries.len());
            let saved = state
                .entries
                .iter()
                .filter(|e| e.status == EntryStatus::Done)
                .count();
            if saved > 0 {
                let _ = write!(tag, " · {saved} SAVED");
            }
            if let Some((done, total)) = state.resolving {
                let _ = write!(tag, " · MATCHING {done}/{total}");
            }
            if state.edit_mode {
                let _ = write!(
                    tag,
                    " · EDIT ▸ {}/{}",
                    state.selected + 1,
                    state.entries.len()
                );
            }
        }
        Pane::Scope => {
            let _ = write!(tag, " · {}", state.scope_name);
        }
        Pane::Video => {}
        Pane::Library => {
            let _ = write!(tag, " · {}", state.library_status);
            if let Some(filter) = state.browse_filter {
                let _ = write!(tag, " · ⌕ {filter}▏");
            }
        }
    }

    let live = (state.pane == Pane::Queue && state.edit_mode)
        || (state.pane == Pane::Library && state.browse_filter.is_some());
    let block = if live {
        panel(&tag).border_style(Style::new().fg(ACCENT2))
    } else {
        panel(&tag)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Clicking the panel is how the panel changes.
    map.add(area, Zone::Act(Action::CyclePane));
    if inner.width < 2 || inner.height < 1 {
        return;
    }

    match state.pane {
        Pane::Queue if state.entries.is_empty() => {
            draw_pane_idle(frame, inner, "( o ) add something to play")
        }
        Pane::Queue => draw_queue(frame, inner, state, map),
        Pane::Scope => match viz {
            Some((vizzer, snap)) => vizzer.render(snap, inner, frame.buffer_mut()),
            // No tap, or a terminal too small to sample one.
            None => draw_pane_idle(frame, inner, "( v ) choose a view"),
        },
        Pane::Library if state.browse.is_empty() => draw_pane_idle(
            frame,
            inner,
            match state.browse_filter {
                Some(_) => "nothing matches",
                None => "( / ) search  ·  nothing indexed yet",
            },
        ),
        Pane::Library => draw_browse(frame, inner, state, map),
        Pane::Video => {
            // Only reachable with the picture *not* running: video mode paints through
            // `video_bottom` instead of this panel.
            let hint = if state.idle {
                "nothing to show"
            } else if !video_fits(state.term_cols, state.term_rows) {
                "terminal too small for video"
            } else {
                "( v ) start the picture"
            };
            draw_pane_idle(frame, inner, hint)
        }
    }
}

/// The queue, inline: rows scrolled to keep the cursor centred, one click zone each.
fn draw_queue(frame: &mut Frame, area: Rect, state: &UiState, map: &mut ClickMap) {
    // Remembered so the wheel scrolls the list when the pointer is over it, and the
    // volume when it is not.
    map.queue_area = Some(area);

    let height = area.height as usize;
    let offset = state
        .selected
        .saturating_sub(height / 2)
        .min(state.entries.len().saturating_sub(height));
    let end = (offset + height).min(state.entries.len());
    let index_width = state.entries.len().to_string().len();

    let mut lines = Vec::new();
    for (row, i) in (offset..end).enumerate() {
        let entry = &state.entries[i];
        let selected = i == state.selected;

        let marker = if entry.current {
            "▶ "
        } else if selected && state.edit_mode {
            "↕ "
        } else {
            "  "
        };
        let (glyph, glyph_style) = match entry.status {
            EntryStatus::Done => ("  ✓ ".to_string(), Style::new().fg(OK)),
            EntryStatus::Downloading(pct) => (format!("{pct:>3.0}% "), accent()),
            EntryStatus::Queued => ("  · ".to_string(), Style::new().fg(WARN)),
            EntryStatus::Failed => ("  ✗ ".to_string(), Style::new().fg(ERR)),
            EntryStatus::None => ("    ".to_string(), Style::new()),
        };

        // Two different failures, and they matter differently. An entry that will never
        // play is dead weight and reads as such: dimmed, in a red dark enough to be read
        // as absence rather than as an alarm. An entry whose *name* could not be looked up
        // still plays perfectly well - it is only that nobody knows what it is called - so
        // it stays legible and is marked in amber, the colour this interface already uses
        // for "there is something not quite right here".
        let title_style = if entry.missing {
            Style::new().fg(DEAD)
        } else if entry.current {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else if entry.title_failed {
            Style::new().fg(WARN)
        } else if !entry.resolved || !entry.titled {
            dim()
        } else {
            Style::new().fg(BRIGHT)
        };
        let suffix = if entry.missing {
            " ✗ unplayable"
        } else if !entry.resolved {
            " ⌕ matching…"
        } else if entry.title_failed {
            " ✗ no name"
        } else if !entry.titled {
            " ⋯"
        } else {
            ""
        };
        let suffix_style = if entry.missing {
            Style::new().fg(DEAD)
        } else if entry.title_failed {
            Style::new().fg(WARN)
        } else {
            faint()
        };

        let row_style = if selected && state.edit_mode {
            Style::new().fg(Color::Black).bg(ACCENT2)
        } else if selected {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        lines.push(
            Line::from(vec![
                Span::styled(marker, Style::new().fg(OK)),
                Span::styled(format!("{:>index_width$} ", i + 1), dim()),
                Span::styled(glyph, glyph_style),
                Span::styled(entry.title.to_string(), title_style),
                Span::styled(suffix, suffix_style),
            ])
            .style(row_style),
        );

        map.add(
            Rect {
                x: area.x,
                y: area.y + row as u16,
                width: area.width,
                height: 1,
            },
            Zone::Act(Action::JumpTo(i)),
        );
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The library pane: one line per track, the same shape as the queue so the eye does not
/// have to re-learn the panel when it changes what it is listing.
fn draw_browse(frame: &mut Frame, area: Rect, state: &UiState, map: &mut ClickMap) {
    map.queue_area = Some(area);
    let height = area.height as usize;
    let offset = state
        .browse_selected
        .saturating_sub(height / 2)
        .min(state.browse.len().saturating_sub(height));
    let end = (offset + height).min(state.browse.len());

    let mut lines = Vec::new();
    for (row, i) in (offset..end).enumerate() {
        let entry = &state.browse[i];
        let selected = i == state.browse_selected;
        let time = entry
            .duration
            .filter(|d| *d > 0.0)
            .map(|d| format_time(Some(d)))
            .unwrap_or_else(|| "  ·  ".to_string());
        // Title gets whatever the detail and clock do not; both are clipped rather than
        // wrapped, because a list that reflows as you scroll is unreadable.
        let detail_width = (area.width as usize / 3).clamp(0, 40);
        let title_width = (area.width as usize).saturating_sub(detail_width + time.width() + 5);
        let title_style = if entry.current {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(BRIGHT)
        };
        let row_style = if selected {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        lines.push(
            Line::from(vec![
                Span::styled(if entry.current { "▶ " } else { "  " }, Style::new().fg(OK)),
                Span::styled(
                    format!(
                        "{:<w$}",
                        truncate(entry.title, title_width),
                        w = title_width
                    ),
                    title_style,
                ),
                Span::styled(
                    format!(
                        " {:<w$}",
                        truncate(&entry.detail, detail_width),
                        w = detail_width
                    ),
                    dim(),
                ),
                Span::styled(format!(" {time}"), faint()),
            ])
            .style(row_style),
        );
        map.add(
            Rect {
                x: area.x,
                y: area.y + row as u16,
                width: area.width,
                height: 1,
            },
            Zone::Act(Action::BrowseRow(i)),
        );
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The equalizer view: the presets on the left, and the shape of whichever one the
/// cursor is on drawn as bars on the right.
///
/// The drawing is the point of having a view at all. A preset is five numbers, and five
/// numbers in a row tell a listener nothing; the same five as a curve say "this one is
/// scooped" or "this one is a smile" at a glance, and the cursor previews that shape
/// without committing to it - so a preset can be read before it is heard, and the one
/// actually in use is marked separately.
fn draw_equalizer(frame: &mut Frame, state: &UiState, map: &mut ClickMap) {
    let block = panel("EQUALIZER");
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());
    if inner.width < 24 || inner.height < 6 {
        return;
    }
    // Two rows held back at the bottom: the note for the preset under the cursor, and
    // the hints. Taken off the top of the split so neither column can run into them.
    let body = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    let list_width = (inner.width / 2).clamp(18, 34);
    let [list_a, curve_a] = Layout::horizontal([
        Constraint::Length(list_width),
        Constraint::Min(CURVE_MIN_COLS),
    ])
    .areas(body);

    let cursor_at = state
        .equalizer_cursor
        .min(equalizer::ALL.len().saturating_sub(1));
    let mut lines = Vec::new();
    for (i, preset) in equalizer::ALL.iter().enumerate() {
        if i as u16 >= list_a.height {
            break;
        }
        let in_use = i == state.settings.equalizer;
        let selected = i == cursor_at;
        let name_style = if in_use {
            accent().add_modifier(Modifier::BOLD)
        } else if selected {
            Style::new().fg(BRIGHT)
        } else {
            dim()
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, Style::new().fg(ACCENT2)),
            // `●` is "this is what you are hearing", which is not the same question as
            // where the cursor is - and on this view both need answering at once.
            Span::styled(if in_use { "● " } else { "  " }, accent()),
            Span::styled(preset.name.to_string(), name_style),
        ]));
        map.add(
            Rect {
                x: list_a.x,
                y: list_a.y + i as u16,
                width: list_a.width,
                height: 1,
            },
            Zone::Act(Action::EqualizerRow(i)),
        );
    }
    frame.render_widget(Paragraph::new(lines), list_a);

    if curve_a.width >= CURVE_MIN_COLS {
        frame.render_widget(
            Paragraph::new(curve_lines(equalizer::at(cursor_at), curve_a.height)),
            curve_a,
        );
    }

    let note_a = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(2),
        width: inner.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            equalizer::at(cursor_at).note.to_string(),
            dim(),
        ))),
        note_a,
    );

    let hints_a = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        ..note_a
    };
    let hints = button_row(
        map,
        hints_a,
        &[
            Btn::new("Enter", "Use", Action::EqualizerRow(usize::MAX))
                .icon("●")
                .enabled(false),
            Btn::new("j/k", "Move", Action::CloseView)
                .icon("↕")
                .enabled(false),
            Btn::new("g", "Back", Action::CloseView).icon("←"),
        ],
    );
    frame.render_widget(Paragraph::new(hints), hints_a);
}

/// Narrowest the curve can be drawn and still mean anything: five bands at five columns
/// each (a three-wide bar and the gap that keeps `3.5k` off `10k`), plus the
/// `+12`/`0`/`-12` gutter down the left.
const CURVE_MIN_COLS: u16 = 5 * BAND_COLS as u16 + 5;

/// Columns per band: the bar, then the gap that separates it from the next one.
const BAR_COLS: usize = 3;
const BAND_COLS: usize = BAR_COLS + 2;

/// The preset's five gains as a bar chart about a zero line.
///
/// Half-blocks rather than whole ones: a band is drawn to the nearest half row, so `+2`
/// and `+4` are visibly different on a chart four rows tall instead of rounding to the
/// same bar. Boosts and cuts get different colours because the shape is read faster than
/// the numbers are.
fn curve_lines(preset: &equalizer::Preset, height: u16) -> Vec<Line<'static>> {
    // One row for the frequency labels, the rest split evenly either side of zero.
    let usable = height.saturating_sub(1).max(3);
    let arms = ((usable - 1) / 2).clamp(1, 6);
    let step = f64::from(equalizer::MAX_GAIN) / f64::from(arms);
    let gutter = |db: i32| format!("{db:>3} ");

    let mut lines: Vec<Line<'static>> = Vec::new();
    for row in 0..=(arms * 2) {
        // Rows above the axis count down from `+MAX`, rows below count on past it.
        let above = row < arms;
        let axis = row == arms;
        let label = match row {
            0 => gutter(i32::from(equalizer::MAX_GAIN)),
            r if r == arms => gutter(0),
            r if r == arms * 2 => gutter(-i32::from(equalizer::MAX_GAIN)),
            _ => "    ".to_string(),
        };
        let mut spans = vec![Span::styled(label, faint())];
        spans.push(Span::styled(if axis { "┼" } else { "┤" }, faint()));
        for gain in preset.gains {
            // How far this band reaches from the axis, counted in half-rows so that a
            // `+2` and a `+4` are visibly different on a chart six rows tall.
            let halves = (f64::from(gain).abs() / step * 2.0).round() as u16;
            // `distance` is 1 for the row touching the axis, counting outwards. A row is
            // solid once the bar has reached the far side of it, and half-filled while
            // the bar is somewhere inside it.
            let cell = |distance: u16| -> &'static str {
                match halves.saturating_sub((distance - 1) * 2) {
                    0 => " ",
                    // The drawn half is the one nearest the axis, so the bar grows out
                    // of the line rather than floating above it.
                    1 => {
                        if above {
                            "▄"
                        } else {
                            "▀"
                        }
                    }
                    _ => "█",
                }
            };
            let bar = if axis {
                // The axis is drawn through every band, and a band that moves at all
                // meets it - so a bar is joined to the line it is measured from.
                if gain == 0 { "─" } else { "█" }
            } else if above && gain > 0 {
                cell(arms - row)
            } else if !above && gain < 0 {
                cell(row - arms)
            } else {
                " "
            };
            let style = if gain > 0 {
                Style::new().fg(OK)
            } else if gain < 0 {
                Style::new().fg(ACCENT2)
            } else {
                faint()
            };
            spans.push(Span::styled(bar.repeat(BAR_COLS), style));
            spans.push(Span::styled(
                if axis { "──" } else { "  " }.to_string(),
                faint(),
            ));
        }
        lines.push(Line::from(spans));
    }
    // Frequencies under their own bars, in the same columns the bars occupy.
    let mut labels = vec![Span::styled(" ".repeat(5), faint())];
    for band in equalizer::BANDS {
        labels.push(Span::styled(
            format!("{:<w$}", band.label, w = BAND_COLS),
            dim(),
        ));
    }
    lines.push(Line::from(labels));
    lines
}

/// One row of the view chooser: the pane, its current variant, a note, and whether it
/// can be shown here at all.
fn menu_rows(state: &UiState) -> [(Pane, String, &'static str, bool); 4] {
    [
        (
            Pane::Queue,
            format!("{} tracks", state.entries.len()),
            "what plays next, in the panel",
            state.pane_ready[0],
        ),
        (
            Pane::Scope,
            state.scope_name.to_string(),
            "live audio · (h/l) restyles",
            state.pane_ready[1],
        ),
        (
            Pane::Video,
            state.settings.quality.label().to_string(),
            // The picture belongs to one deck, so the decks cannot trade places under it.
            if state.settings.crossfade {
                "fullscreen ASCII · no overlap while it is up"
            } else {
                "true-colour ASCII, fullscreen"
            },
            state.pane_ready[2],
        ),
        (
            Pane::Library,
            state.library_status.to_string(),
            "your music and what you played",
            state.pane_ready[3],
        ),
    ]
}

/// The view chooser (`v`): a popup over the main view, so the thing being chosen for is
/// still on screen behind it.
fn draw_view_menu(frame: &mut Frame, state: &UiState, map: &mut ClickMap) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).clamp(20, 62);
    let height = 12.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    // Nothing of the view underneath shows through a popup.
    frame.render_widget(ratatui::widgets::Clear, popup);
    let block = panel("VIEW").border_style(accent());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width < 8 || inner.height < 2 {
        return;
    }

    let rows = menu_rows(state);
    let mut lines = Vec::new();
    for (i, (pane, value, note, enabled)) in rows.iter().enumerate() {
        let cursor = if state.menu_cursor == i { "▸ " } else { "  " };
        // Wide enough for the longest label, so the value column stays a column.
        let name = format!("{} {:<8}", pane.icon(), pane.label());
        let value_text = format!("‹ {value} ›");
        let (name_style, value_style) = if !enabled {
            (faint(), faint())
        } else if *pane == state.pane {
            (
                Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
                accent().add_modifier(Modifier::BOLD),
            )
        } else {
            (Style::new().fg(BRIGHT), dim())
        };
        lines.push(Line::from(vec![
            Span::styled(cursor.to_string(), Style::new().fg(ACCENT2)),
            Span::styled(name, name_style),
            Span::styled(format!("{value_text:<15}"), value_style),
            Span::styled(format!(" {note}"), faint()),
        ]));
        lines.push(Line::raw(""));
        if *enabled {
            map.add(
                Rect {
                    x: inner.x,
                    y: inner.y + (i * 2) as u16,
                    width: inner.width,
                    height: 1,
                },
                Zone::Act(Action::MenuRow(i)),
            );
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);

    let hints_row = (rows.len() * 2) as u16;
    if inner.height > hints_row {
        let hints_a = Rect {
            x: inner.x,
            y: inner.y + hints_row,
            width: inner.width,
            height: 1,
        };
        let hints = fit_row(
            vec![
                Btn::new("Enter", "Show", Action::MenuRow(state.menu_cursor)).icon("▶"),
                Btn::new("h/l", "Style", Action::CycleScope)
                    .icon("∿")
                    .enabled(scope_fits(state.term_cols, state.term_rows))
                    .drop_order(10),
                Btn::new("v", "Back", Action::CloseView).icon("←"),
            ],
            hints_a.width,
        );
        let hints = button_row(map, hints_a, &hints);
        frame.render_widget(Paragraph::new(hints), hints_a);
    }
}

/// The bordered single-line text input with cursor, shared by Open and add-next.
fn draw_prompt_box(frame: &mut Frame, area: Rect, prompt: &PromptState, tag: &str) {
    if area.height == 0 {
        return;
    }
    let block = panel(tag).border_style(accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    let status = if prompt.busy {
        Span::styled("  ◌ resolving…", Style::new().fg(WARN))
    } else if let Some(err) = &prompt.error {
        Span::styled(format!("  ✗ {err}"), Style::new().fg(ERR))
    } else {
        Span::raw("")
    };

    // Keep the cursor visible: show the tail when the text overflows.
    let budget = (inner.width as usize).saturating_sub(3 + status.content.chars().count());
    let chars: Vec<char> = prompt.text.chars().collect();
    let (shown, cursor_at) = if chars.len() + 1 > budget {
        let skip = chars.len() + 1 - budget;
        (
            chars[skip.min(chars.len())..].iter().collect::<String>(),
            prompt.cursor.saturating_sub(skip),
        )
    } else {
        (prompt.text.clone(), prompt.cursor)
    };
    let shown_chars: Vec<char> = shown.chars().collect();
    let before: String = shown_chars[..cursor_at.min(shown_chars.len())]
        .iter()
        .collect();
    let at: String = shown_chars
        .get(cursor_at)
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".to_string());
    let after: String = if cursor_at < shown_chars.len() {
        shown_chars[(cursor_at + 1).min(shown_chars.len())..]
            .iter()
            .collect()
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled("▸ ", Style::new().fg(ACCENT2)),
        Span::styled(before, Style::new().fg(BRIGHT)),
        Span::styled(at, Style::new().fg(Color::Black).bg(ACCENT)),
        Span::styled(after, Style::new().fg(BRIGHT)),
        status,
    ]);
    frame.render_widget(Paragraph::new(line), inner);
}

// ---------------------------------------------------------------------------
// Open view (the URL bar)
// ---------------------------------------------------------------------------

fn draw_open(frame: &mut Frame, state: &UiState, map: &mut ClickMap) {
    let area = frame.area();
    let default_prompt = PromptState::default();
    let prompt = state.prompt.unwrap_or(&default_prompt);

    // Vertically centred column, comfortably narrow.
    let width = area.width.clamp(20, 72);
    let x = area.x + (area.width - width) / 2;
    let top = area.height.saturating_sub(9) / 2;

    let brand_a = Rect::new(x, area.y + top, width, 2);
    let input_a = Rect::new(x, area.y + top + 3, width, 3);
    let hints_a = Rect::new(x, area.y + top + 7, width, 1);

    let brand = vec![
        Line::from(vec![
            Span::styled("▛▞ ", Style::new().fg(ACCENT2).add_modifier(Modifier::BOLD)),
            Span::styled(
                "YTM://PLAYER",
                Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled("paste a link or a local path", dim())),
    ];
    frame.render_widget(Paragraph::new(brand), brand_a);

    draw_prompt_box(frame, input_a, prompt, "OPEN");

    let mut hints = vec![Btn::new("Enter", "Play", Action::Submit).icon("▶")];
    if state.resume.is_some() {
        hints.push(Btn::new("Tab", "Resume", Action::ResumeLast).icon("↺"));
    }
    hints.push(Btn::new("Esc", "Quit", Action::Quit).icon("✕"));
    let hints = button_row(map, hints_a, &fit_row(hints, hints_a.width));
    frame.render_widget(Paragraph::new(hints), hints_a);

    if area.height > top + 8 {
        let providers_a = Rect::new(x, area.y + top + 8, width, 1);
        // The resume offer replaces the provider list: it is the more useful of the two
        // and there is one line for both.
        let line = match &state.resume {
            Some((title, when)) => Line::from(vec![
                Span::styled("↺ ", Style::new().fg(ACCENT2)),
                Span::styled(truncate(title, width.saturating_sub(24) as usize), dim()),
                Span::styled(format!(" · {when}"), faint()),
            ]),
            None => Line::from(Span::styled(
                "youtube · soundcloud · spotify · file · folder · or just type a search",
                faint(),
            )),
        };
        frame.render_widget(Paragraph::new(line), providers_a);
    }
}

/// Clip `text` to `width` display cells, with an ellipsis when it did not fit.
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width || width == 0 {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.to_string().width();
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Settings view
// ---------------------------------------------------------------------------

/// Which settings row, by name rather than by position.
///
/// The rows used to be a list to draw and a `match` on indices to act on, which are two
/// orderings of the same thing and therefore two orderings that can disagree. Inserting
/// `Beat mixing` in the middle did exactly that: every row below it kept its own label and
/// picked up its neighbour's action, so the new row cycled the transition and the
/// transition row did nothing at all. Naming them means the list can be reordered and
/// nothing has to be renumbered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Setting {
    Quality,
    Format,
    TagDownloads,
    Shuffle,
    Repeat,
    Normalize,
    SmartLoading,
    Clipboard,
    Crossfade,
    FadeLength,
    FadeCurve,
    BeatMixing,
    Transition,
}

impl Setting {
    /// Top to bottom, as drawn. The one place the order is written down.
    ///
    /// The fade rows sit together and in the order a fade is described: whether there is
    /// one, how long it is, what shape it takes - and only then the scored machinery and
    /// which score it runs, which are the two rows that need beat mixing on to mean
    /// anything. The curve deliberately sits above that line, because it is the one part
    /// of a transition's character available with beat mixing off.
    pub const ALL: [Setting; 13] = [
        Setting::Quality,
        Setting::Format,
        Setting::TagDownloads,
        Setting::Shuffle,
        Setting::Repeat,
        Setting::Normalize,
        Setting::SmartLoading,
        Setting::Clipboard,
        Setting::Crossfade,
        Setting::FadeLength,
        Setting::FadeCurve,
        Setting::BeatMixing,
        Setting::Transition,
    ];

    /// The row at `cursor`, if the cursor is on one.
    pub fn at(cursor: usize) -> Option<Setting> {
        Setting::ALL.get(cursor).copied()
    }
}

/// How many rows the settings menu has. Taken from the list itself, so it cannot be a
/// count of something else.
pub const SETTING_ROWS: usize = Setting::ALL.len();

/// What the automix has found out about the track it is about to bring in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Analysis {
    /// ffmpeg is decoding a slice of it, on a thread of its own.
    Reading,
    /// Measured, and the arriving deck will be pulled by `stretch` to match. A stretch of
    /// zero means the two are already together.
    Matched { bpm: f64, stretch: f64 },
    /// Measured, but this transition does not ask for a tempo match - or the two tracks
    /// are too far apart for one to be honest.
    Free { bpm: f64 },
    /// Nothing usable came back. The transition still runs; it runs on a timer.
    Unreadable,
    /// Measured well ahead of time; the transition is not even cued yet.
    Ready { bpm: f64 },
}

/// The note is owned rather than borrowed because one of them has numbers in it - the fade
/// length says where the join falls, and that moves with the score.
///
/// The settings rows: which row it is, its name, its current value, whether it's locked,
/// and the note shown.
///
/// The identity travels with the label so the two are written on one line and cannot be
/// separated by an edit somewhere else.
fn setting_rows(state: &UiState) -> [(Setting, &'static str, String, bool, String); SETTING_ROWS] {
    let quality_note = if state.quality_locked {
        "audio-only source — nothing to pick".to_string()
    } else {
        "height for the video pane and MP4 saves".to_string()
    };
    let format_note = if !state.download_enabled && !state.idle {
        "local files are already on disk".to_string()
    } else if state.format_locked {
        "SoundCloud & Spotify always save MP3".to_string()
    } else {
        "audio-only sources still save MP3".to_string()
    };
    let fade_note = if !state.settings.crossfade {
        "enable crossfade first".to_string()
    } else {
        // Say where the join falls, because the number alone is ambiguous: a fifteen
        // second fade is not fifteen seconds of overlap, it is a fifteen second move with
        // the end of the old track somewhere inside it, and the score decides where.
        // Where the curve really puts the join, not where the score writes it: a `Late`
        // curve moves it, and a number that did not move with it would be a lie about
        // the very thing this note exists to explain.
        let split = state
            .settings
            .transition
            .out_end_at(state.settings.fade_curve);
        let secs = f64::from(state.settings.crossfade_secs);
        return_split_note(secs * split, secs * (1.0 - split))
    };
    [
        (
            Setting::Quality,
            "Quality",
            state.settings.quality.label().to_string(),
            state.quality_locked,
            quality_note.to_string(),
        ),
        (
            Setting::Format,
            "Save format",
            state.settings.format.label().to_string(),
            state.format_locked,
            format_note.to_string(),
        ),
        (
            Setting::TagDownloads,
            "Tag saves",
            onoff(state.settings.tag_downloads),
            false,
            "write title/artist and cover art into saved files".to_string(),
        ),
        (
            Setting::Shuffle,
            "Shuffle",
            onoff(state.settings.shuffle),
            !state.is_playlist,
            "play the queue in a random order".to_string(),
        ),
        (
            Setting::Repeat,
            "Repeat",
            state.settings.repeat.label().to_string(),
            false,
            "what happens at the end of a track or the queue".to_string(),
        ),
        (
            Setting::Normalize,
            "Normalise",
            state.settings.normalize.label().to_string(),
            false,
            "level tracks from their ReplayGain tags".to_string(),
        ),
        (
            Setting::SmartLoading,
            "Smart loading",
            onoff(state.settings.smart_loading),
            false,
            "playlists start after the first track resolves".to_string(),
        ),
        (
            Setting::Clipboard,
            "Clipboard watch",
            onoff(state.settings.clipboard_watch),
            false,
            "queue links you copy anywhere on the system".to_string(),
        ),
        (
            Setting::Crossfade,
            "Crossfade",
            onoff(state.settings.crossfade),
            false,
            "overlap tracks on auto-advance (2nd decoder)".to_string(),
        ),
        (
            Setting::FadeLength,
            "Fade length",
            {
                // `20 s ▸ join 13.5` rather than `20 s = 13.5+6.5`: the second form reads
                // as two tracks - "13.5 of the old, 6.5 of the new" - when it is one span
                // with the old track's end inside it. Naming the join says which.
                let secs = f64::from(state.settings.crossfade_secs);
                let before = secs
                    * state
                        .settings
                        .transition
                        .out_end_at(state.settings.fade_curve);
                format!("{secs:.0} s ▸ join {before:.1}")
            },
            !state.settings.crossfade,
            fade_note,
        ),
        (
            Setting::FadeCurve,
            "Fade curve",
            state.settings.fade_curve.label().to_string(),
            !state.settings.crossfade,
            if state.settings.crossfade {
                state.settings.fade_curve.note().to_string()
            } else {
                "enable crossfade first".to_string()
            },
        ),
        (
            Setting::BeatMixing,
            "Beat mixing",
            onoff(state.settings.beat_mixing),
            !state.settings.crossfade,
            if !state.settings.crossfade {
                "enable crossfade first".to_string()
            } else if state.settings.beat_mixing {
                // Say the cost out loud where the choice is made, not in a manual.
                "match tempo, lock the bars, run the score · no video".to_string()
            } else {
                "off: a clean fade, no analysis, video still works".to_string()
            },
        ),
        (
            Setting::Transition,
            "Transition",
            state.settings.transition.label().to_string(),
            !state.settings.crossfade || !state.settings.beat_mixing,
            if !state.settings.crossfade {
                "enable crossfade first".to_string()
            } else if !state.settings.beat_mixing {
                "turn on beat mixing to use a score".to_string()
            } else if state.settings.transition.origin().is_empty() {
                state.settings.transition.note().to_string()
            } else {
                // Say when a transition is the user's own file rather than a built-in, so
                // it is obvious which of them can be edited to change what is heard.
                "yours · ~/.config/ytmplayer/transitions".to_string()
            },
        ),
    ]
}

/// `3–30 s total, split either side of the join` - the point of the number, said once.
///
/// The bounds are read from [`settings`] rather than written out here. They have been
/// changed twice; a copy of them in the interface is a copy that is right until it is not,
/// and the settings row is the one place a user goes to find out what the range is.
fn return_split_note(before: f64, after: f64) -> String {
    let range = format!(
        "{}\u{2013}{} s",
        crate::settings::CROSSFADE_MIN,
        crate::settings::CROSSFADE_MAX
    );
    // Says which track each number belongs to. The old wording ("split either side of
    // the join") and the old value (`20 s = 13.5+6.5`) both read as "13.5 seconds of the
    // outgoing track plus 6.5 of the incoming one", so a listener checking the new track
    // after a 20 s move expected to find it 6.5 seconds in and found it 20 - and
    // reasonably concluded the setting was being ignored. It was not: the numbers are
    // two parts of one span, not two tracks.
    if after < 0.5 {
        format!("{range} · the whole move lands before the old track ends")
    } else if before < 0.5 {
        format!("{range} · almost all of it after the old track has gone")
    } else {
        format!("{range} · both audible for {before:.1} s, then {after:.1} s on the new one alone")
    }
}

fn onoff(on: bool) -> String {
    if on { "On" } else { "Off" }.to_string()
}

fn draw_settings(frame: &mut Frame, state: &UiState, map: &mut ClickMap) {
    let block = panel("SETTINGS");
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());
    if inner.width < 8 || inner.height < 2 {
        return;
    }

    let rows = setting_rows(state);
    // Double-spacing reads better and there used to be room for it at six settings.
    // There is not at ten, so the airy layout is what gets dropped first - before the
    // hints, and long before any row goes off the bottom.
    let step: u16 = if inner.height >= (rows.len() as u16) * 2 + 4 {
        2
    } else {
        1
    };
    // Still too tall (a very short terminal): scroll, keeping the cursor in view.
    let body = inner.height.saturating_sub(3).max(1);
    let visible = (body / step).max(1) as usize;
    let first = state
        .settings_cursor
        .saturating_sub(visible.saturating_sub(1));
    let last = (first + visible).min(rows.len());

    let mut lines = Vec::new();
    for (i, (_, name, value, locked, note)) in rows.iter().enumerate().take(last).skip(first) {
        let cursor = if state.settings_cursor == i {
            "▸ "
        } else {
            "  "
        };
        let value_text = format!("‹ {value} ›");
        let value_style = if *locked {
            faint()
        } else {
            accent().add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::styled(cursor.to_string(), Style::new().fg(ACCENT2)),
            Span::styled(format!("{name:<16}"), Style::new().fg(BRIGHT)),
            // Wide enough for the longest value ("MP3 (audio)" in guillemets), so the
            // notes column stays a column.
            Span::styled(format!("{value_text:<16}"), value_style),
            Span::styled(format!("  {note}"), dim()),
        ]));
        map.add(
            Rect {
                x: inner.x,
                y: inner.y + (i - first) as u16 * step,
                width: inner.width,
                height: 1,
            },
            Zone::Act(Action::SettingsRow(i)),
        );
        if step == 2 {
            lines.push(Line::raw(""));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);

    let hints_row = (last - first) as u16 * step;
    if inner.height > hints_row + 1 {
        let hints_a = Rect {
            x: inner.x,
            y: inner.y + hints_row + u16::from(step == 1),
            width: inner.width,
            height: 1,
        };
        let hints = button_row(
            map,
            hints_a,
            &fit_row(
                vec![
                    Btn::new("Enter", "Change", Action::SettingsRow(usize::MAX))
                        .icon("↺")
                        .enabled(false)
                        .drop_order(20),
                    Btn::new("j/k", "Move", Action::CloseView)
                        .icon("↕")
                        .enabled(false)
                        .drop_order(10),
                    Btn::new("s", "Back", Action::CloseView).icon("←"),
                ],
                hints_a.width,
            ),
        );
        frame.render_widget(Paragraph::new(hints), hints_a);
        if inner.height > hints_row + 4 {
            let note_a = Rect {
                x: inner.x,
                y: inner.y + hints_row + 3,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::styled("saved to ~/.config/ytmplayer/config", faint())),
                note_a,
            );
        }
    }
    if inner.height > hints_row + 2 {
        let status_a = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(status_bar(status_a.width, state)), status_a);
    }
}

fn draw_effects(frame: &mut Frame, state: &UiState, map: &mut ClickMap) {
    let block = panel("EFFECTS");
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());
    if inner.width < 8 || inner.height < 2 {
        return;
    }

    // Double-spaced while the rack fits, single-spaced and scrolled once it does not.
    // The rack is long enough now that the airy layout is the first thing to give, the
    // same trade the settings menu makes - and past that it scrolls, keeping the cursor
    // in view, rather than letting the terminal decide by clipping.
    let body = inner.height.saturating_sub(2).max(1) as usize;
    let step = if effects::ALL.len() * 2 <= body { 2 } else { 1 };
    let visible = (body / step).max(1);
    let offset = state
        .effects_cursor
        .saturating_sub(visible / 2)
        .min(effects::ALL.len().saturating_sub(visible));
    let end = (offset + visible).min(effects::ALL.len());

    let mut lines = Vec::new();
    for (row, i) in (offset..end).enumerate() {
        let effect = &effects::ALL[i];
        let on = state.effects_on.get(i).copied().unwrap_or(false);
        let cursor = if state.effects_cursor == i {
            "▸ "
        } else {
            "  "
        };
        let value_text = format!("‹ {} ›", if on { "On" } else { "Off" });
        let value_style = if on {
            accent().add_modifier(Modifier::BOLD)
        } else {
            faint()
        };
        lines.push(Line::from(vec![
            Span::styled(cursor.to_string(), Style::new().fg(ACCENT2)),
            Span::styled(format!("{:<16}", effect.name), Style::new().fg(BRIGHT)),
            Span::styled(format!("{value_text:<12}"), value_style),
            Span::styled(format!("  {}", effect.note), dim()),
        ]));
        map.add(
            Rect {
                x: inner.x,
                y: inner.y + (row * step) as u16,
                width: inner.width,
                height: 1,
            },
            Zone::Act(Action::EffectRow(i)),
        );
        if step == 2 {
            lines.push(Line::raw(""));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);

    let hints_row = ((end - offset) * step + 1) as u16;
    if inner.height > hints_row {
        let hints_a = Rect {
            x: inner.x,
            y: inner.y + hints_row,
            width: inner.width,
            height: 1,
        };
        let hints = button_row(
            map,
            hints_a,
            &[
                Btn::new("Enter", "Toggle", Action::EffectRow(usize::MAX))
                    .icon("↺")
                    .enabled(false),
                Btn::new("j/k", "Move", Action::CloseView)
                    .icon("↕")
                    .enabled(false),
                Btn::new("e", "Back", Action::CloseView).icon("←"),
            ],
        );
        frame.render_widget(Paragraph::new(hints), hints_a);
        if inner.height > hints_row + 2 {
            let note_a = Rect {
                x: inner.x,
                y: inner.y + hints_row + 2,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "effects layer onto the playing audio — scopes follow; saves stay clean",
                    faint(),
                )),
                note_a,
            );
        }
    }
    if inner.height > hints_row + 4 {
        let status_a = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(status_bar(status_a.width, state)), status_a);
    }
}

// ---------------------------------------------------------------------------
// Video mode (mpv's tct output owns the top rows; we own the bottom RESERVED_ROWS)
// ---------------------------------------------------------------------------

/// First row of the transport block pinned under the picture.
pub fn bar_row(term_rows: u16) -> u16 {
    term_rows.saturating_sub(RESERVED_ROWS)
}

/// Rows available to mpv's `tct` renderer, i.e. everything above the reserved block.
pub fn video_rows(term_rows: u16) -> u16 {
    term_rows.saturating_sub(RESERVED_ROWS).max(1)
}

/// The transport block for video mode: the same widgets text mode uses - title line,
/// progress row, two button rows, status bar - rendered into an off-screen [`Buffer`]
/// and serialized to ANSI positioned over the reserved rows only. One builder set for
/// both modes, one [`ClickMap`] for both hit-tests: nothing left to drift.
pub fn video_bottom(state: &UiState) -> (String, ClickMap) {
    let mut map = ClickMap::default();
    if state.term_rows <= RESERVED_ROWS || state.term_cols == 0 {
        return (String::new(), map);
    }
    let w = state.term_cols;
    let y0 = bar_row(state.term_rows);
    let mut buf = Buffer::empty(Rect::new(0, y0, w, RESERVED_ROWS));
    let row = |i: u16| Rect::new(0, y0 + i, w, 1);

    // Clicking the picture is the most natural play/pause target.
    map.add(Rect::new(0, 0, w, y0), Zone::Act(Action::TogglePause));

    // Title row: what's playing on the left, the transport chip on the right.
    let mut left = vec![
        Span::styled("▸ ", Style::new().fg(ACCENT2)),
        Span::styled(
            now_playing_text(state),
            Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
        ),
    ];
    if state.is_playlist {
        left.push(Span::styled(
            format!("  {}/{}", state.entry_index + 1, state.entries.len()),
            dim(),
        ));
    }
    let title = justified(w, left, vec![state_chip(state)], " ", faint());
    Paragraph::new(title).render(row(0), &mut buf);

    Paragraph::new(progress_row(&mut map, row(1), state)).render(row(1), &mut buf);

    Paragraph::new(button_row(&mut map, row(2), &transport_buttons(state)))
        .render(row(2), &mut buf);

    // No panel to configure here - the picture *is* the pane - so the row is the
    // player's own controls plus the one way back out of them. Same three zones as
    // the text view's feature row, minus the panel-specific one there is no panel
    // to be specific about.
    let features = fit_row(
        vec![
            download_button(state),
            Btn::sep(),
            view_button(state),
            Btn::new("s", "Setup", Action::OpenSettings)
                .icon("⚙")
                .drop_order(30),
            Btn::new("e", "FX", Action::OpenEffects)
                .icon("♪")
                .hot(state.effects_on.iter().any(|&on| on))
                .drop_order(40),
            Btn::sep(),
            Btn::new("q", "Quit", Action::Quit).icon("✕"),
        ],
        w,
    );
    Paragraph::new(button_row(&mut map, row(3), &features)).render(row(3), &mut buf);

    Paragraph::new(status_rule(w)).render(row(4), &mut buf);
    Paragraph::new(status_bar(w, state)).render(row(5), &mut buf);

    (buffer_to_ansi(&buf), map)
}

/// Serialize a [`Buffer`] to ANSI: per row an absolute `MoveTo` + line clear, then the
/// cells with SGR emitted only when the style changes. Never touches a row outside the
/// buffer's area and never clears the screen, so mpv's picture above survives intact.
fn buffer_to_ansi(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.y + area.height {
        push(&mut out, MoveTo(area.x, y));
        push(&mut out, Clear(ClearType::CurrentLine));
        // Trailing blank cells are already covered by the line clear.
        let mut last = None;
        for x in area.x..area.x + area.width {
            let cell = &buf[(x, y)];
            if cell.symbol() != " "
                || cell.bg != Color::Reset
                || cell.modifier.contains(Modifier::REVERSED)
            {
                last = Some(x);
            }
        }
        let Some(last) = last else { continue };
        let mut current: Option<(Color, Color, Modifier)> = None;
        for x in area.x..=last {
            let cell = &buf[(x, y)];
            let style = (cell.fg, cell.bg, cell.modifier);
            if current != Some(style) {
                write_sgr(&mut out, cell.fg, cell.bg, cell.modifier);
                current = Some(style);
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\u{1b}[0m");
    }
    out
}

/// The SGR reset+set for one style. Only the colours and modifiers this UI actually
/// uses are mapped - RGB, black, bold, dim, reversed - written explicitly rather than
/// through ratatui's private crossterm conversion.
fn write_sgr(out: &mut String, fg: Color, bg: Color, modifier: Modifier) {
    out.push_str("\u{1b}[0m");
    if modifier.contains(Modifier::BOLD) {
        out.push_str("\u{1b}[1m");
    }
    if modifier.contains(Modifier::DIM) {
        out.push_str("\u{1b}[2m");
    }
    if modifier.contains(Modifier::REVERSED) {
        out.push_str("\u{1b}[7m");
    }
    match fg {
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\u{1b}[38;2;{r};{g};{b}m");
        }
        Color::Black => out.push_str("\u{1b}[30m"),
        _ => {}
    }
    if let Color::Rgb(r, g, b) = bg {
        let _ = write!(out, "\u{1b}[48;2;{r};{g};{b}m");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use ratatui::backend::TestBackend;

    fn entries() -> Vec<EntryRow<'static>> {
        vec![
            EntryRow {
                title: "First Song",
                current: true,
                resolved: true,
                missing: false,
                titled: true,
                title_failed: false,
                status: EntryStatus::Done,
            },
            EntryRow {
                title: "1382326714",
                current: false,
                resolved: true,
                missing: false,
                titled: false,
                title_failed: false,
                status: EntryStatus::None,
            },
            EntryRow {
                title: "Third Song",
                current: false,
                resolved: false,
                missing: false,
                titled: true,
                title_failed: false,
                status: EntryStatus::Queued,
            },
        ]
    }

    fn state<'a>(
        entries: &'a [EntryRow<'a>],
        settings: &'a Settings,
        download: &'a DownloadState,
    ) -> UiState<'a> {
        UiState {
            view: View::Main,
            pane: Pane::Scope,
            menu_cursor: 0,
            pane_ready: [true; 4],
            browse: &[],
            browse_selected: 0,
            browse_filter: None,
            library_status: "42 TRACKS",
            source_label: "YouTube",
            title: "Test Title",
            artist: "",
            album: "",
            position: Some(30.0),
            duration: Some(120.0),
            paused: false,
            idle: false,
            volume: Some(100.0),
            meter: None,
            cover: false,
            is_playlist: !entries.is_empty(),
            entry_index: 0,
            next_title: (!entries.is_empty()).then_some("Next Song"),
            is_loading: false,
            resolving: None,
            crossfading: false,
            analysis: None,
            on_the_beat: false,
            transition: "Crossfade",
            bpm: None,
            stalled: None,
            download,
            download_title: None,
            cache: CacheState::Off,
            downloaded: false,
            batch: None,
            download_enabled: true,
            entries,
            selected: 0,
            edit_mode: false,
            settings,
            settings_cursor: 0,
            effects_on: &[],
            effects_cursor: 0,
            equalizer_cursor: 0,
            quality_locked: false,
            format_locked: false,
            scope_name: "SPECTRUM",
            prompt: None,
            prompt_tag: "ADD NEXT",
            toast: None,
            resume: None,
            term_cols: 100,
            term_rows: 30,
        }
    }

    fn render_to_text(state: &UiState) -> (String, ClickMap) {
        render_sized(state, 100, 30)
    }

    /// The rendered frame with its colours intact, for tests about how something looks
    /// rather than what it says.
    fn render_to_ansi(state: &UiState) -> String {
        let mut tui = ratatui::Terminal::new(TestBackend::new(100, 30)).unwrap();
        let _ = draw(&mut tui, state, None).unwrap();
        buffer_to_ansi(tui.backend().buffer())
    }

    fn render_sized(state: &UiState, w: u16, h: u16) -> (String, ClickMap) {
        let mut tui = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        let map = draw(&mut tui, state, None).unwrap();
        let buffer = tui.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        (text, map)
    }

    #[test]
    fn a_downloaded_track_says_so_instead_of_cached() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.cache = CacheState::Ready;

        let (text, _) = render_to_text(&ui);
        assert!(
            text.contains("CACHED"),
            "the plain cache line should still say so"
        );
        assert!(!text.contains("DOWNLOADED"));

        // Once it is actually saved, the ephemeral recorder cache is not the
        // interesting fact about it any more, whatever state that cache is in.
        ui.downloaded = true;
        for (name, cache) in [
            ("Ready", CacheState::Ready),
            ("Buffering", CacheState::Buffering),
            ("Partial", CacheState::Partial),
            ("Off", CacheState::Off),
        ] {
            ui.cache = cache;
            let (text, _) = render_to_text(&ui);
            assert!(
                text.contains("DOWNLOADED"),
                "downloaded outranks cache state {name}: {text}"
            );
            assert!(!text.contains("CACHED"));
        }
    }

    #[test]
    fn a_caching_track_with_no_title_yet_still_names_its_state() {
        // mpv has no media-title until the stream opens; in video mode the picture is
        // blank too, so an empty title row leaves a pane with nothing in it at all.
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.title = "";
        ui.cache = CacheState::Buffering;

        let (text, _) = render_to_text(&ui);
        assert!(text.contains("caching…"), "text mode title row is blank");

        let (video, _) = video_bottom(&ui);
        assert!(video.contains("caching…"), "video mode title row is blank");

        ui.cache = CacheState::Off;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("loading…"));
    }

    #[test]
    fn main_view_shows_brand_track_and_controls() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("YTM://PLAYER"));
        assert!(text.contains("Test Title"));
        assert!(text.contains("NEXT ▸ Next Song"));
        assert!(text.contains("VIEW ▸ SCOPE · SPECTRUM"));
        assert!(text.contains("VOL"));
        assert!(text.contains("+ Add (o)"));
        // The keybar Quit button dispatches.
        let mut quit_at = None;
        for y in 0..30u16 {
            for x in 0..100u16 {
                if map.action_at(x, y) == Some(Action::Quit) {
                    quit_at = Some((x, y));
                }
            }
        }
        assert!(quit_at.is_some(), "no clickable Quit");
    }

    #[test]
    fn idle_state_reads_as_idle_not_broken() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let mut ui = state(&[], &s, &dl);
        ui.idle = true;
        ui.title = "";
        ui.next_title = None;
        ui.is_playlist = false;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("IDLE"));
        assert!(text.contains("nothing loaded"));
    }

    #[test]
    fn volume_rail_click_maps_to_absolute_volume() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (_, map) = render_to_text(&ui);
        // The rail occupies the far-right columns; find a VolumeSet zone and check
        // its extremes: bottom row -> 0, top row -> max.
        let mut sets = Vec::new();
        for y in 0..30u16 {
            for x in 95..100u16 {
                if let Some(Action::VolumeSet(v)) = map.action_at(x, y) {
                    sets.push((y, v));
                }
            }
        }
        assert!(!sets.is_empty(), "volume rail not clickable");
        let top = sets.iter().min_by_key(|(y, _)| *y).unwrap().1;
        let bottom = sets.iter().max_by_key(|(y, _)| *y).unwrap().1;
        assert!(
            top > bottom,
            "rail direction inverted: top {top} bottom {bottom}"
        );
        assert!((bottom - 0.0).abs() < 1e-6);
        assert!((top - VOLUME_MAX).abs() < 1e-6);
    }

    #[test]
    fn prompt_renders_inline_and_open_view_fullscreen() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut prompt = PromptState::default();
        for c in "https://youtu".chars() {
            prompt.insert(c);
        }
        let mut ui = state(&rows, &s, &dl);
        ui.prompt = Some(&prompt);
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("ADD NEXT"));
        assert!(text.contains("https://youtu"));

        ui.view = View::Open;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("OPEN"));
        assert!(text.contains("paste a link"));
    }

    #[test]
    fn prompt_editing_handles_cursor_and_paste() {
        let mut p = PromptState::default();
        p.insert_str("hello");
        p.home();
        p.right();
        p.insert('x'); // hxello
        assert_eq!(p.text, "hxello");
        p.backspace(); // hello
        assert_eq!(p.text, "hello");
        p.end();
        p.delete(); // no-op at end
        assert_eq!(p.text, "hello");
        p.insert_str(" world");
        assert_eq!(p.text, "hello world");
    }

    /// Every action the map holds anywhere on a `w x h` frame.
    fn actions(map: &ClickMap, w: u16, h: u16) -> Vec<Action> {
        let mut found = Vec::new();
        for y in 0..h {
            for x in 0..w {
                if let Some(action) = map.action_at(x, y)
                    && !found.contains(&action)
                {
                    found.push(action);
                }
            }
        }
        found
    }

    #[test]
    fn queue_pane_lists_rows_in_the_centre_panel() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.pane = Pane::Queue;
        let (text, map) = render_to_text(&ui);
        // The queue is the panel now, not a view you leave the player to visit: the
        // NOW block and the keybar are still on screen around it.
        assert!(text.contains("VIEW ▸ QUEUE · 3 TRACKS"), "{text}");
        assert!(text.contains("Test Title"), "now-playing gone: {text}");
        assert!(text.contains("First Song"), "queue rows missing: {text}");
        assert!(text.contains("⋯"), "untitled row not marked: {text}");
        assert!(text.contains("matching"));
        // Controls follow the panel: edit and save-all only exist while it is up.
        assert!(text.contains("✎ Edit (E)"), "{text}");
        assert!(text.contains("⬇ All (a)"), "{text}");
        // Rows play on a click, and beat the panel's own cycle zone underneath them.
        let acts = actions(&map, 100, 30);
        assert!(
            acts.contains(&Action::JumpTo(1)),
            "rows not clickable: {acts:?}"
        );
        assert!(acts.contains(&Action::CyclePane), "panel does not cycle");
        assert!(acts.contains(&Action::DownloadAll));
    }

    #[test]
    fn queue_edit_mode_swaps_the_controls_and_the_border() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.pane = Pane::Queue;
        ui.selected = 1;
        ui.edit_mode = true;
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("· EDIT ▸ 2/3"), "{text}");
        assert!(text.contains("✓ Done (E)"), "{text}");
        // Move controls replace Edit/Save-all, and only in edit mode.
        let acts = actions(&map, 100, 30);
        assert!(
            acts.contains(&Action::MoveDown),
            "no Move control: {acts:?}"
        );
        assert!(acts.contains(&Action::MoveUp));
        assert!(
            !acts.contains(&Action::DownloadAll),
            "save-all still up in edit mode"
        );
    }

    #[test]
    fn the_scope_pane_is_one_of_three_and_the_panel_cycles() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        // Scope: no queue controls, no per-row zones, and the panel still cycles.
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("VIEW ▸ SCOPE"), "{text}");
        assert!(
            !text.contains("⬇ All (a)"),
            "queue controls on the scope: {text}"
        );
        let acts = actions(&map, 100, 30);
        assert!(acts.contains(&Action::CyclePane));
        assert!(!acts.iter().any(|a| matches!(a, Action::JumpTo(_))));
        // Video: the panel says so even before mpv paints anything.
        ui.pane = Pane::Video;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("VIEW ▸ VIDEO"), "{text}");
    }

    #[test]
    fn the_view_menu_is_a_popup_over_the_view_it_chooses_for() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Menu;
        ui.menu_cursor = 2;
        let (text, map) = render_to_text(&ui);
        // Every pane is offered, with its current variant...
        assert!(text.contains("QUEUE"), "{text}");
        assert!(text.contains("SCOPE"), "{text}");
        assert!(text.contains("VIDEO"), "{text}");
        assert!(text.contains("‹ 3 tracks ›"), "{text}");
        assert!(text.contains("‹ SPECTRUM ›"), "{text}");
        // ...the main view still renders behind it...
        assert!(
            text.contains("YTM://PLAYER"),
            "menu replaced the view: {text}"
        );
        // ...and the popup's rows win the hit-test over whatever it covers.
        let acts = actions(&map, 100, 30);
        for row in 0..3 {
            assert!(
                acts.contains(&Action::MenuRow(row)),
                "row {row} not clickable"
            );
        }
        assert!(acts.contains(&Action::CycleScope), "no restyle control");
    }

    #[test]
    fn the_next_track_line_is_not_a_click_target() {
        // It sits one row under the seek bar; a mis-aimed seek must not swap the
        // centre panel out from under the pointer.
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (text, map) = render_to_text(&ui);
        let next_row = text
            .lines()
            .position(|line| line.contains("NEXT ▸"))
            .expect("no NEXT line") as u16;
        for x in 1..90u16 {
            assert_eq!(
                map.action_at(x, next_row),
                None,
                "NEXT line is clickable at column {x}"
            );
        }
    }

    #[test]
    fn every_settings_row_is_drawn_named_and_clickable() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Settings;
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("Quality"));
        assert!(text.contains("Save format"));
        assert!(text.contains("Smart loading"));
        assert!(text.contains("Clipboard watch"));
        assert!(text.contains("Crossfade"));
        assert!(text.contains("Fade length"));
        assert!(text.contains("Fade curve"));
        // The value says where the join falls, because the number alone is ambiguous.
        assert!(
            text.contains("‹ 5 s ▸ join 2.5 ›"),
            "the fade length should name where the join falls: {text}"
        );
        assert!(
            text.contains("enable crossfade first"),
            "fade length locked while crossfade is off: {text}"
        );
        // Every row has a click zone, and they run down the screen in the order
        // `Setting::ALL` declares. Derived rather than written down as coordinates: the
        // panel drops its double-spacing once there are enough rows to need the room,
        // so a test holding a copy of the spacing fails for the wrong reason the next
        // time a setting is added.
        let mut seen: Vec<(u16, usize)> = Vec::new();
        for y in 0..30u16 {
            if let Some(Action::SettingsRow(i)) = map.action_at(4, y) {
                seen.push((y, i));
            }
        }
        assert_eq!(
            seen.len(),
            SETTING_ROWS,
            "not every settings row is clickable: {seen:?}"
        );
        assert!(
            seen.iter().map(|(_, i)| *i).eq(0..SETTING_ROWS),
            "rows are not top-to-bottom in Setting::ALL order: {seen:?}"
        );
    }

    #[test]
    fn the_equalizer_view_lists_presets_and_draws_the_one_under_the_cursor() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Equalizer;
        let (text, map) = render_to_text(&ui);

        for preset in equalizer::ALL {
            assert!(
                text.contains(preset.name),
                "{} missing: {text}",
                preset.name
            );
        }
        // Flat is the default, so it is marked as the one in use and the curve is a
        // plain line - no bars at all, rather than five bars of zero height.
        assert!(
            text.contains("● Flat"),
            "the preset in use is not marked: {text}"
        );
        assert!(
            text.contains(equalizer::ALL[0].note),
            "the note for the preset under the cursor is missing: {text}"
        );
        assert!(!text.contains('█'), "Flat drew a bar: {text}");
        // The axis and its labels are the frame the shape is read against.
        for label in ["100", "300", "1k", "3.5k", "10k", "12", "-12"] {
            assert!(text.contains(label), "curve label {label} missing: {text}");
        }

        // Every preset is clickable, in list order.
        let mut seen = Vec::new();
        for y in 0..30u16 {
            if let Some(Action::EqualizerRow(i)) = map.action_at(4, y) {
                seen.push(i);
            }
        }
        assert!(
            seen.iter().copied().eq(0..equalizer::ALL.len()),
            "presets are not all clickable top-to-bottom: {seen:?}"
        );
    }

    #[test]
    fn the_curve_draws_the_shape_and_not_just_its_numbers() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Equalizer;
        // Loudness is the smile: both ends up, the middle down. If the drawing is right,
        // that is visible as bars above the axis at the edges and below it in the middle.
        ui.equalizer_cursor = equalizer::parse("loudness").expect("a real preset");
        let (text, _) = render_to_text(&ui);
        let lines: Vec<&str> = text.lines().collect();
        let axis = lines
            .iter()
            .position(|l| l.contains('┼'))
            .expect("an axis row");
        let above: String = lines[..axis].concat();
        let below: String = lines[axis + 1..].concat();
        assert!(above.contains('█'), "nothing is boosted: {text}");
        assert!(
            below.contains('█') || below.contains('▀'),
            "nothing is cut: {text}"
        );

        // ...and the cut band really is the middle one. Column arithmetic rather than
        // eyeballing: the bars sit in fixed columns, so the one that dips is knowable.
        let axis_line = lines[axis];
        // Counted in characters, not bytes: the row is full of multi-byte box drawing,
        // so a byte offset would land somewhere else entirely.
        let first_bar = axis_line
            .chars()
            .position(|c| c == '┼')
            .expect("the axis marker")
            + 1;
        let middle_band = first_bar + 2 * BAND_COLS;
        let dips = lines[axis + 1..].iter().any(|l| {
            l.chars()
                .nth(middle_band)
                .is_some_and(|c| c == '█' || c == '▀')
        });
        assert!(dips, "the smile's middle band is not the one cut: {text}");
    }

    #[test]
    fn effects_view_lists_registry_and_marks_enabled() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut on = vec![false; effects::ALL.len()];
        on[0] = true;
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Effects;
        ui.effects_on = &on;
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("EFFECTS"));
        assert!(
            text.contains(effects::ALL[0].name),
            "the top of the rack: {text}"
        );
        assert!(
            text.contains("‹ On ›"),
            "the enabled one is not marked: {text}"
        );
        assert!(text.contains("← Back (e)"));
        // Every row on screen is clickable and they run in registry order.
        let mut seen = Vec::new();
        for y in 0..30u16 {
            if let Some(Action::EffectRow(i)) = map.action_at(4, y) {
                seen.push(i);
            }
        }
        assert!(!seen.is_empty(), "no clickable rows");
        assert!(
            seen.windows(2).all(|w| w[1] == w[0] + 1),
            "rows are not consecutive: {seen:?}"
        );

        // The rack is longer than any terminal, so it scrolls rather than clipping: with
        // the cursor at the end, the last effect is the one on screen and the first is
        // not. Before this it simply drew the first screenful and lost the rest.
        ui.effects_cursor = effects::ALL.len() - 1;
        let (bottom, _) = render_to_text(&ui);
        let last = effects::ALL[effects::ALL.len() - 1].name;
        assert!(
            bottom.contains(last),
            "the end of the rack is unreachable: {bottom}"
        );
    }

    #[test]
    fn main_keybar_offers_effects_and_marks_live_ones() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("♪ FX (e)"), "no effects button: {text}");
        let mut hit = None;
        for y in 0..30u16 {
            for x in 0..100u16 {
                if map.action_at(x, y) == Some(Action::OpenEffects) {
                    hit = Some((x, y));
                }
            }
        }
        assert!(hit.is_some(), "effects button not clickable");
    }

    #[test]
    fn bar_click_seeks_proportionally() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (_, map) = render_to_text(&ui);
        let mut fractions = Vec::new();
        for y in 0..30u16 {
            for x in 0..100u16 {
                if let Some(Action::SeekTo(f)) = map.action_at(x, y) {
                    fractions.push(f);
                }
            }
        }
        assert!(!fractions.is_empty(), "no seek bar");
        let first = fractions.first().copied().unwrap();
        let last = fractions.last().copied().unwrap();
        assert!(
            first < 0.05 && last > 0.95,
            "bar ends wrong: {first} {last}"
        );
    }

    #[test]
    fn video_bottom_repaints_only_the_reserved_rows() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (rendered, _) = video_bottom(&ui);
        // Positioned at the reserved rows: 30-6 = row 24 -> ESC[25;1H...
        assert!(
            rendered.contains("\u{1b}[25;1H"),
            "block row missing: 30-6+1"
        );
        // ...and never above them, and never a whole-screen clear (that blacks out mpv).
        for row in 1..=24 {
            assert!(
                !rendered.contains(&format!("\u{1b}[{row};1H")),
                "painted into mpv's rows: {row}"
            );
        }
        assert!(!rendered.contains("\u{1b}[2J"));
        assert!(rendered.contains("PLAYING"));
        assert!(rendered.contains("Test Title"));
        // Buttons render as `(key) icon label`; match the label. Video mode's row is
        // the player's controls plus the one way back to the chooser.
        assert!(rendered.contains(" View"));
        // The status bar's source segment reaches video mode too.
        assert!(rendered.contains("YOUTUBE"));
    }

    #[test]
    fn video_clicks_hit_bar_picture_and_buttons() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (_, map) = video_bottom(&ui);
        // Picture click pauses.
        assert_eq!(map.action_at(50, 5), Some(Action::TogglePause));
        // The progress row seeks near its left edge...
        let bar = bar_row(30) + 1;
        assert!(matches!(map.action_at(7, bar), Some(Action::SeekTo(f)) if f < 0.1));
        // ...the transport row starts with play/pause...
        assert_eq!(map.action_at(1, bar_row(30) + 2), Some(Action::TogglePause));
        // ...the feature row ends with Quit flush against the right edge...
        assert_eq!(map.action_at(99, bar_row(30) + 3), Some(Action::Quit));
        // ...and the status bar row is not clickable.
        assert_eq!(map.action_at(7, bar_row(30) + 4), None);
    }

    #[test]
    fn status_priority_toast_beats_downloads() {
        let s = Settings::default();
        let dl = DownloadState::Running { percent: 40.0 };
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.download_title = Some("Test Title");
        ui.toast = Some("⚡ queued from clipboard");
        let text: String = status_spans(&ui)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("clipboard"));
        ui.toast = None;
        let text: String = status_spans(&ui)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("40%"), "no percent: {text}");
        assert!(text.contains("▰"), "no gauge: {text}");
        assert!(text.contains("Test Title"), "no title: {text}");
    }

    #[test]
    fn status_bar_reports_batch_progress_and_is_always_present() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        // Resting state: streaming plus source and track count, rule to both edges.
        let text: String = status_bar(100, &ui)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("STREAMING"), "no resting state: {text}");
        assert!(text.contains("YOUTUBE · 3 TRACKS"), "no source: {text}");
        assert_eq!(
            UnicodeWidthStr::width(text.as_str()),
            100,
            "bar does not span the row"
        );

        // A batch mid-download: ordinal, percent, gauge, and what is downloading.
        ui.batch = Some(BatchState {
            saved: 6,
            total: 62,
            percent: Some(42.0),
        });
        ui.download_title = Some("Some Track");
        let text: String = status_bar(100, &ui)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("QUEUE 7/62"), "no ordinal: {text}");
        assert!(text.contains("42%"), "no percent: {text}");
        assert!(text.contains("Some Track"), "no title: {text}");

        // The bar is on every view, not just Main.
        ui.pane = Pane::Queue;
        let (text, _) = render_to_text(&ui);
        assert!(
            text.contains("QUEUE 7/62"),
            "no status bar under the queue pane"
        );
        ui.view = View::Settings;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("QUEUE 7/62"), "no status bar in settings");
    }

    #[test]
    fn a_narrow_row_sheds_extras_instead_of_clipping_quit() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.pane = Pane::Queue;
        // 115 columns fits the lot: the zone dividers cost a few cells, and so does the
        // EQ button, same as any other control would.
        let wide = feature_buttons(&ui, true, 115);
        assert!(row_width(&wide) <= 115);
        for label in [
            "Add", "Edit", "All", "View", "Setup", "FX", "EQ", "Save", "Quit",
        ] {
            assert!(
                wide.iter().any(|b| b.label == label),
                "{label} missing from a wide row"
            );
        }
        // 75 columns does not: the extras go, the panel's own controls and the way out
        // stay, and the row still fits rather than truncating from the right.
        let narrow = feature_buttons(&ui, true, 75);
        assert!(row_width(&narrow) <= 75, "row still overflows 75 cells");
        for label in ["View", "Save", "Quit", "Edit", "All"] {
            assert!(narrow.iter().any(|b| b.label == label), "{label} dropped");
        }
        assert!(
            !narrow.iter().any(|b| b.label == "Add"),
            "Add kept over Quit"
        );
        // Squeezed harder, only the controls that can never be dropped remain, in the
        // order the row still groups them: the pane's own zone empties out entirely
        // and its separator goes with it, but a divider between two zones that both
        // still have something in them stays.
        let tiny = feature_buttons(&ui, true, 40);
        assert!(row_width(&tiny) <= 40, "row still overflows 40 cells");
        assert_eq!(
            tiny.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(),
            vec!["Save", "", "View", "", "Quit"]
        );
        assert!(
            !tiny.first().is_some_and(|b| b.sep) && !tiny.last().is_some_and(|b| b.sep),
            "a row must never start or end on a bare divider"
        );
    }

    #[test]
    fn the_status_line_reports_the_analysis_of_the_next_track() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.next_title = Some("Track Two");

        let say = |ui: &UiState| -> String {
            status_spans(ui)
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };

        ui.analysis = Some(Analysis::Reading);
        let line = say(&ui);
        assert!(line.contains("ANALYSING"), "{line}");
        assert!(
            line.contains("ffmpeg"),
            "the work being done is not named: {line}"
        );
        assert!(line.contains("Track Two"), "{line}");

        // A match says both numbers that matter: what it is, and what was done to it.
        ui.analysis = Some(Analysis::Matched {
            bpm: 132.06,
            stretch: -0.0316,
        });
        let line = say(&ui);
        assert!(line.contains("132.1 BPM"), "{line}");
        assert!(line.contains("-3.2%"), "the stretch is not shown: {line}");
        assert!(line.contains("locked"), "{line}");

        // Nothing to do is not the same as nothing measured, and reads differently.
        ui.analysis = Some(Analysis::Matched {
            bpm: 128.0,
            stretch: 0.0,
        });
        let line = say(&ui);
        assert!(line.contains("already in time"), "{line}");
        assert!(
            !line.contains('%'),
            "a stretch of nothing should not be quoted: {line}"
        );

        ui.analysis = Some(Analysis::Free { bpm: 96.0 });
        assert!(say(&ui).contains("does not match tempo"));

        ui.analysis = Some(Analysis::Unreadable);
        let line = say(&ui);
        assert!(line.contains("no steady beat"), "{line}");
        assert!(
            line.contains("timer"),
            "what happens instead is not said: {line}"
        );

        // And it outranks the fact that the deck is loading, because the two happen
        // together and only one of them says anything new.
        ui.is_loading = true;
        ui.analysis = Some(Analysis::Reading);
        assert!(say(&ui).contains("ANALYSING"), "loading hid the analysis");
    }

    #[test]
    fn a_dead_entry_reads_differently_from_a_nameless_one() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        let list = vec![
            EntryRow {
                title: "Good Track",
                current: false,
                resolved: true,
                missing: false,
                titled: true,
                title_failed: false,
                status: EntryStatus::None,
            },
            // Playable, but nobody could find out what it is called: the raw id is all
            // there is, and that is worth saying rather than leaving it looking busy.
            EntryRow {
                title: "1933131326",
                current: false,
                resolved: true,
                missing: false,
                titled: false,
                title_failed: true,
                status: EntryStatus::None,
            },
            // Never going to play at all.
            EntryRow {
                title: "Gone Forever",
                current: false,
                resolved: false,
                missing: true,
                titled: true,
                title_failed: false,
                status: EntryStatus::None,
            },
        ];
        ui.entries = &list;
        ui.selected = 0;
        ui.pane = Pane::Queue;
        let (text, _) = render_to_text(&ui);

        // The two failures say different things, and neither says "still working".
        assert!(
            text.contains("✗ no name"),
            "nameless entry unmarked: {text}"
        );
        assert!(text.contains("✗ unplayable"), "dead entry unmarked: {text}");
        assert!(
            !text.contains("⌕ matching"),
            "a finished failure still claims to be in progress: {text}"
        );

        // And they are drawn in different colours: amber for the one that still plays,
        // a dark red for the one that never will.
        let painted = render_to_ansi(&ui);
        let amber = format!("38;2;{};{};{}", WARN_RGB.0, WARN_RGB.1, WARN_RGB.2);
        let dead = format!("38;2;{};{};{}", DEAD_RGB.0, DEAD_RGB.1, DEAD_RGB.2);
        assert!(painted.contains(&amber), "nothing drawn in amber");
        assert!(painted.contains(&dead), "nothing drawn in the dead colour");
    }

    #[test]
    fn every_settings_row_carries_its_own_identity() {
        // The list drawn and the list acted on are the same list, in the same order. This
        // is the check that would have caught inserting a row in the middle: before the
        // rows were named, every row below the new one kept its label and picked up its
        // neighbour's action, and the only symptom was a switch that would not switch.
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let drawn: Vec<Setting> = setting_rows(&ui).iter().map(|row| row.0).collect();
        assert_eq!(
            drawn,
            Setting::ALL.to_vec(),
            "the rows are not in the order they act in"
        );

        // And every one of them is a distinct row with something on it.
        let names: Vec<&str> = setting_rows(&ui).iter().map(|row| row.1).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "two rows share a name: {names:?}"
        );
        assert_eq!(names.len(), SETTING_ROWS);
    }

    #[test]
    fn beat_mixing_is_a_choice_with_a_stated_cost() {
        let mut s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();

        // Off by default, and off is a real setting rather than an absence: a clean fade,
        // and the video that beat mixing would cost you.
        assert!(!s.beat_mixing, "beat mixing should be opt-in");
        s.crossfade = true;
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Settings;
        let (text, _) = render_to_text(&ui);
        assert!(
            text.contains("Beat mixing"),
            "the choice is not offered: {text}"
        );
        assert!(
            text.contains("video still works"),
            "the off state does not say what it keeps: {text}"
        );
        // And the score cannot be chosen while it would do nothing.
        assert!(
            text.contains("turn on beat mixing to use a score"),
            "the transition row does not say why it is inert: {text}"
        );

        // On, the cost is stated where the choice is made rather than in a manual.
        s.beat_mixing = true;
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Settings;
        let (text, _) = render_to_text(&ui);
        assert!(
            text.contains("no video"),
            "the on state does not say what it costs: {text}"
        );
    }

    #[test]
    fn the_cover_box_only_takes_room_when_there_is_a_cover() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);

        // Without one, nothing is reserved and the panel is as wide as it ever was.
        ui.cover = false;
        let (plain, map) = render_to_text(&ui);
        assert_eq!(map.cover_area(), None, "a box was reserved with no picture");
        let bar_row = |text: &str| {
            text.lines()
                .find(|line| line.contains("00:30"))
                .unwrap_or_default()
                .to_string()
        };
        let wide = bar_row(&plain);

        // With one, a box appears and everything else moves over to make room for it.
        ui.cover = true;
        let (withart, map) = render_to_text(&ui);
        let (col, row, cols, rows_) = map.cover_area().expect("no box reserved for the cover");
        assert_eq!((cols, rows_), (COVER_COLS, COVER_ROWS));
        assert!(
            col > 0 && row > 0,
            "the box is outside the panel: {col},{row}"
        );
        let narrow = bar_row(&withart);
        assert!(
            narrow.trim_start().len() < wide.trim_start().len(),
            "the progress row did not give up any width for the picture"
        );

        // The reserved cells are left blank, or the picture would be painted over.
        let lines: Vec<&str> = withart.lines().collect();
        for r in row..row + rows_ {
            let line: Vec<char> = lines[r as usize].chars().collect();
            for c in col..col + cols {
                assert_eq!(
                    line.get(c as usize).copied().unwrap_or(' '),
                    ' ',
                    "cell {c},{r} in the cover box was drawn into"
                );
            }
        }

        // On a terminal too narrow to spare the width, the text wins.
        let (_, map) = render_sized(&ui, 34, 30);
        assert_eq!(
            map.cover_area(),
            None,
            "a box was squeezed into a narrow terminal"
        );
    }

    #[test]
    fn the_volume_rail_meters_what_is_coming_out() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        // Left loud and recently louder, right quiet: the two columns must differ, or the
        // meter is decoration rather than an instrument.
        ui.meter = Some([(0.85, 0.95), (0.20, 0.30)]);
        let (text, _) = render_to_text(&ui);
        let lines: Vec<&str> = text.lines().collect();

        let rail: Vec<String> = lines
            .iter()
            .map(|line| line.chars().rev().take(4).collect::<String>())
            .collect();
        let body = rail.join("\n");
        assert!(body.contains('█'), "no meter column drawn:\n{body}");
        assert!(body.contains('━'), "no peak-hold mark drawn:\n{body}");

        // The loud channel must stand taller than the quiet one. Counted rather than
        // eyeballed, because "it looked right" is how a meter ends up reporting the same
        // number twice.
        let column = |offset: usize| {
            lines
                .iter()
                .filter(|line| {
                    let w = line.chars().count();
                    w > offset && line.chars().nth(w - offset - 1) == Some('█')
                })
                .count()
        };
        let (left, right) = (column(2), column(1));
        assert!(
            left > right && right > 0,
            "left {left} rows, right {right} - a louder channel must read higher"
        );

        // And with nothing playing there is nothing to read, rather than a meter pinned
        // at silence.
        ui.meter = None;
        let (quiet, _) = render_to_text(&ui);
        assert!(!quiet.contains('━'), "held peak drawn with no audio");
    }

    #[test]
    fn button_rows_justify_to_the_full_width() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (text, map) = render_to_text(&ui);
        // Keybar rows: transport at 26, features at 27 (100x30, scope pane on, one row
        // given up to the white rule between the keybar and the status line below it).
        // First button starts on the left edge; the last ends flush on the main
        // column's right edge, which is whatever is left beside the volume rail.
        // Worked out from the rail rather than written down beside it - the rail has
        // been widened once already, and a test holding its own copy of the width
        // fails for the wrong reason when it is.
        let main_width = 100 - usize::from(VOL_RAIL_WIDTH);
        assert_eq!(map.action_at(0, 26), Some(Action::TogglePause));
        assert_eq!(map.action_at(main_width as u16 - 1, 27), Some(Action::Quit));
        assert_eq!(map.action_at(0, 27), Some(Action::OpenPrompt));
        // The rule itself: a full-width, unbroken, unclickable line between the two.
        assert_eq!(
            map.action_at(0, 28),
            None,
            "the rule must not be a click target"
        );
        let rule = text.lines().nth(28).unwrap();
        assert!(
            rule.chars().take(main_width).all(|c| c == '─'),
            "row 28 is not a plain rule: {rule:?}"
        );
        // White, not the `faint()` every panel border uses - the one line in the
        // chrome that is not a panel's edge, it is the player's own split between
        // controls and status, and reads as more than background structure.
        let ansi = render_to_ansi(&ui);
        assert!(
            ansi.contains("38;2;235;240;245"),
            "the rule is not styled bright/white"
        );
        // Rendered text agrees with the zone math: the row really ends in "Quit" at
        // the edge. A width-2 icon (unicode-width, not chars) would shift and clip it.
        let features: String = text
            .lines()
            .nth(27)
            .unwrap()
            .chars()
            .take(main_width)
            .collect();
        assert!(
            features.trim_end().ends_with("✕ Quit (q)"),
            "features row misaligned: {features:?}"
        );
        assert_eq!(features.trim_end().chars().count(), main_width);
    }

    #[test]
    fn a_small_terminal_drops_the_centre_panel_entirely() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.term_rows = 12; // below MIN_PANE_ROWS (14) and video's 13
        let (text, map) = render_sized(&ui, 100, 12);
        assert!(
            !text.contains("VIEW ▸"),
            "centre panel should be gone:\n{text}"
        );
        let acts = actions(&map, 100, 12);
        assert!(
            !acts.contains(&Action::CyclePane),
            "panel clickable with no panel"
        );
        // The panel's own controls go with it; the player's stay.
        assert!(
            !text.contains("⬇ All (a)"),
            "queue controls survived: {text}"
        );
        assert!(text.contains("✕ Quit (q)"), "keybar missing:\n{text}");
    }
}
