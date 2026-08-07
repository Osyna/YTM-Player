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

use crate::effects;
use crate::recorder::CacheState;
use crate::settings::Settings;
use crate::tty;
use crate::visualizer::Visualizer;
use crate::viz::VizSnapshot;
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
pub const RESERVED_ROWS: u16 = 5;
/// Width of the volume rail on the main view's right edge, borders included.
const VOL_RAIL_WIDTH: u16 = 5;
/// `--volume-max` in `mpv.rs`: the rail and click math scale to it.
const VOLUME_MAX: f64 = 150.0;
/// Smallest terminal that still renders a scope worth looking at. Under it the visualizer
/// is switched off rather than squeezed into two rows - see [`scope_fits`].
const MIN_SCOPE_COLS: u16 = 44;
const MIN_SCOPE_ROWS: u16 = 20;
/// Rows ASCII video needs for a picture, on top of the transport block below it.
const MIN_PICTURE_ROWS: u16 = 8;
const MIN_VIDEO_COLS: u16 = 44;

/// Whether the visualizer is worth running at this terminal size. False switches the
/// feature off - no pane, no sampling, no scope button - instead of drawing a sliver.
pub fn scope_fits(cols: u16, rows: u16) -> bool {
    cols >= MIN_SCOPE_COLS && rows >= MIN_SCOPE_ROWS
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
const WARN: Color = Color::Rgb(255, 200, 90);
const ERR: Color = Color::Rgb(255, 95, 110);
/// Readable secondary text.
const DIM: Color = Color::Rgb(130, 140, 150);
/// Structure: borders, rules, dead pixels. Barely-there grey-blue.
const FAINT: Color = Color::Rgb(58, 66, 76);
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
    ToggleVideo,
    CycleVisualizer,
    /// Main view: download (or cancel downloading) the current track.
    Download,
    /// Playlist view: download every entry (or cancel the batch).
    DownloadAll,
    OpenPlaylist,
    OpenSettings,
    /// Open the effects menu.
    OpenEffects,
    /// Open the "add a link" prompt.
    OpenPrompt,
    /// Submit the active prompt (the `(Enter) Play` button).
    Submit,
    /// Toggle playlist reorder mode.
    ToggleEdit,
    /// Move the selected playlist entry up/down (edit mode).
    MoveUp,
    MoveDown,
    CloseView,
    /// Play this entry of the playlist.
    JumpTo(usize),
    /// Select settings row `i` and cycle its value forward.
    SettingsRow(usize),
    /// Select effects row `i` and toggle it.
    EffectRow(usize),
    Quit,
}

/// Which full-screen view the text mode is showing.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum View {
    #[default]
    Main,
    Playlist,
    Settings,
    /// The audio effects menu (`e`).
    Effects,
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
    pub source_label: &'a str,
    pub title: &'a str,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub paused: bool,
    /// Nothing is loaded (bare launch, or the queue ran out under `--idle`).
    pub idle: bool,
    pub volume: Option<f64>,
    pub is_playlist: bool,
    /// Current track as an index into `entries`; 0 when there is no playlist.
    pub entry_index: usize,
    pub next_title: Option<&'a str>,
    pub is_loading: bool,
    /// `(resolved, total)` while a background resolver is still matching entries.
    pub resolving: Option<(usize, usize)>,
    pub download: &'a DownloadState,
    /// Title of the track a download or batch is currently pulling, for the status bar.
    pub download_title: Option<&'a str>,
    pub cache: CacheState,
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
    pub quality_locked: bool,
    pub format_locked: bool,
    /// The live visualizer's name, when one is showing.
    pub visualizer: Option<&'a str>,
    /// The inline add-next prompt (Main) or the fullscreen Open input.
    pub prompt: Option<&'a PromptState>,
    /// A transient one-liner (clipboard queue notices and friends).
    pub toast: Option<&'a str>,
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
        let mut f = String::new();
        push(&mut f, DisableBracketedPaste);
        push(&mut f, DisableMouseCapture);
        push(&mut f, Show);
        push(&mut f, LeaveAlternateScreen);
        push(&mut f, EnableLineWrap);
        let _ = self.0.paint(f.as_bytes());
        let _ = disable_raw_mode();
    }
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
}

