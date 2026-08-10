//! Cover art: find the picture for what is playing, and draw it in the terminal.
//!
//! Embedded pictures, a `cover.jpg` beside the track, and remote streams that carry one -
//! read with kitty graphics where the terminal has them and half-blocks everywhere else,
//! so it works wherever the rest of the player does.
//!
//! A picture is not made of cells, which is the whole reason this is shaped the way it is.
//! [`Art::escape_sequence`] addresses absolute screen coordinates and touches nothing
//! outside its own rectangle, so the caller paints a frame, leaves a hole in it, and drops
//! the picture into the hole afterwards. Two consequences follow and both are the caller's
//! to handle: the frame must not draw into those cells, or it will paint over the picture;
//! and when the picture goes away the frame must be redrawn in full, because as far as it
//! is concerned those cells already hold what they should.
//!
//! Preparing one is an ffmpeg run, and a network round trip for a stream, so it belongs off
//! the thread that paints frames.
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Which backend this terminal gets. Public so the caller can say so in the UI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// The kitty graphics protocol: a real picture, on terminals known to implement it.
    Kitty,
    /// Unicode half-blocks in true colour, which works wherever the rest of this program does.
    HalfBlocks,
}

/// Pixel size assumed for one cell when preparing a kitty image. Nothing tells us the real figure
/// without asking the terminal and reading its reply, which would race with the input thread, so
/// the picture is built for the usual 1:2 cell and handed to kitty with the cell rectangle it must
/// fill. Getting the ratio slightly wrong costs a few percent of distortion; asking would cost a
/// stray escape sequence in the key queue.
const CELL_PIXELS: (u32, u32) = (10, 20);

/// Stems tried for a picture sitting beside the track, best first. Deliberately a fixed list in a
/// fixed order: two plausible covers in one directory must always resolve the same way, or the
/// pane changes picture depending on the order the filesystem happened to return.
const BESIDE_STEMS: [&str; 7] = [
    "cover", "folder", "front", "albumart", "album", "artwork", "thumb",
];
/// Extensions tried for those stems. `front.*` in the wild is any of these.
const BESIDE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "bmp", "gif"];

/// How much base64 goes in one kitty escape. The protocol requires chunking above 4096 bytes so
/// that a terminal reading the stream never has to buffer an unbounded escape sequence.
const KITTY_CHUNK: usize = 4096;

/// The prepared pixels, in whichever shape the backend needs. Held rather than the finished
/// escape sequence because the caller decides where the pane sits, and that moves between frames.
enum Pixels {
    /// A PNG, already base64-encoded, ready to be chunked into graphics escapes.
    Kitty(String),
    /// `rgb24`, `cols` wide by `2 * rows` tall: one pixel per half-cell.
    HalfBlocks(Vec<u8>),
}

/// A picture, already scaled for a pane and ready to emit.
pub struct Art {
    cols: u16,
    rows: u16,
    pixels: Pixels,
}

impl Art {
    /// Look for art for a local file. `None` when there is none - not an error.
    ///
    /// The embedded picture wins over a file beside the track, because a tagged album with one
    /// stale `folder.jpg` in it is far more common than the reverse.
    ///
    /// Blocking: runs ffprobe and ffmpeg to completion, so tens of milliseconds for a local file
    /// and longer for a cold cache. Call it off the UI thread.
    pub fn for_file(path: &Path, cols: u16, rows: u16) -> Option<Art> {
        if let Some(art) = prepare(path.as_os_str(), cols, rows) {
            return Some(art);
        }
        prepare(beside(path)?.as_os_str(), cols, rows)
    }

    /// Fetch and prepare art from a URL (ffmpeg can read http(s) directly).
    ///
    /// Only `http` and `https` are accepted. ffmpeg opens plenty of other protocols, and this URL
    /// arrives from track metadata, which is not ours to trust with `file:` or `concat:`.
    ///
    /// Blocking: two requests, one to read the picture's size and one to fetch it, so a whole
    /// network round trip each. Call it off the UI thread, and cache the result against the track
    /// rather than calling it per frame.
    pub fn for_url(url: &str, cols: u16, rows: u16) -> Option<Art> {
        let scheme = url.split_once("://")?.0.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        prepare(OsStr::new(url), cols, rows)
    }

