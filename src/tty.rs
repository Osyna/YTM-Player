//! The single writer for this terminal.
//!
//! mpv's `tct` renderer and our status bar paint the same screen. A tty makes one `write` atomic
//! against other writers, so sharing the terminal looked safe - but a pty that mpv is flooding at
//! megabytes a second is usually close to full, and then the kernel accepts only part of a larger
//! write. `write_all` finishes the job in a second syscall and mpv paints in the gap. That is how
//! half a status bar ends up stranded in the middle of the picture, and how the lone `H` of a
//! chopped `ESC [ 29;1 H` gets printed on screen as a literal character.
//!
//! So mpv no longer holds the terminal at all: its output arrives here on a pipe. Everything that
//! reaches the screen goes through one lock, and mpv's stream is only ever cut at a point where no
//! escape sequence and no UTF-8 character is half-written.
//!
//! Being the one writer is also what lets this decide *what* of mpv's reaches the screen. mpv
//! believes it owns a terminal: it switches to the alternate screen and back, shows and hides
//! the cursor, turns mouse reporting on and off and wipes the display, all on its own schedule -
//! whenever a renderer starts or stops, which includes every track change. None of that is its
//! to decide here, so its stream is let through only while there is a picture to show, and only
//! the part of it that paints.

use parking_lot::Mutex;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// Synchronized output: between the two the terminal keeps showing the previous frame, then
/// shows the new one whole. A terminal that does not know the mode ignores both.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Handle to the terminal; clones share one lock.
#[derive(Clone)]
pub struct Terminal(Arc<Shared>);

struct Shared {
    out: Mutex<io::Stdout>,
    /// The interface frame under construction - see [`Terminal::queue`].
    frame: Mutex<Vec<u8>>,
    /// How many rows, counted from the top, are mpv's to paint. Zero while there is no picture,
    /// which is what makes [`Terminal::forward`] throw its stream away.
    video_rows: AtomicU16,
}

impl Terminal {
    pub fn new() -> Self {
        Self(Arc::new(Shared {
            out: Mutex::new(io::stdout()),
            frame: Mutex::new(Vec::new()),
            video_rows: AtomicU16::new(0),
        }))
    }

    /// Paint `bytes` as one indivisible unit. Short writes are fine here - nothing else can be
    /// painting while the lock is held.
    pub fn paint(&self, bytes: &[u8]) -> io::Result<()> {
        let mut out = self.0.out.lock();
        out.write_all(bytes)?;
        out.flush()
    }

    /// Add to the interface frame under construction. Nothing reaches the screen until
    /// [`Terminal::commit`].
    pub fn queue(&self, bytes: &[u8]) {
        let mut frame = self.0.frame.lock();
        if frame.is_empty() {
            frame.extend_from_slice(SYNC_BEGIN);
        }
        frame.extend_from_slice(bytes);
    }

    /// Put the queued frame on screen, whole.
    ///
    /// One paint under the lock keeps mpv out of it, and the synchronized-output markers around
    /// it keep the terminal from showing it half-applied. Together that is what makes "clear the
    /// screen and redraw everything" one step for the eye, instead of a blank screen followed by
    /// a repaint.
    pub fn commit(&self) -> io::Result<()> {
        let mut frame = self.0.frame.lock();
        if frame.is_empty() {
            return Ok(());
        }
        frame.extend_from_slice(SYNC_END);
        let painted = self.paint(&frame);
        frame.clear();
        painted
    }

    /// Give the top `rows` rows to mpv's picture and start letting its output through, on a
    /// blank screen.
    ///
    /// Wiped here, under the lock, rather than by the caller afterwards: a clear that lands
    /// after mpv's first frame erases it, and a paused player never paints a second one. And
    /// all of the screen rather than just those rows, because a cover picture is not made of
    /// cells and only erasing the whole display takes it away.
    pub fn open_video(&self, rows: u16) -> io::Result<()> {
        let mut out = self.0.out.lock();
        self.0.video_rows.store(rows, Ordering::Relaxed);
        // The last renderer may have been shut out halfway through a frame, which leaves the
        // terminal holding a synchronized update open - and showing nothing until it ends.
        out.write_all(SYNC_END)?;
        out.write_all(b"\x1b[0m\x1b[2J")?;
        out.flush()
    }