impl ClickMap {
    fn add(&mut self, rect: Rect, zone: Zone) {
        if rect.width > 0 && rect.height > 0 {
            self.zones.push((rect, zone));
        }
    }

    pub fn action_at(&self, col: u16, row: u16) -> Option<Action> {
        let at = Position { x: col, y: row };
        self.zones
            .iter()
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
        View::Playlist => draw_playlist(frame, state, map),
        View::Settings => draw_settings(frame, state, map),
        View::Effects => draw_effects(frame, state, map),
        View::Open => draw_open(frame, state, map),
    }
}

/// One `(key) icon Label` control. Disabled buttons render dim and get no click zone.
struct Btn {
    key: &'static str,
    icon: &'static str,
    label: String,
    action: Action,
    enabled: bool,
    hot: bool,
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
        }
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

    /// Rendered width in display cells (not chars - `⚡` is two): `(key) icon label`.
    fn width(&self) -> usize {
        let icon = if self.icon.is_empty() {
            0
        } else {
            self.icon.width() + 1
        };
        self.key.width() + 2 + 1 + icon + self.label.as_str().width()
    }
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
        spans.push(Span::styled(format!("({})", b.key), key_st));
        if !b.icon.is_empty() {
            spans.push(Span::styled(format!(" {}", b.icon), icon_st));
        }
        spans.push(Span::styled(format!(" {}", b.label), label_st));
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
        for (k, c) in ch.chars().take(gauge.width as usize).enumerate() {
            buf[(gauge.x + k as u16, y)].set_char(c).set_fg(color);
        }
    }
    let label = format!("{vol:>3.0}");
    let y = inner.y + inner.height - 1;
    for (k, c) in label.chars().take(inner.width as usize).enumerate() {
        frame.buffer_mut()[(inner.x + k as u16, y)]
            .set_char(c)
            .set_fg(DIM);
    }
}

