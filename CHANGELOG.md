# Changelog

All notable changes to YTM-Player. Format based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **An effects rack on `e`.** Echo, small-room reverb, a +8 dB bass shelf,
  nightcore, centre-cancel karaoke and a slow 8D orbit, freely combinable. Each
  is one libavfilter chain; everything enabled joins into a single `af` node
  placed *before* the visualizer tap, so the scopes measure what the ears get.
  Downloads and the stream recorder read the demuxer, upstream of every filter,
  so a saved file is never coloured by an effect. Adding one is a line in
  `src/effects.rs`.
- **Crossfade, off by default, 3-15 s (5 default).** On playlist auto-advance
  the outgoing track fades down and the incoming one fades up, half the
  configured time each side. mpv gets `gapless-audio` + `prefetch-playlist`
  while it's on, so the seam under the fade carries no silence. mpv decodes one
  track at a time, so this is a volume envelope across an instant switch, not a
  two-stream overlap: what it buys is a seamless transition, not a DJ mix.
  Manual skips still cut instantly, the queue's last track plays out clean, and
  anything shorter than twice the transition never fades. `j`/`k` during a fade
  move your level, not the envelope's.

- **An always-on status bar** across the bottom of every view, video mode
  included: transfer state on the left (cache state, a single download's
  percent and gauge, or a whole-playlist batch as `⬇ QUEUE 7/62 · 42% ▰▰▰▱ …`
  with the track it is pulling), the source and track count on the right, a
  rule between so the row reaches both edges.
- **Buttons are `(key) icon Label` and justify across the whole row** - the
  gaps stretch so the first control starts on the left edge and the last ends
  on the right, in every view. Geometry is measured in display cells, not
  chars, so double-width glyphs can't shift a row anymore.
- **Queue edit mode got its own control bar** - `(K) ▲ Up`, `(J) ▼ Down`,
  `(e) ✓ Done` - plus the panel tag shows where the grabbed row sits
  (`EDIT ▸ 2/62`) and the row wears a `↕` while it moves.

- **Live scopes with `c`, drawn natively in the TUI.** A 16-band spectrum
  analyzer with falling peak caps, a stereo braille waveform, and VU meters
  with peak-hold needles and a dB scale. The data comes from a transparent tap
  in mpv's own audio filter chain: `astats` and sixteen octave-spaced bandpass
  branches print levels through `ametadata` into FIFOs the player reads ~45
  times a second (`src/viz.rs`). No capture device, no loopback, no PCM
  copying, no new dependency — and the scopes render as ordinary widgets in a
  pane of the main view, composed with the volume rail, queue and keybar
  instead of owning the whole screen. Adding one is a struct with a `render`
  method plus one registry line in `src/visualizer.rs`.
- **A URL bar when launched bare.** `ytmplayer` with no argument opens on an
  input instead of usage text: paste a link or a local path, Enter, play.
  Bracketed paste is wired up, `~` expands.
- **`o` queues a link while playing.** An inline "ADD NEXT" prompt resolves in
  the background and inserts right after the current track — a single video, a
  whole playlist, a Spotify album (matched on YouTube), or a local path. The
  queue view and mpv stay in step; a toast confirms.
- **Clipboard watch, off by default.** Enabled in settings, any YouTube /
  SoundCloud / Spotify link copied anywhere on the system queues itself as the
  next track (wl-paste, xclip or xsel — whichever the system has). Whatever was
  already in the clipboard at startup is deliberately ignored.
- **Queue edit mode.** `e` in the playlist view, then `J`/`K` (or the buttons)
  move the selected track; mpv's playlist mirrors every move, including the
  currently playing entry.
- **Background title resolution.** Entries whose only name is a URL slug or a
  numeric SoundCloud id render dim with a `⋯` and are re-titled from real
  metadata as batched yt-dlp lookups land - no more `1382326714` rows.
- **A full-height volume rail** on the main view's right edge: click to set,
  wheel to nudge, 100% line marked, overdrive above 100% shown in red.
- **SoundCloud and Spotify links.** yt-dlp already plays SoundCloud (tracks and
  sets); Spotify links, which can't be streamed, are matched to YouTube by
  reading the public `open.spotify.com/embed` metadata — a track becomes one
  YouTube search, a playlist or album one per entry. Spotify links need `curl`.
