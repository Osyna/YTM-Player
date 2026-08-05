# Changelog

All notable changes to YTM-Player. Format based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [3.0.0] - 2026-08-05

The Bash script is gone. YTM-Player is now a single Rust binary that talks to
mpv and yt-dlp directly over their own interfaces, and it can put the video on
your terminal and pull the track down to disk.

This is a rewrite, so everything below is relative to the 2.x shell script.

### Added

- **ASCII video with `v`.** Renders the current track as true-colour half-block
  video in the same terminal, using mpv's built-in `tct` output. Playback never
  stops and never restarts — the picture is a video track handed to the running
  mpv, and `v` again hands the terminal back.
- **Downloads with `d`.** Saves the current track to `downloads/` in the
  background, with live percentage in the status line. Press `d` again to
  cancel. At the default MP3 tier the audio has already been captured while it
  streamed, so the save is instant and touches the network not at all.
- **Quality cycling with `Tab`.** `MP3 -> 480p -> 720p -> 1080p -> Best`. The
  chosen tier is what `d` writes, and raising it above MP3 also raises the
  resolution the next `v` asks for.
- **Volume with `j` / `k`**, up to 150%.
- **Mouse support.** Click the progress bar to seek to that point; click the
  status line to toggle play/pause.
- **Playlist navigation with `n` / `b`,** with the position shown as `4/100`
  and a hint line while the next track loads.
- **Prebuilt binaries** attached to every tagged release — a glibc build and a
  fully static musl build that runs on any x86-64 Linux.
- **CI** on every push: formatting, Clippy with warnings denied, tests, release
  build.

### Changed

- **Rewritten in Rust** as one binary. 2,524 lines across six modules, 730 KB
  built (840 KB static), 35 crates in the tree and four direct dependencies.
- **Audio-only by default.** Nothing decodes a picture until you press `v`. One
  yt-dlp call returns both the audio and a low-resolution video URL; only the
  audio is handed to mpv, and the video URL is held back until it is wanted.
- **Startup is one yt-dlp call instead of three.** The old path ran a preflight
  probe and then let mpv's `ytdl_hook` resolve the same video all over again.
  Resolving once up front and starting mpv with `--no-ytdl` cut **1.9 s** off
  every start, measured against the old path on the same video.
- **mpv runs with `--osc=no`.** The on-screen controller is a Lua overlay that
  can never be visible in a terminal, and loading it costs **8.3 MB** of
  interpreter and font machinery. Measured, not guessed.
- Together with dropping the default video track (another 5.9 MB), a playing
  session went from **101.7 MB to 88.5 MB** of mpv RSS. YTM-Player's own
  process sits at 2.8 MB.
- Resolution is not what costs memory: 144p and 480p video measured within
  0.1 MB of each other. Whether a video track exists at all is what matters.

### Fixed

- **Torn frames while ASCII video is playing.** mpv and the status bar were
  both writing to the same terminal. A tty makes one `write` atomic, but a pty
  that mpv is flooding at megabytes a second is usually near full, so a larger
  write came back short and finished in a second syscall — with mpv painting in
  the gap. Half a status bar would strand itself in the middle of the picture,
  and a cursor move chopped mid-sequence printed its trailing `H` on screen as a
  literal character. mpv's output is now piped to us and forwarded by the one
  writer that owns the terminal, cut only where no escape sequence and no UTF-8
  character is half-written.
- **A row nothing ever repainted.** mpv numbers its `tct` rows from zero but
  positions them with a cursor-move that has no row zero, so it paints one row
  fewer than the height it is given. Anything that landed on the row above the
  status bar stayed there for the rest of the session. That row is now swept
  with every frame.
- **Leftover characters after a label shrank.** Text-mode rows only overwrote
  as many columns as they had characters, so `Download(1080p)` becoming
  `Download(MP3)` left the tail behind and `[q] Quit` could read `[q] Quittt`.
  Every row now clears to the end of the line.
- **The 2.x flicker, at the type level.** The old script's
  `printf "%.2f %.2f"` crashed to `invalid number` whenever mpv answered with
  anything that was neither `null` nor a number, spamming the terminal a
  hundred times a second. Positions are now `Option<f64>`; there is no format
  string left to feed a stray value to.
- **Orphaned mpv processes.** `Mpv` and the terminal guard are RAII, and
  `Ctrl+C` is caught, so quitting by any route leaves no stray process, no
  socket in `/tmp`, and a terminal in the state it was found.
- **Video tracks leaking across playlist entries.** Re-adding a track while mpv
  was still loading the next entry silently orphaned it, one per second. A
  playlist advance now hands the terminal back and lets `v` fetch a picture for
  the new track.
- **`Tab` not reaching the live picture.** A bare `video-remove` only drops the
  *selected* track, which is nothing at all while video is toggled off, so the
  old resolution survived and was re-selected. Tracks are now added with an
  explicit select and removed by id.

### Removed

- `ytmplayer.sh`, and with it the `jq`, `socat` and `bc` dependencies. JSON is
  parsed natively, the IPC socket is a `UnixStream`, and the arithmetic is
  `f64`. A missing `bc` used to stop the player from starting at all.

## [2.0] - 2024

The Bash era: audio playback through mpv driven over a JSON IPC socket with
`socat`, `jq` and `bc`, a progress bar, play/pause, seek and playlist
next/previous.

### Fixed

- Progress bar flicker, and a duration that could render as `03:333`
  ([#1](https://github.com/Osyna/YTM-Player/pull/1), thanks
  [@ConttiDev](https://github.com/ConttiDev)).

[3.0.0]: https://github.com/Osyna/YTM-Player/releases/tag/v3.0.0
[2.0]: https://github.com/Osyna/YTM-Player/commits/main
