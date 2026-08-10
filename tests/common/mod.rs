//! Driving the real program, through a real terminal.
//!
//! Everything in `src` is tested against a buffer or a pure function; none of it starts an
//! mpv, and the parts that were hardest to get right - the two crossfade decks trading
//! places, a shuffle both of them have to agree about, a resume that has to wait for a
//! stream to open - only exist once there is a process, a pseudo-terminal and an IPC
//! socket. So the tests here take the same route a user does: spawn the binary on a pty,
//! type at it, and ask mpv itself what happened.
//!
//! Three pieces: [`Pty`] to run it, [`Screen`] to read what it drew, and [`Ipc`] to
//! interrogate the decks behind its back.

#![allow(dead_code)] // each test file uses a different part of this

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where the binary under test is. `CARGO_BIN_EXE_*` is set by cargo for integration
/// tests, so this follows `--release` without being told.
///
/// Checked for length, which sounds paranoid until it happens: an interrupted link
/// leaves a zero-byte artifact behind a fingerprint that says "fresh", so `cargo build`
/// reports success and every test here fails with "the player never drew a frame". It
/// draws no frame because it is not a program. `touch src/main.rs` to force the relink.
fn binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_ytmplayer"));
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    assert!(
        size > 0,
        "{} is {size} bytes - the last build was interrupted mid-link; \
         `touch src/main.rs && cargo build` to force it again",
        path.display()
    );
    path
}

// ---------------------------------------------------------------------------
// The terminal
// ---------------------------------------------------------------------------

/// The program on a pseudo-terminal, with its output parsed into a [`Screen`].
pub struct Pty {
    master: RawFd,
    child: Child,
    pub screen: Screen,
}

impl Pty {
    /// Spawn the player with `args` on a `cols x rows` pty.
    ///
    /// `home` becomes `$HOME` with `XDG_*` cleared, and the working directory too, so a
    /// test gets its own settings, history, resume point and `downloads/` and cannot
    /// disturb the machine it runs on - or be disturbed by it, which is the failure that
    /// wasted the most time before this existed. `downloads/` is relative-path state the
    /// same as the rest, it just did not have a test to notice before there was one that
    /// downloads anything.
    pub fn spawn(args: &[&str], home: &Path, cols: u16, rows: u16) -> Pty {
        let (master, slave) = open_pty(cols, rows);
        // SAFETY: `slave` is a fresh fd from openpty and is not used again here; each
        // `from_raw_fd` gets its own dup so the three handles can be closed independently.
        let (stdin, stdout, stderr) = unsafe {
            (
                Stdio::from_raw_fd(dup(slave)),
                Stdio::from_raw_fd(dup(slave)),
                Stdio::from_raw_fd(dup(slave)),
            )
        };
        let child = Command::new(binary())
            .args(args)
            .env("TERM", "xterm-256color")
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CACHE_HOME")
            .current_dir(home)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn ytmplayer");
        // SAFETY: the child holds its own dups; this end is finished with.
        unsafe { libc::close(slave) };
        set_nonblocking(master);
        Pty {
            master,
            child,
            screen: Screen::new(cols, rows),
        }
    }

    /// The player's own pid, for finding the mpv it spawned.
    pub fn child_id(&self) -> u32 {
        self.child.id()
    }

    /// mpv's IPC socket for the playing deck. The player names it after its own pid.
    pub fn deck_socket(&self) -> PathBuf {
        std::env::temp_dir().join(format!("mpvsocket_{}", self.child.id()))
    }

    /// ...and for the parked crossfade deck, which only exists while crossfade is on.
    pub fn spare_socket(&self) -> PathBuf {
        std::env::temp_dir().join(format!("mpvsocket_{}_b", self.child.id()))
    }