/// The idle scope pane: a dotted grid with a centred hint, so an empty pane still
/// looks like an instrument, not a bug.
fn draw_scope_idle(frame: &mut Frame, inner: Rect, hint: &str) {
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
    // Below the threshold the scope is off, not squeezed: its rows go to NOW.
    let scope_on = scope_fits(state.term_cols, state.term_rows);
    let (now_c, scope_c) = if scope_on {
        (Constraint::Length(5), Constraint::Min(4))
    } else {
        (Constraint::Min(5), Constraint::Length(0))
    };
    let [header_a, now_a, scope_a, prompt_a, keys_a, act_a] = Layout::vertical([
        Constraint::Length(1),
        now_c,
        scope_c,
        Constraint::Length(prompt_rows),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(main_col);

    frame.render_widget(Paragraph::new(header_line(state)), header_a);
    map.add(header_a, Zone::Act(Action::TogglePause));

    // -- NOW panel: title, meta, progress --------------------------------------------
    let now = panel("NOW");
    let now_inner = now.inner(now_a);
    frame.render_widget(now, now_a);
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
                    state.title.to_string(),
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

        // Meta line: what plays next, clickable into the queue.
        if now_inner.height >= 3 {
            let meta_a = Rect {
                y: now_inner.y + 2,
                height: 1,
                ..now_inner
            };
            if let Some(next) = state.next_title {
                let line = Line::from(vec![
                    Span::styled("NEXT ▸ ", faint()),
                    Span::styled(next.to_string(), dim()),
                ]);
                frame.render_widget(Paragraph::new(line), meta_a);
                map.add(meta_a, Zone::Act(Action::OpenPlaylist));
            } else if state.is_playlist {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled("NEXT ▸ ─ end of queue ─", faint()))),
                    meta_a,
                );
                map.add(meta_a, Zone::Act(Action::OpenPlaylist));
            }
        }
    }

    // -- SCOPE panel (dropped entirely when the terminal is too small) ------------------
    if scope_on {
        let tag = match state.visualizer {
            Some(name) => format!("SCOPE ▸ {name}"),
            None => "SCOPE ▸ OFF".to_string(),
        };
        let scope = panel(&tag);
        let scope_inner = scope.inner(scope_a);
        frame.render_widget(scope, scope_a);
        map.add(scope_a, Zone::Act(Action::CycleVisualizer));
        if scope_inner.width >= 2 && scope_inner.height >= 1 {
            match viz {
                Some((vizzer, snap)) if state.visualizer.is_some() => {
                    vizzer.render(snap, scope_inner, frame.buffer_mut());
                }
                _ => draw_scope_idle(frame, scope_inner, "( c ) cycle scopes"),
            }
        }
    }

    // -- prompt (add a link) -------------------------------------------------------------
    if let Some(prompt) = state.prompt {
        draw_prompt_box(frame, prompt_a, prompt, "ADD NEXT");
    }

    // -- keybar ---------------------------------------------------------------------------
    let [keys1_a, keys2_a] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(keys_a);
    let pause_label = if state.paused { "Play" } else { "Pause" };
    let pause_icon = if state.paused { "▶" } else { "⏸" };
    let mut transport = vec![
        Btn::new("Space", pause_label, Action::TogglePause)
            .icon(pause_icon)
            .enabled(!state.idle),
        Btn::new("h", "-5s", Action::SeekBack)
            .icon("«")
            .enabled(!state.idle),
        Btn::new("l", "+5s", Action::SeekForward)
            .icon("»")
            .enabled(!state.idle),
        Btn::new("j", "Vol-", Action::VolumeDown).icon("▾"),
        Btn::new("k", "Vol+", Action::VolumeUp).icon("▴"),
    ];
    if state.is_playlist {
        transport.push(Btn::new("b", "Prev", Action::Prev).icon("⇤"));
        transport.push(Btn::new("n", "Next", Action::Next).icon("⇥"));
    features.push(Btn::new("s", "Setup", Action::OpenSettings).icon("⚙"));
    frame.render_widget(
        Paragraph::new(button_row(map, keys1_a, &transport)),
        keys1_a,
    );

    let video_ok = video_fits(state.term_cols, state.term_rows);
    let mut features = vec![Btn::new("o", "Add", Action::OpenPrompt).icon("+")];
    if state.is_playlist {
        features.push(Btn::new("p", "Queue", Action::OpenPlaylist).icon("≡"));
    }
    features.push(Btn::new("s", "Settings", Action::OpenSettings).icon("⚙"));
    features.push(
        Btn::new("v", "Video", Action::ToggleVideo)
            .icon("▣")
            .enabled(!state.idle && video_ok),
    );
    features.push(
        Btn::new("c", "Scope", Action::CycleVisualizer)
            .icon("∿")
            .enabled(scope_on),
    );
    features.push(
        Btn::new("e", "FX", Action::OpenEffects)
            .icon("♪")
            .hot(state.effects_on.iter().any(|&on| on)),
    );
    features.push(download_button(state));
    features.push(Btn::new("q", "Quit", Action::Quit).icon("✕"));
    frame.render_widget(Paragraph::new(button_row(map, keys2_a, &features)), keys2_a);

    // -- always-on status bar ---------------------------------------------------------
    frame.render_widget(Paragraph::new(status_bar(act_a.width, state)), act_a);
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

    let hints = button_row(
        map,
        hints_a,
        &[
            Btn::new("Enter", "Play", Action::Submit).icon("▶"),
            Btn::new("Esc", "Quit", Action::Quit).icon("✕"),
        ],
    );
    frame.render_widget(Paragraph::new(hints), hints_a);
    if area.height > top + 8 {
        let providers_a = Rect::new(x, area.y + top + 8, width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "youtube · soundcloud · spotify · file · folder",
                faint(),
            ))),
            providers_a,
        );
    }
}