    /// Take the screen back. Whatever mpv still has in the pipe - the rest of a frame, its
    /// renderer's goodbye - is discarded from here on, so nothing has to wait for it to drain
    /// before the interface is drawn where the picture was.
    pub fn close_video(&self) {
        self.0.video_rows.store(0, Ordering::Relaxed);
    }

    /// Forward mpv's video output until the pipe closes.
    pub fn forward(&self, mut video: impl Read) {
        let mut pending = Vec::new();
        let mut chunk = [0u8; 32 * 1024];
        loop {
            match video.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => pending.extend_from_slice(&chunk[..n]),
            }
            let cut = safe_cut(&pending);
            if cut > 0 {
                if self.paint_picture(&pending[..cut]).is_err() {
                    return;
                }
                pending.drain(..cut);
            }
        }
    }

    /// Paint a stretch of mpv's stream, if there is a picture on screen for it to belong to.
    fn paint_picture(&self, stream: &[u8]) -> io::Result<()> {
        let mut out = self.0.out.lock();
        // Read under the lock: a frame let through before the interface took the screen back
        // must not be painted after it.
        let rows = self.0.video_rows.load(Ordering::Relaxed);
        if rows == 0 {
            return Ok(());
        }
        picture_only(stream, rows, &mut *out)?;
        out.flush()
    }
}

/// Blank the top `rows` rows and nothing below them: cursor to the end of the last one, then
/// erase from the start of the screen up to it.
fn erase_rows(out: &mut impl Write, rows: u16) -> io::Result<()> {
    write!(out, "\x1b[0m\x1b[{rows};999H\x1b[1J")
}

/// Write `stream` - whole sequences only, which is how [`safe_cut`] leaves it - without the
/// parts that are mpv running a terminal rather than painting a picture:
///
/// - Private modes (`ESC [ ? … h` and `l`): the alternate screen, the cursor, mouse reporting.
///   mpv sets them when a renderer starts and resets them when it stops, and the reset is the
///   destructive one - it drops the terminal back to the shell's screen and switches off the
///   reporting that clicks arrive on, in the middle of a session. They are ours, set once at
///   startup. Synchronized output is the exception, being about painting.
/// - Erase display (`ESC [ 2 J`), which a starting renderer sends to get a clean canvas. Its
///   canvas is the top `rows` rows; the transport bar below them is not mpv's to wipe.
fn picture_only(stream: &[u8], rows: u16, out: &mut impl Write) -> io::Result<()> {
    let (mut kept, mut at) = (0, 0);
    while let Some(esc) = stream[at..].iter().position(|&b| b == ESC) {
        let start = at + esc;
        let end = start + escape_len(&stream[start..]).unwrap_or(stream.len() - start);
        let seq = &stream[start..end];
        let erase = seq == b"\x1b[2J";
        let mode = seq.starts_with(b"\x1b[?")
            && matches!(seq.last(), Some(b'h' | b'l'))
            && seq != SYNC_BEGIN
            && seq != SYNC_END;
        if erase || mode {
            out.write_all(&stream[kept..start])?;
            if erase {
                erase_rows(out, rows)?;
            }
            kept = end;
        }
        at = end;
    }
    out.write_all(&stream[kept..])
}

/// Longest prefix of `buf` that can be painted on its own, i.e. that leaves no escape sequence and
/// no UTF-8 character half-written. Cutting anywhere else would let a status bar land inside one of
/// mpv's sequences, and the terminal prints the remains of that sequence as stray text.
fn safe_cut(buf: &[u8]) -> usize {
    let mut at = 0;
    let mut safe = 0;
    while at < buf.len() {
        let Some(len) = (if buf[at] == ESC {
            escape_len(&buf[at..])
        } else {
            char_len(&buf[at..])
        }) else {
            break;
        };
        at += len;
        safe = at;
    }
    safe
}