    /// Read and parse output for `duration`, answering anything that needs an answer.
    pub fn pump(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        let mut buf = [0u8; 65536];
        while Instant::now() < deadline {
            // SAFETY: reading into a local buffer from a fd this struct owns.
            let n = unsafe { libc::read(self.master, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let chunk = &buf[..n as usize];
                self.screen.feed(chunk);
                // crossterm asks the terminal where the cursor is on every `clear`, and
                // blocks for two seconds when nothing answers. A test that does not
                // answer measures its own timeout instead of the program.
                if find(chunk, b"\x1b[6n") {
                    self.send("\x1b[1;1R");
                }
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// Type at the program, then let it react.
    pub fn key(&mut self, keys: &str) {
        self.send(keys);
        self.pump(Duration::from_millis(700));
    }

    /// Click at a cell, in the SGR encoding the program enables.
    pub fn click(&mut self, col: u16, row: u16) {
        self.send(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
        self.send(&format!("\x1b[<0;{};{}m", col + 1, row + 1));
        self.pump(Duration::from_millis(700));
    }

    /// Type without waiting - for a burst of the same key, where one settle at the end
    /// is enough and one per press would triple the test's runtime.
    pub fn send_only(&self, text: &str) {
        self.send(text);
    }

    fn send(&self, text: &str) {
        // SAFETY: writing a local buffer to a fd this struct owns.
        unsafe {
            libc::write(self.master, text.as_ptr().cast(), text.len());
        }
    }

    /// Wait until `predicate` sees what it is looking for, pumping as it goes.
    ///
    /// Returns whether it happened, so a test can assert with its own message rather
    /// than dying inside the helper.
    pub fn wait_for(&mut self, within: Duration, predicate: impl Fn(&Screen) -> bool) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(100));
            if predicate(&self.screen) {
                return true;
            }
        }
        false
    }

    /// Quit the way a user does, so the program's own shutdown runs: it is the thing
    /// that kills both decks and removes their sockets.
    pub fn quit(&mut self) {
        self.key("q");
        self.pump(Duration::from_millis(500));
        let _ = self.child.wait();
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // A test that failed mid-way must not leave an mpv singing on the machine - and
        // `kill()` is `SIGKILL`, which the player cannot catch, so its own shutdown never
        // runs and both decks are orphaned onto init. Ask first: the player handles
        // `SIGTERM` and kills its decks on the way out. Insist only if it will not go.
        // (This is not hypothetical. It is how a failing test run left seven mpv
        // processes playing at once on the machine this was written on.)
        // SAFETY: signalling a child this struct spawned and has not yet reaped.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        // SAFETY: closing a fd this struct owns, exactly once.
        unsafe { libc::close(self.master) };
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn open_pty(cols: u16, rows: u16) -> (RawFd, RawFd) {
    let (mut master, mut slave) = (0, 0);
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two fds through the out-params and reads the size struct.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    (master, slave)
}

fn dup(fd: RawFd) -> RawFd {
    // SAFETY: duplicating a live fd.
    let copy = unsafe { libc::dup(fd) };
    assert!(copy >= 0, "dup failed");
    copy
}

fn set_nonblocking(fd: RawFd) {
    // SAFETY: reading and writing this fd's own flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

// ---------------------------------------------------------------------------
// The screen
// ---------------------------------------------------------------------------

/// Enough of a terminal to assert against.
///
/// The program paints with absolute cursor moves, line and screen clears, SGR runs and
/// text - and nothing else, because it builds every frame itself rather than relying on
/// scrolling or wrapping. So that is all this understands. Anything it does not
/// recognise is skipped rather than printed, which is the difference between a readable
/// assertion failure and a wall of escape codes.
pub struct Screen {
    cols: u16,
    rows: u16,
    cells: Vec<char>,
    cursor: (u16, u16),
}

impl Screen {
    fn new(cols: u16, rows: u16) -> Screen {
        Screen {
            cols,
            rows,
            cells: vec![' '; cols as usize * rows as usize],
            cursor: (0, 0),
        }
    }

    fn at(&mut self, col: u16, row: u16) -> Option<&mut char> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells
            .get_mut(row as usize * self.cols as usize + col as usize)
    }

    fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => self.escape(&mut chars),
                '\r' => self.cursor.0 = 0,
                '\n' => self.cursor.1 = (self.cursor.1 + 1).min(self.rows.saturating_sub(1)),
                c if (c as u32) < 0x20 => {}
                c => {
                    let (col, row) = self.cursor;
                    if let Some(cell) = self.at(col, row) {
                        *cell = c;
                    }
                    self.cursor.0 = self.cursor.0.saturating_add(1);
                }
            }
        }
    }

    fn escape(&mut self, chars: &mut std::iter::Peekable<std::str::Chars>) {
        if chars.peek() != Some(&'[') {
            // Two-byte escapes and OSC: nothing this program paints with.
            chars.next();
            return;
        }
        chars.next();
        let mut params = String::new();
        let mut final_byte = ' ';
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                final_byte = c;
                break;
            }
            params.push(c);
        }
        let numbers: Vec<u16> = params
            .trim_start_matches('?')
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        let first = numbers.first().copied().unwrap_or(0);
        match final_byte {
            // Absolute move; the wire is 1-based and may omit either coordinate.
            'H' | 'f' => {
                let row = numbers.first().copied().unwrap_or(1).max(1) - 1;
                let col = numbers.get(1).copied().unwrap_or(1).max(1) - 1;
                self.cursor = (col, row);
            }
            'J' => {
                // 2 clears everything; the program only ever uses that one.
                if first == 2 {
                    self.cells.fill(' ');
                }
            }
            'K' => {
                let (col, row) = self.cursor;
                for c in col..self.cols {
                    if let Some(cell) = self.at(c, row) {
                        *cell = ' ';
                    }
                }
            }
            _ => {}
        }
    }

    /// One row, trailing blanks removed.
    pub fn line(&self, row: u16) -> String {
        let start = row as usize * self.cols as usize;
        let end = start + self.cols as usize;
        self.cells[start..end.min(self.cells.len())]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The whole screen, for an assertion message worth reading.
    pub fn text(&self) -> String {
        (0..self.rows)
            .map(|row| self.line(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn contains(&self, needle: &str) -> bool {
        (0..self.rows).any(|row| self.line(row).contains(needle))
    }

    /// The first line containing `needle`, for pulling a value out of a panel title.
    pub fn find_line(&self, needle: &str) -> Option<String> {
        (0..self.rows)
            .map(|row| self.line(row))
            .find(|line| line.contains(needle))
    }
}

// ---------------------------------------------------------------------------
// The decks
// ---------------------------------------------------------------------------

/// A second client on mpv's IPC socket, so a test can ask a deck what it is really
/// doing rather than inferring it from the screen.
pub struct Ipc {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Ipc {
    /// Connect, waiting for the socket to appear. `None` if it never does - which is a
    /// fact worth asserting on for the parked deck, since it only exists with crossfade
    /// turned on.
    pub fn connect(path: &Path, within: Duration) -> Option<Ipc> {
        let deadline = Instant::now() + within;
        loop {
            if let Ok(stream) = UnixStream::connect(path) {
                let reader = BufReader::new(stream.try_clone().ok()?);
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
                return Some(Ipc {
                    reader,
                    writer: stream,
                    next_id: 1,
                });
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Run a command and hand back its `data`, or `None` for anything that errored -
    /// which is how mpv reports a property that is simply not available yet.
    pub fn command(&mut self, command: serde_json::Value) -> Option<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = serde_json::json!({ "command": command, "request_id": id });
        writeln!(self.writer, "{payload}").ok()?;
        self.writer.flush().ok()?;
        // mpv interleaves unsolicited events with replies; the id decides, not arrival.
        for _ in 0..64 {
            let mut line = String::new();
            if self.reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("request_id").and_then(serde_json::Value::as_u64) == Some(id) {
                if value.get("error").and_then(serde_json::Value::as_str) != Some("success") {
                    return None;
                }
                return value.get("data").cloned();
            }
        }
        None
    }

    pub fn get(&mut self, name: &str) -> Option<serde_json::Value> {
        self.command(serde_json::json!(["get_property", name]))
    }

    pub fn number(&mut self, name: &str) -> Option<f64> {
        self.get(name).and_then(|v| v.as_f64())
    }

    pub fn integer(&mut self, name: &str) -> Option<i64> {
        self.get(name).and_then(|v| v.as_i64())
    }

    pub fn seek(&mut self, to: f64) {
        self.command(serde_json::json!(["seek", to, "absolute+exact"]));
    }

    /// The deck's `af` chain flattened to its filter graphs, which is what a test about
    /// transition styles wants to look at.
    pub fn filter_graph(&mut self) -> String {
        self.get("af")
            .and_then(|v| v.as_array().cloned())
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|n| n.get("params")?.get("graph")?.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    }

    /// The playlist as file names, which is what a shuffle test wants to compare.
    pub fn playlist(&mut self) -> Vec<String> {
        self.get("playlist")
            .and_then(|v| v.as_array().cloned())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e.get("filename").and_then(serde_json::Value::as_str))
                    .map(|path| {
                        Path::new(path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scratch directory that removes itself, so a failed test leaves no litter.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("ytmtest_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch dir");
        Scratch(path)
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The pid of the mpv this player spawned.
pub fn find_mpv_child(parent: u32) -> Option<i32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        let is_mpv = status.lines().any(|l| l == "Name:\tmpv");
        let has_parent = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:\t"))
            .and_then(|p| p.trim().parse::<u32>().ok())
            == Some(parent);
        if is_mpv && has_parent {
            return Some(pid);
        }
    }
    None
}

/// Stop or continue a process, to simulate one that has wedged rather than died.
pub fn signal(pid: i32, sig: i32) {
    // SAFETY: signalling a pid this test found and owns transitively.
    unsafe { libc::kill(pid, sig) };
}

/// Write a settings file into a test's own `$HOME`, so a test that needs a particular
/// configuration does not have to walk the menu to get one - which takes long enough that
/// the track under test has moved on by the time it finishes.
pub fn write_config(home: &Path, body: &str) {
    let dir = home.join(".config").join("ytmplayer");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("config"), body).expect("write config");
}

/// Whether the tools these tests drive are present. Absent, the tests skip rather than
/// fail: a machine without mpv cannot be expected to run a player.
///
/// Both spellings of the version flag are tried, because ffmpeg wants `-version` and
/// answers `--version` with a usage message and a non-zero exit - which silently skipped
/// every test in this file the first time it was written.
pub fn tools_available() -> bool {
    ["mpv", "ffmpeg"].iter().all(|tool| {
        ["--version", "-version"].iter().any(|flag| {
            Command::new(tool)
                .arg(flag)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    })
}

/// Ask `playerctl` something, trimmed. An error is an empty string, which reads the same
/// as "not there" at every call site here.
pub fn playerctl(args: &[&str]) -> String {
    Command::new("playerctl")
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Where a fixture with this exact recipe lives once encoded, keyed by everything that
/// changes its bytes - so two different tests asking for the same beat track share a
/// cache entry, and one asking for a different `bpm` gets its own.
///
/// `/tmp` rather than inside a [`Scratch`]: a scratch directory is gone with the test
/// that made it, which is exactly the lifetime this is trying to outlive. Nothing here
/// ever evicts an entry - a few hundred short mp3s is kilobytes, and `/tmp` is somebody
/// else's problem to clear on reboot.
fn fixture_cache_path(recipe: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    recipe.hash(&mut hasher);
    std::env::temp_dir().join(format!("ytmfix_{:016x}.mp3", hasher.finish()))
}

/// Run `encode` to build `recipe` only if it has never been built before, then link (or,
/// failing that, copy) the cached result onto `path`. `encode` is handed a private,
/// process-unique path to write to and the result is moved into the shared cache slot
/// afterwards - never encoded there directly - because several tests now run at once and
/// two of them asking for the same recipe for the first time at the same moment must not
/// both be writing through the one name `ffmpeg` for either of them reads back.
/// `rename` onto the same filesystem is atomic, so whichever finishes second simply
/// replaces the first with an equally valid encoding of the same recipe - never a
/// half-written one.
fn cached_fixture(recipe: &str, path: &Path, encode: impl FnOnce(&Path)) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("media dir");
    }
    let cached = fixture_cache_path(recipe);
    if !cached.is_file() {
        // `.mp3` stays the last extension: several of the `encode` closures below pick
        // their output muxer from it rather than passing `-f` explicitly. Thread id, not
        // just process id: `cargo test`'s default runner is many threads in one process,
        // and two of them building the same recipe for the first time at once must not
        // collide on the same temporary name.
        let building = cached.with_file_name(format!(
            "{}.{:?}.tmp.mp3",
            cached.file_stem().unwrap_or_default().to_string_lossy(),
            std::thread::current().id()
        ));
        encode(&building);
        std::fs::rename(&building, &cached).expect("move fixture into the cache");
    }
    let _ = std::fs::remove_file(path);
    std::fs::hard_link(&cached, path)
        .or_else(|_| std::fs::copy(&cached, path).map(|_| ()))
        .unwrap_or_else(|e| {
            panic!(
                "could not place cached fixture {} at {}: {e}",
                cached.display(),
                path.display()
            )
        });
}

/// A track with an unmistakable beat at `bpm`: a decaying bass thump on every beat and a
/// tick between them. Real music is harder than this and the automix says so by refusing
/// to align on it; the point here is to prove the machinery runs when the material allows.
pub fn make_beat_track(path: &Path, bpm: f64, seconds: u32) {
    make_beat_track_with_lead(path, bpm, seconds, 0.0);
}

/// The same, but with `lead` seconds of silence before the first beat.
///
/// Which is what almost every real record has, and the reason a sync has to seek the
/// arriving deck rather than simply pressing play on it: two tracks at the same tempo
/// whose bar lines are three quarters of a second apart are not in time with each other.
pub fn make_beat_track_with_lead(path: &Path, bpm: f64, seconds: u32, lead: f64) {
    let recipe = format!("beat:{bpm}:{seconds}:{lead}");
    cached_fixture(&recipe, path, |out| {
        let beat = 60.0 / bpm;
        let expression = format!(
            "aevalsrc='gt(t,{lead})*(0.9*sin(2*PI*55*t)*exp(-12*mod(t-{lead},{beat}))\
             +0.25*sin(2*PI*6000*t)*exp(-90*mod(t-{lead}+{half},{beat})))':d={seconds}:s=48000",
            half = beat / 2.0
        );
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", &expression])
            .args(["-c:a", "libmp3lame", "-b:a", "192k"])
            .arg(out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg failed for {}", out.display());
    });
}

/// A track carrying an embedded cover picture, in `colour`.
///
/// Tagged the way a real download is: a single still image muxed in as a video stream,
/// which is what ffmpeg writes for an ID3 picture frame and what players read back.
pub fn make_cover_track(path: &Path, colour: &str, seconds: u32) {
    let recipe = format!("cover:{colour}:{seconds}");
    cached_fixture(&recipe, path, |out| {
        let art = out.with_extension("cover.png");
        let ok = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i"])
            .arg(format!("color=c={colour}:s=200x200:d=1"))
            .args(["-frames:v", "1"])
            .arg(&art)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run ffmpeg");
        assert!(ok.success(), "could not make a cover picture");
        let ok = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}"))
            .arg("-i")
            .arg(&art)
            .args([
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:v",
                "copy",
                "-id3v2_version",
                "3",
            ])
            .arg(out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run ffmpeg");
        assert!(
            ok.success(),
            "could not mux the cover into {}",
            out.display()
        );
        let _ = std::fs::remove_file(&art);
    });
}

/// One track with real tags, for the paths where the tags are the thing under test.
pub fn make_tagged_track(path: &Path, title: &str, artist: &str, album: &str, seconds: u32) {
    let recipe = format!("tagged:{title}:{artist}:{album}:{seconds}");
    cached_fixture(&recipe, path, |out| {
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}"))
            .args(["-metadata", &format!("title={title}")])
            .args(["-metadata", &format!("artist={artist}")])
            .args(["-metadata", &format!("album={album}")])
            .args(["-c:a", "libmp3lame"])
            .arg(out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg failed for {}", out.display());
    });
}

/// A folder of short, silent-ish tones the player can treat as a local playlist.
///
/// Generated rather than committed: a test fixture that is a binary blob in the
/// repository is one nobody can check the provenance of.
pub fn make_tracks(dir: &Path, names: &[&str], seconds: u32) {
    std::fs::create_dir_all(dir).expect("media dir");
    for (i, name) in names.iter().enumerate() {
        let path = dir.join(format!("{name}.mp3"));
        let freq = 220 + i * 110;
        let recipe = format!("tone:{freq}:{seconds}");
        cached_fixture(&recipe, &path, |out| {
            let status = Command::new("ffmpeg")
                .args(["-y", "-f", "lavfi", "-i"])
                .arg(format!("sine=frequency={freq}:duration={seconds}"))
                .args(["-c:a", "libmp3lame"])
                .arg(out)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("run ffmpeg");
            assert!(status.success(), "ffmpeg failed for {}", out.display());
        });
    }
}

/// Skip rather than fail where mpv and ffmpeg are not installed: a machine without them
/// cannot run a player, and saying so is more useful than a red test.
#[macro_export]
macro_rules! require_tools {
    () => {
        if !$crate::common::tools_available() {
            eprintln!("skipping: mpv and ffmpeg are needed to drive the real program");
            return;
        }
    };
}

/// Turn a setting on through the menu, by name, so the test does not encode row numbers
/// that move every time a setting is added.
pub fn set_setting(pty: &mut Pty, name: &str, presses: usize) {
    pty.key("s");
    assert!(
        pty.wait_for(Duration::from_secs(5), |s| s.contains("SETTINGS")),
        "settings never opened:\n{}",
        pty.screen.text()
    );
    // The cursor is where the last visit left it, and it only walks down here, so go
    // back to the top first. The row sits inside a panel, so the line starts with a
    // border rather than with the marker - match the marker and the name together.
    for _ in 0..12 {
        pty.send_only("k");
    }
    pty.pump(Duration::from_millis(700));
    let marked = format!("▸ {name}");
    for _ in 0..12 {
        if pty.screen.contains(&marked) {
            break;
        }
        pty.key("j");
    }
    assert!(
        pty.screen.contains(&marked),
        "never found the {name} row:\n{}",
        pty.screen.text()
    );
    for _ in 0..presses {
        pty.key("\r");
    }
    pty.key("s");
}
