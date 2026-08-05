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

use parking_lot::Mutex;
use std::io::{self, Read, Write};
use std::sync::Arc;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Handle to the terminal; clones share one lock.
#[derive(Clone)]
pub struct Terminal(Arc<Mutex<io::Stdout>>);

impl Terminal {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(io::stdout())))
    }

    /// Paint `bytes` as one indivisible unit. Short writes are fine here - nothing else can be
    /// painting while the lock is held.
    pub fn paint(&self, bytes: &[u8]) -> io::Result<()> {
        let mut out = self.0.lock();
        out.write_all(bytes)?;
        out.flush()
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
                if self.paint(&pending[..cut]).is_err() {
                    return;
                }
                pending.drain(..cut);
            }
        }
    }
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
}
