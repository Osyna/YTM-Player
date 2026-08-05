//! Terminal rendering: raw-mode/alt-screen/mouse lifecycle and frame drawing.
//!
//! Two layouts share one click classifier (see `click_action`) so what the user sees and what
//! a click does can't drift apart:
//!   * text mode  - full UI, bar on a fixed row near the top.
//!   * video mode - mpv's `tct` renderer owns the top rows; we own the bottom `RESERVED_ROWS`.

use crate::recorder::CacheState;
use crate::tty::Terminal;
use crate::youtube::DownloadState;
use crossterm::Command;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::style::Stylize;
use crossterm::terminal::{
    Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use std::fmt::Write as _;
use std::io;

/// Bottom rows reserved for the status bar while ASCII video is playing.
pub const RESERVED_ROWS: u16 = 3;
/// Inner cells of the progress bar, excluding the enclosing brackets, once the terminal is wide
/// enough that it doesn't need to shrink.
const BAR_WIDTH_MAX: usize = 50;
/// The bar never shrinks smaller than this - narrower stops being recognisable as a bar.
const BAR_WIDTH_MIN: usize = 10;
/// Fixed-width content sharing the bar's row, on top of the bar itself: brackets, clock,
/// status text and volume suffix - the widest of the two layouts' bar rows (video mode's).
const BAR_ROW_OVERHEAD: usize = 37;
/// Column of the bar's opening bracket.
const BAR_COL: u16 = 0;
/// Row the bar sits on in the full text UI.
const BAR_ROW_TEXT: u16 = 2;
/// Row the `Status:` line sits on in the text UI - clicking it toggles play/pause.
const STATUS_ROW_TEXT: u16 = 3;

/// The bar's inner width on a terminal `term_cols` wide, shrinking so its row can never overflow
/// and scroll the screen. Both layouts and `click_action` share this - a bar that renders one
/// width but hit-tests another is the same kind of drift the click/render split already guards
/// against.
fn bar_width(term_cols: u16) -> usize {
    (term_cols as usize)
        .saturating_sub(BAR_ROW_OVERHEAD)
        .clamp(BAR_WIDTH_MIN, BAR_WIDTH_MAX)
}

/// Restores the terminal on the way out, whatever happened in between.
pub struct TerminalGuard(Terminal);

impl TerminalGuard {
    pub fn new(term: Terminal) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut f = String::new();
        push(&mut f, EnterAlternateScreen);
        push(&mut f, Hide);
        push(&mut f, EnableMouseCapture);
        push(&mut f, DisableLineWrap);
        push(&mut f, Clear(ClearType::All));
        term.paint(f.as_bytes())?;
        Ok(TerminalGuard(term))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut f = String::new();
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
/// otherwise silently break the UI and click-to-seek.
pub fn reassert_terminal(term: &Terminal) -> io::Result<()> {
    let mut f = String::new();
    push(&mut f, EnterAlternateScreen);
    push(&mut f, Hide);
    push(&mut f, EnableMouseCapture);
    push(&mut f, DisableLineWrap);
    term.paint(f.as_bytes())
}

pub fn clear_screen(term: &Terminal) -> io::Result<()> {
    let mut f = String::new();
    push(&mut f, MoveTo(0, 0));
    push(&mut f, Clear(ClearType::All));
    term.paint(f.as_bytes())
}

/// Row the progress bar occupies in the given mode.
pub fn bar_row(video_mode: bool, term_rows: u16) -> u16 {
    if video_mode {
        term_rows.saturating_sub(RESERVED_ROWS)
    } else {
        BAR_ROW_TEXT
    }
}

/// Rows available to mpv's `tct` renderer, i.e. everything above the reserved status bar.
pub fn video_rows(term_rows: u16) -> u16 {
    term_rows.saturating_sub(RESERVED_ROWS).max(1)
}

/// What a left-click at a given cell means. One classifier so the render layout and the
/// click handling can't drift apart.
#[derive(Debug, PartialEq)]
pub enum ClickAction {
    /// Seek to this 0.0..=1.0 fraction of the track.
    Seek(f64),
    /// Toggle play/pause: the status text, or anywhere on the video image.
    TogglePause,
    Ignore,
}

pub fn click_action(
    col: u16,
    row: u16,
    video_mode: bool,
    term_rows: u16,
    term_cols: u16,
) -> ClickAction {
    let width = bar_width(term_cols);
    let first = BAR_COL + 1;
    let last = first + width as u16 - 1;

    if row == bar_row(video_mode, term_rows) {
        if col >= first && col <= last {
            return ClickAction::Seek((col - first) as f64 / (width - 1) as f64);
        }
        // In video mode the status text shares this row, to the right of the bar.
        if video_mode && col > last {
            return ClickAction::TogglePause;
        }
        return ClickAction::Ignore;
    }

    if video_mode {
        // Clicking the picture itself is the most natural play/pause target.
        if row < video_rows(term_rows) {
            return ClickAction::TogglePause;
        }
        return ClickAction::Ignore;
    }

    if row == STATUS_ROW_TEXT {
        return ClickAction::TogglePause;
    }
    ClickAction::Ignore
}

pub struct FrameData<'a> {
    pub title: &'a str,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub paused: bool,
    pub is_playlist: bool,
    pub playlist_pos: i64,
    pub playlist_count: i64,
    pub is_loading: bool,
    pub next_title: &'a str,
    pub download: &'a DownloadState,
    pub cache: CacheState,
    pub volume: Option<f64>,
    pub quality_label: &'a str,
    pub video_mode: bool,
    pub term_cols: u16,
    pub term_rows: u16,
}

fn format_time(seconds: Option<f64>) -> String {
    let seconds = seconds
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(0.0)
        .round() as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn draw_progress_bar(position: Option<f64>, duration: Option<f64>, width: usize) -> String {
    let filled = match (position, duration) {
        (Some(p), Some(d)) if d > 0.0 => {
            ((width as f64) * p / d).round().clamp(0.0, width as f64) as usize
        }
        _ => 0,
    };
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

fn truncate(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > max {
        format!(
            "{}...",
            chars[..max.saturating_sub(3)].iter().collect::<String>()
        )
    } else {
        text.to_string()
    }
}

fn status_text(paused: bool) -> String {
    if paused {
        format!("{}", "⏸ PAUSED".red().bold())
    } else {
        format!("{}", "▶ PLAYING".green().bold())
    }
}

/// `[███░░░] 01:23 / 03:45` - the bar and its clock, identical in both layouts.
fn progress_line(data: &FrameData) -> String {
    format!(
        "{} {} / {}",
        draw_progress_bar(data.position, data.duration, bar_width(data.term_cols)),
        format_time(data.position),
        format_time(data.duration)
    )
}

/// `  Vol:100%`, or nothing until mpv reports a volume.
fn volume_suffix(volume: Option<f64>) -> String {
    volume.map(|v| format!("  Vol:{v:.0}%")).unwrap_or_default()
}

/// Render `[key] Label` pairs as one spaced run. Both layouts build their control lines from
/// this, so a new binding can't be styled one way here and another way there.
fn hints(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, label)| format!("{} {label}", format!("[{key}]").cyan()))
        .collect::<Vec<_>>()
        .join("  ")
}

/// The one transient line: an active download wins, otherwise the cache hint.
fn activity_line(download: &DownloadState, cache: CacheState) -> Option<String> {
    match download {
        DownloadState::Running { percent } => {
            return Some(format!(
                "{}",
                format!("⬇ Downloading... {percent:.1}%").cyan()
            ));
        }
        DownloadState::Done { path } => {
            return Some(format!("{}", format!("✓ Saved: {path}").green()));
        }
        DownloadState::Failed { message } => {
            return Some(format!("{}", format!("✗ Download failed: {message}").red()));
        }
        DownloadState::Cancelled => {
            return Some(format!("{}", "⊘ Download cancelled".yellow()));
        }
        DownloadState::Idle => {}
    }
    match cache {
        CacheState::Off => None,
        CacheState::Buffering => Some(format!("{}", "⋯ caching audio...".dark_grey())),
        CacheState::Ready => Some(format!("{}", "✓ audio cached".green())),
        CacheState::Partial => Some(format!("{}", "⚠ partial cache".yellow())),
    }
}

/// Append a command's ANSI to the frame under construction.
fn push(frame: &mut String, cmd: impl Command) {
    let _ = cmd.write_ansi(frame);
}

/// The bytes for one complete frame, positioned absolutely so it does not care where mpv left the
/// cursor. Handing this to [`Terminal::paint`] is what keeps a frame indivisible.
pub fn frame(data: &FrameData) -> String {
    if data.video_mode {
        video_bar(data)
    } else {
        text_ui(data)
    }
}

/// Compact three-row bar pinned to the bottom while mpv draws video above it.
fn video_bar(data: &FrameData) -> String {
    let row = bar_row(true, data.term_rows);

    let line1 = format!(
        "{}  {}{}",
        progress_line(data),
        status_text(data.paused),
        volume_suffix(data.volume)
    );

    // The title gives up its row whenever there's something more urgent to say.
    let line2 = activity_line(data.download, data.cache).unwrap_or_else(|| {
        let mut title = format!("{}", truncate(data.title, 60).yellow());
        if data.is_playlist {
            let _ = write!(
                title,
                "  ({}/{})",
                data.playlist_pos + 1,
                data.playlist_count
            );
        }
        title
    });

    let save = format!("Save({})", data.quality_label);
    let mut line3 = hints(&[
        ("p", "Play/Pause"),
        ("h/l", "Seek"),
        ("j/k", "Vol"),
        ("v", "Text"),
        ("d", &save),
        ("Tab", "Qual"),
        ("q", "Quit"),
    ]);
    if data.is_playlist {
        let _ = write!(line3, "  {}", hints(&[("n", "Next"), ("b", "Prev")]));
    }

    let mut frame = String::new();
    // mpv numbers its `tct` rows from zero but positions them with CUP, which has no row zero -
    // so it paints one row fewer than the height we hand it, and the row directly above this bar
    // belongs to nobody. Whatever lands there stays for the rest of the session unless we sweep
    // it, and everything below is about to be redrawn anyway.
    push(&mut frame, MoveTo(0, row.saturating_sub(1)));
    push(&mut frame, Clear(ClearType::FromCursorDown));
    for (offset, line) in [line1, line2, line3].into_iter().enumerate() {
        push(&mut frame, MoveTo(0, row + offset as u16));
        frame.push_str(&line);
        push(&mut frame, Clear(ClearType::UntilNewLine));
    }
    frame
}

/// Full text UI used when ASCII video is off.
fn text_ui(data: &FrameData) -> String {
    // Raw mode means every line needs its own carriage return, and every line has to wipe its own
    // tail: shrink `Download(1080p)` to `Download(MP3)` without one and the leftover characters
    // sit there forever, which is how `[q] Quit` used to end up reading `[q] Quittt`.
    let mut body = String::new();
    let mut line = |text: String| {
        let _ = write!(body, "{text}");
        push(&mut body, Clear(ClearType::UntilNewLine));
        let _ = writeln!(body, "\r");
    };

    line(format!("{}", "▶ YTMPlayer".blue().bold()));
    line(format!("{}", truncate(data.title, 40).yellow()));
    line(progress_line(data));
    line(format!(
        "Status: {}{}",
        status_text(data.paused),
        volume_suffix(data.volume)
    ));

    if data.is_playlist {
        line(format!(
            "\rPlaylist: {}/{}",
            data.playlist_pos + 1,
            data.playlist_count
        ));
        if data.is_loading {
            line(format!(
                "{}",
                format!("Loading next track: {}", data.next_title).magenta()
            ));
        }
    }

    line(String::new());
    let mut playback = hints(&[("p", "Play/Pause"), ("h/l", "Seek 5s"), ("j/k", "Volume")]);
    if data.is_playlist {
        playback.push_str(&format!("  {}", hints(&[("n", "Next"), ("b", "Previous")])));
    }
    line(playback);

    let download = format!("Download({})", data.quality_label);
    line(hints(&[
        ("v", "ASCII video"),
        ("d", &download),
        ("Tab", "Quality"),
        ("q", "Quit"),
    ]));
    line(format!(
        "{}",
        "(click bar to seek - click status line to play/pause)".dark_grey()
    ));

    if let Some(activity) = activity_line(data.download, data.cache) {
        line(activity);
    }

    let mut frame = String::new();
    push(&mut frame, MoveTo(0, 0));
    frame.push_str(&body);
    push(&mut frame, Clear(ClearType::FromCursorDown));
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: u16 = 30;
    // Wide enough that bar_width(COLS) == BAR_WIDTH_MAX, so these tests exercise the same
    // full-width bar the old fixed BAR_WIDTH constant used to be.
    const COLS: u16 = 100;

    fn seek_fraction(action: ClickAction) -> Option<f64> {
        match action {
            ClickAction::Seek(f) => Some(f),
            _ => None,
        }
    }

    #[test]
    fn click_maps_across_the_whole_bar() {
        let width = bar_width(COLS);
        let first = BAR_COL + 1;
        let last = first + width as u16 - 1;
        // Leftmost cell seeks to the start, rightmost to the end.
        assert_eq!(
            click_action(first, BAR_ROW_TEXT, false, ROWS, COLS),
            ClickAction::Seek(0.0)
        );
        assert_eq!(
            click_action(last, BAR_ROW_TEXT, false, ROWS, COLS),
            ClickAction::Seek(1.0)
        );
        let mid = seek_fraction(click_action(first + 24, BAR_ROW_TEXT, false, ROWS, COLS)).unwrap();
        assert!((mid - 0.4898).abs() < 0.001, "midpoint drifted: {mid}");
    }

    #[test]
    fn bracket_and_wrong_row_clicks_do_not_seek() {
        let first = BAR_COL + 1;
        // The enclosing bracket is not a seek target.
        assert_eq!(
            click_action(BAR_COL, BAR_ROW_TEXT, false, ROWS, COLS),
            ClickAction::Ignore
        );
        // Right column, wrong row: row 1 is the title in text mode.
        assert_eq!(
            click_action(first, 1, false, ROWS, COLS),
            ClickAction::Ignore
        );
    }

    #[test]
    fn status_line_toggles_playback_in_text_mode() {
        assert_eq!(
            click_action(0, STATUS_ROW_TEXT, false, ROWS, COLS),
            ClickAction::TogglePause
        );
        assert_eq!(
            click_action(30, STATUS_ROW_TEXT, false, ROWS, COLS),
            ClickAction::TogglePause
        );
    }

    #[test]
    fn video_image_and_its_status_text_toggle_playback() {
        let bar = bar_row(true, ROWS);
        // Anywhere on the picture.
        assert_eq!(
            click_action(10, 0, true, ROWS, COLS),
            ClickAction::TogglePause
        );
        assert_eq!(
            click_action(10, bar - 1, true, ROWS, COLS),
            ClickAction::TogglePause
        );
        // The status text shares the bar row, to the right of the bar itself.
        let past_bar = BAR_COL + 1 + bar_width(COLS) as u16;
        assert_eq!(
            click_action(past_bar + 20, bar, true, ROWS, COLS),
            ClickAction::TogglePause
        );
        // But the bar on that same row still seeks.
        assert_eq!(
            click_action(BAR_COL + 1, bar, true, ROWS, COLS),
            ClickAction::Seek(0.0)
        );
    }

    #[test]
    fn video_mode_layout_does_not_reuse_text_mode_rows() {
        let row = bar_row(true, ROWS);
        assert_eq!(row, ROWS - RESERVED_ROWS);
        // The text-mode bar row is part of the picture in video mode, so it toggles, not seeks.
        assert_eq!(
            click_action(BAR_COL + 1, BAR_ROW_TEXT, true, ROWS, COLS),
            ClickAction::TogglePause
        );
    }

    #[test]
    fn video_never_overlaps_the_status_bar() {
        // tct draws rows 0..=video_rows-1, which must stay above the bar.
        assert!(video_rows(ROWS) <= bar_row(true, ROWS));
        // Degenerate terminals must still leave the renderer at least one row.
        assert_eq!(video_rows(1), 1);
    }

    #[test]
    fn bar_width_shrinks_to_fit_narrow_terminals_but_caps_on_wide_ones() {
        assert_eq!(
            bar_width(200),
            BAR_WIDTH_MAX,
            "plenty of room - use the cap"
        );
        assert_eq!(
            bar_width(COLS),
            BAR_WIDTH_MAX,
            "100 cols is already wide enough for the cap"
        );
        assert!(
            bar_width(50) < BAR_WIDTH_MAX,
            "a narrow terminal must shrink the bar"
        );
        assert_eq!(
            bar_width(1),
            BAR_WIDTH_MIN,
            "never shrinks below the floor, however tiny"
        );
    }

    /// Strips crossterm's escape codes (SGR colors, `Clear`) and splits on cursor-position moves
    /// (`ESC[row;colH`), returning the visible width of each queued line in between. Direct
    /// regression test for the duplicate-bar bug: a queued line wider than the terminal wraps,
    /// and wrapping on the bottom row scrolls the whole alt screen out from under every future
    /// absolute `MoveTo`, leaving old frames behind on screen.
    fn line_widths(raw: &str) -> Vec<usize> {
        let mut widths = Vec::new();
        let mut chunk = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                let mut final_byte = None;
                for c2 in chars.by_ref() {
                    if ('@'..='~').contains(&c2) {
                        final_byte = Some(c2);
                        break;
                    }
                }
                if matches!(final_byte, Some('H') | Some('f')) {
                    widths.push(chunk.chars().count());
                    chunk.clear();
                }
            } else {
                chunk.push(c);
            }
        }
        widths.push(chunk.chars().count());
        widths
    }

    #[test]
    fn video_bar_lines_never_exceed_terminal_width() {
        let download = DownloadState::Idle;
        // 47 is the exact point bar_width's floor (10) plus its overhead (37) stops fitting;
        // 87 is where it hits the cap (50) and stops growing. Cover both boundaries plus either
        // side.
        for term_cols in [47u16, 50, 60, 80, 87, 100, 150, 200] {
            let data = FrameData {
                title: "a very long title that would overflow a narrow terminal all on its own",
                position: Some(83.0),
                duration: Some(213.0),
                paused: false,
                is_playlist: false,
                playlist_pos: 0,
                playlist_count: 0,
                is_loading: false,
                next_title: "",
                download: &download,
                cache: CacheState::Ready,
                volume: Some(100.0),
                quality_label: "1080p",
                video_mode: true,
                term_cols,
                term_rows: 40,
            };
            let raw = video_bar(&data);
            // The first line carrying any text is line1 - the bar plus clock, status and volume,
            // which is the one that used to overflow.
            let bar_line_width = line_widths(&raw).into_iter().find(|w| *w > 0).unwrap();
            assert!(
                bar_line_width <= term_cols as usize,
                "bar line is {bar_line_width} cols wide, terminal is only {term_cols}"
            );
        }
    }

    #[test]
    fn both_layouts_position_every_line_absolutely() {
        // Nothing may reach the screen that depends on where the cursor already was: mpv's frames
        // land between ours and leave it anywhere at all.
        let download = DownloadState::Idle;
        for video_mode in [false, true] {
            let data = FrameData {
                title: "a very long title that would overflow a narrow terminal all on its own",
                position: Some(83.0),
                duration: Some(213.0),
                paused: false,
                is_playlist: true,
                playlist_pos: 3,
                playlist_count: 100,
                is_loading: false,
                next_title: "",
                download: &download,
                cache: CacheState::Ready,
                volume: Some(100.0),
                quality_label: "1080p",
                video_mode,
                term_cols: 200,
                term_rows: 40,
            };
            let rendered = frame(&data);
            assert!(
                rendered.starts_with("\u{1b}["),
                "video_mode={video_mode} frame starts with unpositioned output"
            );
        }
    }

    #[test]
    fn every_text_line_wipes_its_own_tail() {
        // A frame only overwrites as many columns as it has characters, so a line that shrinks
        // between frames leaves the old ending behind unless it clears to the end of the row.
        let download = DownloadState::Idle;
        let data = |quality_label| FrameData {
            title: "Big Buck Bunny",
            position: Some(83.0),
            duration: Some(213.0),
            paused: false,
            is_playlist: false,
            playlist_pos: 0,
            playlist_count: 0,
            is_loading: false,
            next_title: "",
            download: &download,
            cache: CacheState::Ready,
            volume: Some(100.0),
            quality_label,
            video_mode: false,
            term_cols: 100,
            term_rows: 31,
        };
        let rendered = frame(&data("MP3"));
        assert!(
            rendered.contains("Download(MP3)"),
            "fixture no longer exercises the shrinking label"
        );
        // The final segment is the frame's own clear-below, not a row.
        let rows: Vec<&str> = rendered.split("\r\n").collect();
        for row in &rows[..rows.len() - 1] {
            assert!(
                row.ends_with("\u{1b}[K"),
                "row does not clear to end of line: {row:?}"
            );
        }
        // The long label must be strictly wider, or the shrink this guards against cannot happen.
        assert!(frame(&data("1080p")).len() > rendered.len());
    }
}