// ---------------------------------------------------------------------------
// Playlist view
// ---------------------------------------------------------------------------

fn draw_playlist(frame: &mut Frame, state: &UiState, map: &mut ClickMap) {
    let saved = state
        .entries
        .iter()
        .filter(|e| e.status == EntryStatus::Done)
        .count();
    let mut tag = format!("QUEUE ▸ {} TRACKS", state.entries.len());
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

    let block = if state.edit_mode {
        panel(&tag).border_style(Style::new().fg(ACCENT2))
    } else {
        panel(&tag)
    };
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());
    if inner.width < 8 || inner.height < 2 {
        return;
    }

    let [list_a, hints_a, status_a] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Keep the cursor centred once the list is taller than the window.
    let height = list_a.height as usize;
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

        let title_style = if entry.current {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else if !entry.resolved || !entry.titled {
            dim()
        } else {
            Style::new().fg(BRIGHT)
        };
        let suffix = if entry.missing {
            " ✗ no match"
        } else if !entry.resolved {
            " ⌕ matching…"
        } else if !entry.titled {
            " ⋯"
        } else {
            ""
        };

        let row_style = if selected && state.edit_mode {
            Style::new().fg(Color::Black).bg(ACCENT2)
        } else if selected {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        let line = Line::from(vec![
            Span::styled(marker, Style::new().fg(OK)),
            Span::styled(format!("{:>index_width$} ", i + 1), dim()),
            Span::styled(glyph, glyph_style),
            Span::styled(entry.title.to_string(), title_style),
            Span::styled(suffix, faint()),
        ])
        .style(row_style);
        lines.push(line);

        map.add(
            Rect {
                x: list_a.x,
                y: list_a.y + row as u16,
                width: list_a.width,
                height: 1,
            },
            Zone::Act(Action::JumpTo(i)),
        );
    }
    frame.render_widget(Paragraph::new(lines), list_a);

    let hints = if state.edit_mode {
        button_row(
            map,
            hints_a,
            &[
                Btn::new("K", "Up", Action::MoveUp)
                    .icon("▲")
                    .enabled(state.selected > 0),
                Btn::new("J", "Down", Action::MoveDown)
                    .icon("▼")
                    .enabled(state.selected + 1 < state.entries.len()),
                Btn::new("e", "Done", Action::ToggleEdit)
                    .icon("✓")
                    .hot(true),
            ],
        )
    } else {
        let batch_label = if state.batch.is_some() {
            "Cancel all"
        } else {
            "Save all"
        };
        let batch_icon = if state.batch.is_some() { "⊘" } else { "⬇" };
        button_row(
            map,
            hints_a,
            &[
                Btn::new("Enter", "Play", Action::JumpTo(state.selected)).icon("▶"),
                Btn::new("e", "Edit", Action::ToggleEdit)
                    .icon("✎")
                    .enabled(state.entries.len() > 1),
                Btn::new("d", batch_label, Action::DownloadAll)
                    .icon(batch_icon)
                    .enabled(state.download_enabled && !state.entries.is_empty()),
                Btn::new("p", "Back", Action::CloseView).icon("←"),
            ],
        )
    };
    frame.render_widget(Paragraph::new(hints), hints_a);
    frame.render_widget(Paragraph::new(status_bar(status_a.width, state)), status_a);
}

// ---------------------------------------------------------------------------
// Settings view
// ---------------------------------------------------------------------------