    /// Cell size it was prepared for.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// The escape sequence that draws it with its top-left at `(col, row)`, 1-based, absolute.
    ///
    /// Nothing here depends on where the cursor was, and the result ends in a reset, so it can be
    /// concatenated with anything else in any order. It contains no newline, no carriage return
    /// and no erase: painting the bottom-right cell of the screen cannot scroll the picture mpv
    /// is drawing above it, and nothing outside the `cols` by `rows` rectangle is touched.
    pub fn escape_sequence(&self, col: u16, row: u16) -> String {
        match &self.pixels {
            Pixels::Kitty(payload) => self.kitty_sequence(payload, col, row),
            Pixels::HalfBlocks(rgb) => self.half_block_sequence(rgb, col, row),
        }
    }

    /// One `MoveTo`, then the image in chunks. `C=1` stops the terminal moving the cursor over the
    /// picture, which is what would otherwise scroll the screen when the pane reaches the last
    /// row; `q=2` stops it answering, which would otherwise land in the key queue as garbage.
    fn kitty_sequence(&self, payload: &str, col: u16, row: u16) -> String {
        let mut out = String::with_capacity(payload.len() + payload.len() / KITTY_CHUNK * 16 + 64);
        let _ = write!(out, "\u{1b}[{row};{col}H");
        let mut chunks = payload.as_bytes().chunks(KITTY_CHUNK).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            let more = u8::from(chunks.peek().is_some());
            if first {
                let (cols, rows) = (self.cols, self.rows);
                let _ = write!(out, "\u{1b}_Ga=T,f=100,c={cols},r={rows},C=1,q=2,m={more};");
                first = false;
            } else {
                let _ = write!(out, "\u{1b}_Gm={more};");
            }
            out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            out.push_str("\u{1b}\\");
        }
        out.push_str("\u{1b}[0m");
        out
    }

    /// Upper half-blocks: the top pixel is the foreground colour, the bottom one the background,
    /// so a cell carries two. SGR is only re-emitted when a colour actually changes, exactly as
    /// `ui::buffer_to_ansi` does, and it matters just as much here - a 40x20 pane is 800 cells, and
    /// a full escape pair for each would be several kilobytes down a pipe mpv is already
    /// saturating.
    fn half_block_sequence(&self, rgb: &[u8], col: u16, row: u16) -> String {
        let width = usize::from(self.cols);
        let mut out = String::with_capacity(width * usize::from(self.rows) * 8);
        for y in 0..usize::from(self.rows) {
            let _ = write!(out, "\u{1b}[{};{col}H", row + y as u16);
            let mut current: Option<([u8; 3], [u8; 3])> = None;
            for x in 0..width {
                let top = pixel(rgb, width, x, y * 2);
                let bottom = pixel(rgb, width, x, y * 2 + 1);
                if current != Some((top, bottom)) {
                    let _ = write!(
                        out,
                        "\u{1b}[38;2;{};{};{}m\u{1b}[48;2;{};{};{}m",
                        top[0], top[1], top[2], bottom[0], bottom[1], bottom[2]
                    );
                    current = Some((top, bottom));
                }
                out.push('\u{2580}');
            }
            // Per line, so a caller that truncates the string still cannot leak a colour.
            out.push_str("\u{1b}[0m");
        }
        out
    }
}

/// Which backend this terminal gets. Cheap - four environment variables and no I/O - so the
/// caller may ask per frame if that is convenient.
pub fn backend() -> Backend {
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    backend_from(
        &var("TERM"),
        &var("TERM_PROGRAM"),
        &var("KITTY_WINDOW_ID"),
        &var("TMUX"),
    )
}