- **Ratatui UI, fully mouse-interactive, redesigned.** Text mode is four views
  on [Ratatui](https://ratatui.rs): main (with the scope pane), queue, settings
  and the URL bar, all sharing one dark hacker-terminal look - `▛▞ YTM://PLAYER`
  brand line, `╸TAG╺` panels, electric-cyan/magenta accents. Every `(key)`
  label is a clickable button, the progress bar seeks, playlist rows jump on
  click, and the wheel scrolls lists or turns the volume. Each frame still
  reaches the terminal as one atomic write, so mpv's `tct` frames and the UI
  can't interleave.
- **Playlist view on `p`.** Every entry titled and scrollable; click or `Enter`
  jumps. `d` inside the view downloads the whole playlist with a live
  queued/percent/saved/failed column per track, and `d` again cancels. The main
  view shows the upcoming track.
- **Settings menu on `s`**, persisted to `~/.config/ytmplayer/config`: video and
  download quality (`480p/720p/1080p/Best`), save format (MP3/MP4), smart
  loading, and clipboard watch. Audio-only sources (SoundCloud, Spotify) ignore
  MP4 and always save MP3; local files download nothing.
- **Smart loading.** A Spotify playlist plays after a single search: the first
  track resolves alone, the rest match on YouTube in the background and append
  to the live playlist as they land (mpv runs `--idle=yes` so a slow match can
  never end the session early). Measured on a 13-track album: sound in 3s
  instead of 14s.
- **SoundCloud radio.** Station pages and a track's `/recommended` page play as
  playlists.
- **Local playback.** A media file, a folder of media files, or an
  `.m3u`/`.m3u8`/`.pls` playlist - decoded by mpv directly, no yt-dlp involved.

### Fixed

- **A whole batch of 12 titles could still be dropped silently.** When `yt-dlp`
  printed *nothing* usable for a batch - one dead SoundCloud link is enough - the
  resolver moved to the next chunk and those twelve rows kept their raw ids
  forever, while later batches resolved normally. Anything a batch leaves
  unresolved is now retried once, individually, so a bad link costs only itself.
- **The spectrum flattening into a solid wall on loud tracks.** Band levels are
  normalised against a fixed 64 dB floor, which is right for material mastered
  near the design target and useless for anything hotter: every band lands within
  a few percent of the top. The spectrum now levels each frame against a
  slowly-falling loudness reference (instant attack, gradual release, floored so
  silence stays flat), and loud and quiet material both read correctly.
- **A panic in a background thread left the terminal unusable.** The terminal
  guard only covered an unwind on the main thread; a panic in the tap reader, a
  resolver or a download thread left raw mode and the alternate screen on, with
  the panic message itself unreadable. A `std::panic` hook now restores the
  terminal from any thread.
- **A blank pane while a track was caching.** mpv publishes no `media-title`
  until the stream is open, so the title row rendered empty - and in video mode,
  with no picture yet, the pane had nothing in it at all. It now reads `caching…`
  or `loading…`.
- **An unresponsive mpv froze the UI for a second and a half.** The IPC read
  timeout is a stall budget on a local Unix socket, where replies take well under
  a millisecond; it is now 150 ms.
- **A wedged `yt-dlp` held the download slot forever.** After its output closes
  it gets 20 seconds to exit and is then terminated, instead of freezing the row
  mid-percentage.
- **A crash mid-save could empty the config.** Settings are written to a temp
  file and renamed over the config, so it is either the old one or the new one.
- **Abandoned `.part` files and `ytmviz_<pid>` FIFO directories piled up.**
  A killed download leaves gigabytes nobody will resume, and a SIGKILLed player
  leaves 17 FIFOs behind. Both are swept at startup - parts only once they are
  older than a day, so a download in flight is never touched.
- **Playlist titles resolving in blocks of 12 or not at all.** `yt-dlp` exits 1
  when *any* URL in a batch is dead (private, deleted, geo-blocked), even under
  `--ignore-errors`, while still printing every title it did resolve. The
  resolver treated the whole chunk as failed and threw away the other eleven
  titles. It now keeps whatever came back; a dead entry costs itself, nothing
  else.

### Changed

- `src/viz.rs` is now `src/audio_tap.rs`: capture (`audio_tap`) and rendering
  (`visualizer`) no longer sit behind two filenames one letter apart. `curl`,
  which reads Spotify links, is declared as an optional dependency in the Arch,
  Debian and RPM packaging.
- Play/pause moved from `p` to `Space` (a click on the status line still works);
  `p` now opens the playlist view.
- `Tab` quality cycling is gone; quality and save format live in the settings
  menu. SoundCloud and Spotify downloads are always MP3.
- `j`/`k` volume now follows vim: `j` down, `k` up.
- **Video mode wears the whole player UI, not a stripped bar.** The bottom rows
  are the same widgets text mode draws - title and transport chip, clickable
  progress row, two justified button rows, the status bar - rendered off-screen
  and serialized to ANSI over exactly those rows, sharing one click map with
  text mode. The hand-rolled video bar and its separate hit-test are gone.
- **A too-small terminal disables the scope and the video** rather than
  squeezing them: below the thresholds the pane's rows go to NOW, `(c)`/`(v)`
  render dim, the keys answer with a toast, the audio tap isn't even sampled -
  and shrinking a live video mode drops it back to text automatically instead
  of letting mpv stream a picture with nowhere to land.
- Visualizers no longer render through mpv's `tct` video path at all - `c` is
  pure UI state now, so cycling scopes is instant, works while a video is
  loading, never tears the terminal down, and resize just works. `v` (real
  ASCII video) keeps the `tct` path.
- mpv always runs `--idle=yes`: the session's end is the player's decision.
  URL-launched sessions still exit when the queue runs out; anything opened
  interactively (URL bar, `o`, clipboard) keeps the player up for the next
  link.

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
- **Distribution packages** attached to every tagged release: `.pkg.tar.zst`
  and a `PKGBUILD` for Arch, `.deb` for Debian and Ubuntu, `.rpm` for Fedora,
  RHEL and openSUSE, and a plain tarball for everything else. Each one declares
  `mpv` and `yt-dlp` as dependencies and suggests `ffmpeg`, so the package
  manager installs what the player needs. Built for `x86_64` and `aarch64`,
  statically linked against musl so they depend on no system library at all,
  and listed in a `SHA256SUMS` file.
- **CI** on every push: formatting, Clippy with warnings denied, tests, release
  build.

### Changed

- **Rewritten in Rust** as one binary. 2,524 lines across six modules, 731 KB
  built (836 KB as a static musl binary), 35 crates in the tree and four direct
  dependencies.
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

[Unreleased]: https://github.com/Osyna/YTM-Player/compare/v3.0.0...HEAD
[3.0.0]: https://github.com/Osyna/YTM-Player/releases/tag/v3.0.0
[2.0]: https://github.com/Osyna/YTM-Player/commits/main