/// Length of the escape sequence starting at `buf[0]`, or `None` while it is still arriving.
fn escape_len(buf: &[u8]) -> Option<usize> {
    match *buf.get(1)? {
        // CSI: parameter and intermediate bytes, then exactly one final byte.
        b'[' => (2..buf.len())
            .find(|&i| (0x40..=0x7e).contains(&buf[i]))
            .map(|i| i + 1),
        // OSC: runs until BEL or ST.
        b']' => (2..buf.len()).find_map(|i| match buf[i] {
            BEL => Some(i + 1),
            ESC if buf.get(i + 1) == Some(&b'\\') => Some(i + 2),
            _ => None,
        }),
        // Everything else is a two-byte escape.
        _ => Some(2),
    }
}

/// Length of the UTF-8 character starting at `buf[0]`, or `None` while it is still arriving.
fn char_len(buf: &[u8]) -> Option<usize> {
    let len = match buf[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // A stray continuation byte: pass it through rather than stalling the stream on it.
        _ => 1,
    };
    (buf.len() >= len).then_some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_stream_is_cut_at_its_end() {
        let s = b"\x1b[29;1H[###]\x1b[K\xe2\x96\x84 done";
        assert_eq!(safe_cut(s), s.len());
    }

    #[test]
    fn a_half_written_escape_is_held_back() {
        // Every truncation of a cursor move must stop at the text before it, never inside the
        // sequence - this is the exact split that used to print a bare `H` into the video.
        let whole = b"ok\x1b[29;1H";
        for cut in 3..whole.len() {
            assert_eq!(
                safe_cut(&whole[..cut]),
                2,
                "cut {cut} sliced into the escape"
            );
        }
        assert_eq!(safe_cut(whole), whole.len());
    }

    #[test]
    fn a_half_written_character_is_held_back() {
        // The half-blocks mpv paints with are three bytes each.
        let whole = "x\u{2584}".as_bytes();
        assert_eq!(safe_cut(&whole[..2]), 1);
        assert_eq!(safe_cut(&whole[..3]), 1);
        assert_eq!(safe_cut(whole), whole.len());
    }

    #[test]
    fn an_unterminated_osc_is_held_back() {
        assert_eq!(safe_cut(b"hi\x1b]0;title"), 2);
        assert_eq!(safe_cut(b"hi\x1b]0;title\x07"), 12);
        assert_eq!(safe_cut(b"hi\x1b]0;title\x1b\\"), 13);
    }

    #[test]
    fn a_stream_reassembles_byte_for_byte_across_arbitrary_chunking() {
        // Whatever the pipe hands us, the bytes reaching the terminal must be the bytes mpv wrote,
        // in order - the cut only decides *when* they go, never *what* goes.
        let stream: Vec<u8> =
            b"\x1b[1;1H\xe2\x96\x84\xe2\x96\x80\x1b[38;2;1;2;3mA\x1b]0;t\x07Z".repeat(7);
        for step in 1..=9 {
            let (mut pending, mut painted) = (Vec::new(), Vec::new());
            for piece in stream.chunks(step) {
                pending.extend_from_slice(piece);
                let cut = safe_cut(&pending);
                painted.extend_from_slice(&pending[..cut]);
                pending.drain(..cut);
            }
            assert_eq!(painted, stream, "step {step} lost or reordered bytes");
            assert!(pending.is_empty(), "step {step} stranded a tail");
        }
    }

    #[test]
    fn mpv_paints_its_picture_and_does_not_get_to_run_the_terminal() {
        // What a renderer writes over its life, as mpv 0.42 sends it: modes on and a wipe,
        // frames, modes off. Only the frame may reach the screen as written - the wipe is cut
        // down to the picture's rows, and the modes are not mpv's to set at all. The last of
        // them, left in, puts the user back on their shell's screen mid-session.
        let frame = "\x1b[?2026h\x1b[1;1f\x1b[38;2;1;2;3m\x1b[48;2;4;5;6m\u{2584}\x1b[0m\x1b[24;1f\x1b[?2026l";
        let stream =
            format!("\x1b[?1003h\x1b[?25l\x1b[?1049h\x1b[2J{frame}\x1b[?1003l\x1b[?25h\x1b[?1049l");
        let mut painted = Vec::new();
        picture_only(stream.as_bytes(), 24, &mut painted).unwrap();
        assert_eq!(
            String::from_utf8(painted).unwrap(),
            format!("\x1b[0m\x1b[24;999H\x1b[1J{frame}")
        );
    }
}