/// The detection itself, split out so it can be tested without touching the environment.
///
/// Only terminals that ship the graphics protocol are claimed, and a multiplexer vetoes it
/// outright: tmux and screen need the payload wrapped in their own passthrough sequence, and an
/// unwrapped one is printed to the screen as text.
fn backend_from(term: &str, term_program: &str, kitty_window_id: &str, tmux: &str) -> Backend {
    if !tmux.is_empty() || term.starts_with("screen") || term.starts_with("tmux") {
        return Backend::HalfBlocks;
    }
    let known = term.contains("kitty")
        || term.contains("ghostty")
        || !kitty_window_id.is_empty()
        || term_program.eq_ignore_ascii_case("ghostty")
        || term_program.eq_ignore_ascii_case("wezterm");
    if known {
        Backend::Kitty
    } else {
        Backend::HalfBlocks
    }
}

/// Read one pixel, tolerating a short buffer so a truncated ffmpeg run cannot panic mid-frame.
fn pixel(rgb: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
    let at = (y * width + x) * 3;
    match rgb.get(at..at + 3) {
        Some(&[r, g, b]) => [r, g, b],
        _ => [0, 0, 0],
    }
}

/// Probe `input`, scale it, and wrap the result up as [`Art`]. `None` for anything that is not a
/// picture we can read, which includes "this file has no cover" and "that JPEG is corrupt" - the
/// caller treats both the same way, by drawing no art.
fn prepare(input: &OsStr, cols: u16, rows: u16) -> Option<Art> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let backend = backend();
    let cell = match backend {
        Backend::Kitty => CELL_PIXELS,
        // One pixel per half-cell, and a half-cell is roughly square.
        Backend::HalfBlocks => (1, 2),
    };
    let canvas = (u32::from(cols) * cell.0, u32::from(rows) * cell.1);
    let vf = scale_filter(probe_size(input)?, canvas);
    let pixels = match backend {
        Backend::Kitty => Pixels::Kitty(base64(&decode(input, &vf, Output::Png, 0)?)),
        Backend::HalfBlocks => Pixels::HalfBlocks(decode(
            input,
            &vf,
            Output::Rgb24,
            canvas.0 as usize * canvas.1 as usize * 3,
        )?),
    };
    Some(Art { cols, rows, pixels })
}

/// A picture beside the track: `cover.jpg`, `folder.png`, `front.*` and the other names ripping
/// tools leave behind. Matched case-insensitively because half the world's `Cover.JPG` came off a
/// Windows box.
fn beside(path: &Path) -> Option<PathBuf> {
    // A bare relative filename has an empty parent, which is not a directory anything can read.
    let dir = match path.parent()? {
        empty if empty.as_os_str().is_empty() => Path::new("."),
        dir => dir,
    };
    let names: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().to_lowercase(),
                entry.path(),
            )
        })
        .collect();
    BESIDE_STEMS.iter().find_map(|stem| {
        BESIDE_EXTENSIONS.iter().find_map(|ext| {
            let wanted = format!("{stem}.{ext}");
            names
                .iter()
                .find(|(name, _)| *name == wanted)
                .map(|(_, path)| path.clone())
        })
    })
}

/// What ffmpeg should be asked to produce. The two backends want different bytes out of the same
/// scaling pass.
#[derive(Clone, Copy)]
enum Output {
    Png,
    Rgb24,
}