/// The settings rows: name, current value, whether it's locked, and the note shown.
fn setting_rows(state: &UiState) -> [(&'static str, String, bool, &'static str); 6] {
    let quality_note = if state.quality_locked {
        "audio-only source — nothing to pick"
    } else {
        "height for (v) video and MP4 saves"
    };
    let format_note = if !state.download_enabled && !state.idle {
        "local files are already on disk"
    } else if state.format_locked {
        "SoundCloud & Spotify always save MP3"
    } else {
        "audio-only sources still save MP3"
    };
    let fade_note = if state.settings.crossfade {
        "3–15 s, split across the transition"
    } else {
        "enable crossfade first"
    };
    [
        (
            "Quality",
            state.settings.quality.label().to_string(),
            state.quality_locked,
            quality_note,
        ),
        (
            "Save format",
            state.settings.format.label().to_string(),
            state.format_locked,
            format_note,
        ),
        (
            "Smart loading",
            onoff(state.settings.smart_loading),
            false,
            "playlists start after the first track resolves",
        ),
        (
            "Clipboard watch",
            onoff(state.settings.clipboard_watch),
            false,
            "queue links you copy anywhere on the system",
        ),
        (
            "Crossfade",
            onoff(state.settings.crossfade),
            false,
            "fade tracks into each other on auto-advance",
        ),
        (
            "Fade length",
            format!("{} s", state.settings.crossfade_secs),
            !state.settings.crossfade,
            fade_note,
        ),
    ]
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
    let row_count = rows.len();
    let mut lines = Vec::new();
    for (i, (name, value, locked, note)) in rows.iter().enumerate() {
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
            Span::styled(format!("{value_text:<12}"), value_style),
            Span::styled(format!("  {note}"), dim()),
        ]));
        map.add(
            Rect {
                x: inner.x,
                // Rows render double-spaced (a blank line after each).
                y: inner.y + (i * 2) as u16,
                width: inner.width,
                height: 1,
            },
            Zone::Act(Action::SettingsRow(i)),
        );
        lines.push(Line::raw(""));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    let hints_row = (row_count * 2 + 1) as u16;
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
                Btn::new("Enter", "Change", Action::SettingsRow(usize::MAX))
                    .icon("↺")
                    .enabled(false),
                Btn::new("j/k", "Move", Action::CloseView)
                    .icon("↕")
                    .enabled(false),
                Btn::new("s", "Back", Action::CloseView).icon("←"),
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
                Paragraph::new(Line::styled("saved to ~/.config/ytmplayer/config", faint())),
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