/// Size of the first video stream, which for an audio file is the attached picture. ffprobe rather
/// than a decode because it answers from the header, and because a file with no video stream at
/// all - the common case for an untagged MP3 - is then a fast no.
fn probe_size(input: &OsStr) -> Option<(u32, u32)> {
    let output = Command::new("ffprobe")
        .args(["-v", "quiet", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(input)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (w, h) = text.trim().split_once(',')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// One ffmpeg pass: first video frame, scaled, padded, written to stdout.
///
/// `-nostdin` is not optional. ffmpeg inherits the terminal otherwise and reads the keys meant for
/// the player, and `-v error` keeps its banner off a screen the UI is painting. `-xerror` turns a
/// decode error into a failed exit rather than a half-decoded picture: a cover that is grey below
/// the midline reads as a bug in the player, whereas no cover at all reads as no cover at all.
///
/// `expect` is the exact byte count wanted, or zero for a container format whose length is not
/// known in advance; a short read is the other shape a broken picture arrives in.
fn decode(input: &OsStr, vf: &str, output: Output, expect: usize) -> Option<Vec<u8>> {
    let format: [&str; 4] = match output {
        Output::Png => ["-c:v", "png", "-f", "image2pipe"],
        Output::Rgb24 => ["-pix_fmt", "rgb24", "-f", "rawvideo"],
    };
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-xerror", "-i"])
        .arg(input)
        .args(["-map", "0:v:0", "-frames:v", "1", "-vf", vf])
        .args(format)
        .arg("-")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !result.status.success() {
        return None;
    }
    let wanted = if expect == 0 { 1 } else { expect };
    (result.stdout.len() >= wanted).then(|| {
        let mut bytes = result.stdout;
        if expect > 0 {
            bytes.truncate(expect);
        }
        bytes
    })
}

/// The `-vf` argument that fits `source` into `canvas` and centres it there.
///
/// The fit is computed here rather than with ffmpeg's `force_original_aspect_ratio` so that the
/// arithmetic is visible and testable, and so that a 1x3000 sliver in a small pane rounds to one
/// pixel instead of to zero, which that filter treats as an error and refuses to run.
fn scale_filter(source: (u32, u32), canvas: (u32, u32)) -> String {
    let (w, h) = fit(source, canvas);
    let (x, y) = ((canvas.0 - w) / 2, (canvas.1 - h) / 2);
    // Padding, not stretching: a square cover in a wide pane keeps its shape and gains bars.
    format!(
        "scale={w}:{h}:flags=bicubic,pad={}:{}:{x}:{y}:color=black,format=rgb24",
        canvas.0, canvas.1
    )
}

/// Largest size with `source`'s aspect ratio that fits inside `canvas`, never smaller than one
/// pixel in either direction. Integer throughout: the comparison is a cross-multiplication, so two
/// panes of the same shape always decide the same way rather than drifting apart on a float.
///
/// The free dimension rounds to nearest, which is worth the extra term - at the twenty-odd pixels
/// a half-block pane has to play with, rounding always the same way skews every picture in the
/// same direction by several percent.
fn fit(source: (u32, u32), canvas: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (u64::from(source.0.max(1)), u64::from(source.1.max(1)));
    let (cw, ch) = (u64::from(canvas.0.max(1)), u64::from(canvas.1.max(1)));
    if sw * ch >= sh * cw {
        // Relatively wider than the canvas, so width binds and the bars go above and below.
        let h = ((sh * cw) + sw / 2) / sw;
        (cw as u32, h.clamp(1, ch) as u32)
    } else {
        let w = ((sw * ch) + sh / 2) / sh;
        (w.clamp(1, cw) as u32, ch as u32)
    }
}

/// Standard base64 with padding, which is what the kitty protocol carries its payload in. Twenty
/// lines of table lookup, against a crate and its supply chain for the same thing.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let bits = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);
        for i in 0..4 {
            // Six bits each; the characters that would encode bytes the input never had are `=`.
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(bits >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four coloured quadrants over a 4x2-cell pane, i.e. 4x4 pixels.
    fn quadrants() -> Art {
        let (red, green, blue, white) = ([255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]);
        let mut rgb = Vec::new();
        for row in 0..4 {
            for col in 0..4 {
                let colour = match (row < 2, col < 2) {
                    (true, true) => red,
                    (true, false) => green,
                    (false, true) => blue,
                    (false, false) => white,
                };
                rgb.extend_from_slice(&colour);
            }
        }
        Art {
            cols: 4,
            rows: 2,
            pixels: Pixels::HalfBlocks(rgb),
        }
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
        // The high bits exercise the last two alphabet entries, which is where a hand-written
        // table usually goes wrong.
        assert_eq!(base64(&[0xff, 0xef, 0xbe]), "/+++");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
    }

    #[test]
    fn base64_output_is_the_right_length_for_every_remainder() {
        for len in 0..64 {
            let encoded = base64(&vec![0x5au8; len]);
            assert_eq!(encoded.len(), len.div_ceil(3) * 4, "length {len}");
            assert_eq!(
                encoded.bytes().filter(|&b| b == b'=').count(),
                (3 - len % 3) % 3,
                "padding for {len}"
            );
        }
    }

    #[test]
    fn a_square_picture_fills_a_square_canvas() {
        assert_eq!(fit((600, 600), (40, 40)), (40, 40));
        assert_eq!(fit((37, 37), (40, 40)), (40, 40));
    }

    #[test]
    fn a_wide_picture_in_a_tall_pane_is_padded_not_stretched() {
        // 3:1 into a square canvas keeps 3:1 and leaves bars top and bottom.
        let (w, h) = fit((3000, 1000), (40, 40));
        assert_eq!((w, h), (40, 13));
        assert!(
            (w as f64 / h as f64 - 3.0).abs() < 0.2,
            "aspect drifted to {}",
            w as f64 / h as f64
        );
        assert!(scale_filter((3000, 1000), (40, 40)).contains("pad=40:40:0:13:"));
    }

    #[test]
    fn a_tall_picture_in_a_wide_pane_is_padded_not_stretched() {
        assert_eq!(fit((1000, 3000), (80, 20)), (7, 20));
        assert!(scale_filter((1000, 3000), (80, 20)).contains("pad=80:20:36:0:"));
    }

    #[test]
    fn a_sliver_never_rounds_away_to_nothing() {
        // Zero-width output is what ffmpeg's own aspect-preserving scaler produces here, and it
        // then refuses to run at all.
        assert_eq!(fit((1, 3000), (40, 40)), (1, 40));
        assert_eq!(fit((3000, 1), (40, 40)), (40, 1));
        assert_eq!(fit((0, 0), (40, 40)), (40, 40));
    }

    #[test]
    fn the_fitted_size_always_stays_inside_the_canvas() {
        for &source in &[(1, 3000), (3000, 1), (1920, 1080), (600, 600), (5, 7)] {
            for &canvas in &[(40, 40), (80, 20), (1, 2), (7, 3), (400, 800)] {
                let (w, h) = fit(source, canvas);
                assert!(w >= 1 && h >= 1, "{source:?} in {canvas:?} vanished");
                assert!(
                    w <= canvas.0 && h <= canvas.1,
                    "{source:?} overflowed {canvas:?}"
                );
            }
        }
    }

    #[test]
    fn half_blocks_put_the_right_colour_in_the_right_cell() {
        let drawn = quadrants().escape_sequence(1, 1);
        let (red, green) = (
            "\u{1b}[38;2;255;0;0m\u{1b}[48;2;255;0;0m",
            "\u{1b}[38;2;0;255;0m\u{1b}[48;2;0;255;0m",
        );
        let (blue, white) = (
            "\u{1b}[38;2;0;0;255m\u{1b}[48;2;0;0;255m",
            "\u{1b}[38;2;255;255;255m\u{1b}[48;2;255;255;255m",
        );
        // Red then green on the top cell row, blue then white on the bottom one, two cells each,
        // and the colour is only set where it changes.
        let block = '\u{2580}';
        assert_eq!(
            drawn,
            format!(
                "\u{1b}[1;1H{red}{block}{block}{green}{block}{block}\u{1b}[0m\
                 \u{1b}[2;1H{blue}{block}{block}{white}{block}{block}\u{1b}[0m"
            )
        );
    }

    #[test]
    fn the_escape_sequence_stays_inside_its_rectangle() {
        let drawn = quadrants().escape_sequence(9, 5);
        // A newline or a carriage return here would scroll mpv's picture off the top of the
        // screen, which is the one failure this module must never cause.
        assert!(!drawn.contains('\n') && !drawn.contains('\r'));
        // Nothing that erases, scrolls or moves relatively - only absolute positioning.
        for cue in ["\u{1b}[2J", "\u{1b}[K", "\u{1b}[J", "\u{1b}[S", "\u{1b}[T"] {
            assert!(
                !drawn.contains(cue),
                "{cue:?} would touch the rest of the screen"
            );
        }
        let rows: Vec<&str> = drawn.matches("\u{1b}[").filter(|_| true).collect();
        assert!(!rows.is_empty());
        assert!(drawn.starts_with("\u{1b}[5;9H"));
        assert!(drawn.contains("\u{1b}[6;9H"));
        assert!(
            !drawn.contains("\u{1b}[7;9H"),
            "painted a row it was not given"
        );
        assert!(drawn.ends_with("\u{1b}[0m"), "left styling behind");
    }

    #[test]
    fn kitty_chunks_carry_the_keys_once_and_end_the_run() {
        let art = Art {
            cols: 3,
            rows: 2,
            pixels: Pixels::Kitty(base64(&vec![0u8; 9000])),
        };
        let drawn = art.escape_sequence(4, 7);
        assert!(drawn.starts_with("\u{1b}[7;4H\u{1b}_Ga=T,f=100,c=3,r=2,C=1,q=2,m=1;"));
        // Three chunks for 12000 base64 characters, and only the last says the run has ended.
        assert_eq!(drawn.matches("\u{1b}_G").count(), 3);
        assert_eq!(drawn.matches("m=1;").count(), 2);
        assert_eq!(drawn.matches("m=0;").count(), 1);
        assert!(drawn.ends_with("\u{1b}\\\u{1b}[0m"));
        assert!(!drawn.contains('\n'));
    }

    #[test]
    fn only_terminals_known_to_draw_pictures_are_offered_them() {
        assert_eq!(backend_from("xterm-kitty", "", "", ""), Backend::Kitty);
        assert_eq!(backend_from("xterm-256color", "", "3", ""), Backend::Kitty);
        assert_eq!(backend_from("xterm-ghostty", "", "", ""), Backend::Kitty);
        assert_eq!(
            backend_from("xterm-256color", "WezTerm", "", ""),
            Backend::Kitty
        );
        // Unknown, uncertain, or wrapped in a multiplexer that would print the payload as text.
        assert_eq!(
            backend_from("xterm-256color", "", "", ""),
            Backend::HalfBlocks
        );
        assert_eq!(backend_from("", "", "", ""), Backend::HalfBlocks);
        assert_eq!(
            backend_from("xterm-256color", "iTerm.app", "", ""),
            Backend::HalfBlocks
        );
        assert_eq!(
            backend_from("screen.xterm-kitty", "", "7", ""),
            Backend::HalfBlocks
        );
        assert_eq!(
            backend_from("xterm-kitty", "", "7", "/tmp/tmux-1000/default,1,0"),
            Backend::HalfBlocks
        );
    }

    #[test]
    fn a_short_buffer_reads_black_rather_than_panicking() {
        assert_eq!(pixel(&[1, 2, 3], 1, 0, 0), [1, 2, 3]);
        assert_eq!(pixel(&[1, 2, 3], 1, 0, 9), [0, 0, 0]);
        assert_eq!(pixel(&[], 4, 3, 3), [0, 0, 0]);
    }

    #[test]
    fn a_url_that_is_not_http_is_refused_without_running_anything() {
        assert!(Art::for_url("file:///etc/passwd", 10, 10).is_none());
        assert!(Art::for_url("concat:/etc/passwd", 10, 10).is_none());
        assert!(Art::for_url("/etc/passwd", 10, 10).is_none());
        assert!(Art::for_url("https://example.invalid/x.jpg", 0, 0).is_none());
    }
}