fn draw_effects(frame: &mut Frame, state: &UiState, map: &mut ClickMap) {
    let block = panel("EFFECTS");
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());
    if inner.width < 8 || inner.height < 2 {
        return;
    }

    let mut lines = Vec::new();
    for (i, effect) in effects::ALL.iter().enumerate() {
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
                // Rows render double-spaced (a blank line after each).
                y: inner.y + (i * 2) as u16,
                width: inner.width,
                height: 1,
            },
            Zone::Act(Action::EffectRow(i)),
        );
        lines.push(Line::raw(""));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    let hints_row = (effects::ALL.len() * 2 + 1) as u16;
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
    features.push(Btn::new("s", "Setup", Action::OpenSettings).icon("⚙"));
                )),
        Btn::new("e", "FX", Action::OpenEffects)
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
            state.title.to_string(),
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

    let pause_label = if state.paused { "Play" } else { "Pause" };
    let pause_icon = if state.paused { "▶" } else { "⏸" };
    let mut transport = vec![
        Btn::new("Space", pause_label, Action::TogglePause).icon(pause_icon),
        Btn::new("h", "-5s", Action::SeekBack).icon("«"),
        Btn::new("l", "+5s", Action::SeekForward).icon("»"),
        Btn::new("j", "Vol-", Action::VolumeDown).icon("▾"),
        Btn::new("k", "Vol+", Action::VolumeUp).icon("▴"),
    ];
    if state.is_playlist {
        transport.push(Btn::new("b", "Prev", Action::Prev).icon("⇤"));
        transport.push(Btn::new("n", "Next", Action::Next).icon("⇥"));
    }
    Paragraph::new(button_row(&mut map, row(2), &transport)).render(row(2), &mut buf);

    let mut features = vec![
        Btn::new("v", "Text", Action::ToggleVideo).icon("▤"),
        Btn::new("c", "Scope", Action::CycleVisualizer)
            .icon("∿")
            .enabled(scope_fits(state.term_cols, state.term_rows)),
    ];
    if state.is_playlist {
        features.push(Btn::new("p", "Queue", Action::OpenPlaylist).icon("≡"));
    }
    features.push(Btn::new("s", "Settings", Action::OpenSettings).icon("⚙"));
    features.push(
        Btn::new("e", "Effects", Action::OpenEffects)
            .icon("♪")
            .hot(state.effects_on.iter().any(|&on| on)),
    );
    features.push(download_button(state));
    features.push(Btn::new("q", "Quit", Action::Quit).icon("✕"));
    Paragraph::new(button_row(&mut map, row(3), &features)).render(row(3), &mut buf);

    Paragraph::new(status_bar(w, state)).render(row(4), &mut buf);

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
                status: EntryStatus::Done,
            },
            EntryRow {
                title: "1382326714",
                current: false,
                resolved: true,
                missing: false,
                titled: false,
                status: EntryStatus::None,
            },
            EntryRow {
                title: "Third Song",
                current: false,
                resolved: false,
                missing: false,
                titled: true,
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
            source_label: "YouTube",
            title: "Test Title",
            position: Some(30.0),
            duration: Some(120.0),
            paused: false,
            idle: false,
            volume: Some(100.0),
            is_playlist: !entries.is_empty(),
            entry_index: 0,
            next_title: (!entries.is_empty()).then_some("Next Song"),
            is_loading: false,
            resolving: None,
            download,
            download_title: None,
            cache: CacheState::Off,
            batch: None,
            download_enabled: true,
            entries,
            selected: 0,
            edit_mode: false,
            settings,
            settings_cursor: 0,
            effects_on: &[],
            effects_cursor: 0,
            quality_locked: false,
            format_locked: false,
            visualizer: None,
            prompt: None,
            toast: None,
            term_cols: 100,
            term_rows: 30,
        }
    }

    fn render_to_text(state: &UiState) -> (String, ClickMap) {
        render_sized(state, 100, 30)
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
    fn main_view_shows_brand_track_and_controls() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("YTM://PLAYER"));
        assert!(text.contains("Test Title"));
        assert!(text.contains("NEXT ▸ Next Song"));
        assert!(text.contains("SCOPE ▸ OFF"));
        assert!(text.contains("VOL"));
        assert!(text.contains("(o) + Add"));
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

    #[test]
    fn playlist_marks_untitled_and_unresolved_rows() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Playlist;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("QUEUE ▸ 3 TRACKS"));
        assert!(text.contains("⋯"), "untitled row not marked: {text}");
        assert!(text.contains("matching"));
        assert!(text.contains("(e) ✎ Edit"));
    }

    #[test]
    fn playlist_edit_mode_swaps_hints_and_border() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Playlist;
        ui.edit_mode = true;
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("· EDIT"));
        assert!(text.contains("(e) ✓ Done"));
        // Move buttons only clickable in edit mode.
        let mut found_move = false;
        for y in 0..30u16 {
            for x in 0..100u16 {
                if map.action_at(x, y) == Some(Action::MoveDown) {
                    found_move = true;
                }
            }
        }
        assert!(found_move, "no Move control in edit mode");
    }

    #[test]
    fn settings_has_six_rows_including_crossfade() {
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
        assert!(text.contains("‹ 5 s ›"), "default fade length: {text}");
        assert!(
            text.contains("enable crossfade first"),
            "fade length locked while crossfade is off: {text}"
        );
        // Rows are clickable at their double-spaced positions.
        assert_eq!(map.action_at(4, 1 + 6), Some(Action::SettingsRow(3)));
        assert_eq!(map.action_at(4, 1 + 8), Some(Action::SettingsRow(4)));
        assert_eq!(map.action_at(4, 1 + 10), Some(Action::SettingsRow(5)));
    }

    #[test]
    fn effects_view_lists_registry_and_marks_enabled() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let on = [true, false, false, false, false, false];
        let mut ui = state(&rows, &s, &dl);
        ui.view = View::Effects;
        ui.effects_on = &on;
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("EFFECTS"));
        for effect in effects::ALL {
            assert!(
                text.contains(effect.name),
                "missing {}: {text}",
                effect.name
            );
        }
        assert!(text.contains("‹ On ›"), "echo shows enabled: {text}");
        // Rows click at their double-spaced positions; the toggle targets the row.
        assert_eq!(map.action_at(4, 1), Some(Action::EffectRow(0)));
        assert_eq!(map.action_at(4, 1 + 2), Some(Action::EffectRow(1)));
        assert_eq!(map.action_at(4, 1 + 10), Some(Action::EffectRow(5)));
        assert!(text.contains("(e) ← Back"));
    }

    #[test]
    fn main_keybar_offers_effects_and_marks_live_ones() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (text, map) = render_to_text(&ui);
        assert!(text.contains("(e) ♪ Effects"), "no effects button: {text}");
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
        // Positioned at the reserved rows: 30-5 = row 25 -> ESC[26;1H...
        assert!(
            rendered.contains("\u{1b}[26;1H"),
            "block row missing: 30-5+1"
        );
        // ...and never above them, and never a whole-screen clear (that blacks out mpv).
        for row in 1..=25 {
            assert!(
                !rendered.contains(&format!("\u{1b}[{row};1H")),
                "painted into mpv's rows: {row}"
            );
        }
        assert!(!rendered.contains("\u{1b}[2J"));
        assert!(rendered.contains("PLAYING"));
        assert!(rendered.contains("Test Title"));
        // Buttons render as `(key) icon label`; match the label.
        assert!(rendered.contains(" Scope"));
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
        ui.view = View::Playlist;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("QUEUE 7/62"), "no status bar in queue view");
        ui.view = View::Settings;
        let (text, _) = render_to_text(&ui);
        assert!(text.contains("QUEUE 7/62"), "no status bar in settings");
    }

    #[test]
    fn button_rows_justify_to_the_full_width() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let ui = state(&rows, &s, &dl);
        let (text, map) = render_to_text(&ui);
        // Keybar rows: transport at 27, features at 28 (100x30, scope pane on).
        // First button starts on the left edge; the last ends flush on the main
        // column's right edge (95 wide next to the 5-cell volume rail).
        assert_eq!(map.action_at(0, 27), Some(Action::TogglePause));
        assert_eq!(map.action_at(94, 28), Some(Action::Quit));
        assert_eq!(map.action_at(0, 28), Some(Action::OpenPrompt));
        // Rendered text agrees with the zone math: the row really ends in "Quit" at
        // the edge. A width-2 icon (unicode-width, not chars) would shift and clip it.
        let features: String = text.lines().nth(28).unwrap().chars().take(95).collect();
        assert!(
            features.trim_end().ends_with("(q) ✕ Quit"),
            "features row misaligned: {features:?}"
        );
        assert_eq!(features.trim_end().chars().count(), 95);
    }

    #[test]
    fn small_terminal_disables_scope_and_video() {
        let s = Settings::default();
        let dl = DownloadState::Idle;
        let rows = entries();
        let mut ui = state(&rows, &s, &dl);
        ui.term_rows = 12; // below MIN_SCOPE_ROWS (20) and video's 13
        let (text, map) = render_sized(&ui, 100, 12);
        assert!(!text.contains("SCOPE"), "scope pane should be gone");
        assert!(text.contains("(e) ♪ FX"), "no effects button: {text}");
            for x in 0..100u16 {
                let action = map.action_at(x, y);
                assert_ne!(action, Some(Action::CycleVisualizer), "scope clickable");
                assert_ne!(action, Some(Action::ToggleVideo), "video clickable");
            }
        }
        // The keybar itself is still there, just with those two disabled.
        assert!(text.contains("(q) ✕ Quit"), "keybar missing:\n{text}");
    }
}
