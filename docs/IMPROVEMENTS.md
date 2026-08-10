# Improvement tiers

Five self-contained tiers, ordered by ambition rather than priority. Each is
shippable on its own; the sequencing notes at the end say which ones unblock
which. Every item names the code it touches — nothing here is generic advice.

Baseline at time of writing: 9,263 lines across 11 modules, 72 unit tests, zero
integration tests, `Player` at 51 fields / 85 methods, two mpv decks.

---

## Tier 1 — Finish the music player

*Done.* Shuffle, repeat, ReplayGain, tagged saves, search and a command line all shipped.

Gaps a user hits on day one. Days of work, low risk, no architecture moved.

- **Shuffle and repeat.** A music player without them is conspicuous. mpv has
  `playlist-shuffle` / `playlist-unshuffle` and `loop-playlist`; both are one
  property away. Must mirror onto the parked deck (`Player::mirror_playlist`,
  and shuffle has to happen *before* the deck is cued or the two disagree about
  what comes next). Two settings rows, two keys.
- **Loudness normalisation — `--replaygain=track`.** This is now urgent rather
  than nice: a real overlap between a −18 LUFS master and a −6 LUFS one sounds
  broken no matter how good the curve is. One mpv option per deck, set alongside
  `set_gapless` in `Player::sync_seamless`. Protects the feature just built.
- **Tag what we save.** `DownloadSpec::ytdlp_args` passes `-x --audio-format
  mp3` and nothing else, so every saved file lands with no artist, no album, no
  cover. Add `--add-metadata --embed-thumbnail --parse-metadata` (gated on
  ffmpeg, like the transcode already is). Three strings.
- **Search instead of paste.** `ytsearch:` is already used internally to match
  Spotify tracks (`youtube::resolve_search_pairs`). Let the Open bar and `(o)
  Add` accept plain text: if `youtube::kind_of` says it is not a URL or a path,
  resolve it as `ytsearch10:` and show the hits in the centre panel — the queue
  pane already renders exactly that list.
- **`--shuffle`, `--volume`, `--version` on the CLI.** `display_usage` documents
  keys but the binary takes no flags at all.

## Tier 2 — Make it trustworthy

*Two of four done.*

- ~~**Integration tests.**~~ Done: `tests/live.rs` drives the real binary on a real
  pseudo-terminal and cross-examines mpv over its own socket. Six tests cover the
  deck swap, the shuffle-order invariant, resume, panel cycling, the library, and
  a wedged mpv. `libc` is a dev-dependency only; CI installs mpv and ffmpeg so
  they cannot silently skip.
- ~~**Stop swallowing failures.**~~ Done, at the boundary rather than at eighty
  call sites: `Mpv` counts consecutive missed replies, and five in a row put
  `⚠ MPV NOT RESPONDING` in the status bar until it answers again.
- **Break up `Player`.** Still outstanding, and now the largest single piece of
  debt: 60-odd fields and 100-odd methods in a 2,900-line `main.rs`, having
  absorbed a crossfade state machine, a library pane, a history, a resume point
  and an MPRIS pump since this was written. The seams are unchanged - `session`
  (entries, resolution, playlist map, downloads), `engine` (the deck pair,
  crossfade, tap, recorder, video), `input` (key and click dispatch) - and the
  live tests now make the move safe in a way it was not before.
- **A crossfade that cannot silently no-op.** `cross_refused` is still set in
  several places and never surfaced.

## Tier 3 — Desktop citizen

*Two of four done:* MPRIS2 (hand-marshalled, no new dependency) and session
persistence (resume + play history). Cover art in the terminal and configurable
keys/theme are still outstanding.

Make it behave like an application, not a script. 2–4 weeks.

- **MPRIS / D-Bus.** The single biggest "feels native" win on Linux: media keys,
  the GNOME/KDE panel, `playerctl`, notification popups on track change. Maps
  almost one-to-one onto what `Snapshot` already reads and what `Action`
  already dispatches.
- **Cover art in the terminal.** The kitty graphics and sixel protocols are a
  natural extension of work already done for true-colour `tct` video, and the
  centre panel is now a general-purpose box that could hold an artwork pane as a
  fourth `Pane`.
- **Session persistence.** Resume where playback stopped, a play history, named
  saved queues. `settings.rs` already owns a config directory; this is a second
  file in it.
- **Configurable keys and theme.** The palette is one `const` block at the top of
  `ui.rs` and the key map is two `match` blocks — both are a config table away
  from being user-editable, and the click map would follow for free because keys
  and clicks already share the `Action` vocabulary.

## Tier 4 — Become a DJ tool

Leverage the thing that is now unusual: two synchronised decks in a terminal.
Months, high risk, genuinely differentiating.

- **Beat-aware transitions.** The audio tap already streams per-band energy ~45×
  a second; onset detection and a BPM estimate are a small step from there. Align
  the swap to a bar boundary instead of a wall clock, and the overlap stops
  sounding like two songs and starts sounding like a mix.
- **Tempo match.** `speed` plus `audio-pitch-correction` on the incoming deck,
  nudged to the outgoing BPM across the overlap.
- **A real crossfader.** The centre panel is already a clickable pane with a
  vocabulary of actions; a draggable fader that takes manual control of the two
  deck gains is a natural fifth `Pane`, with cue/preview on the parked deck.
- **Transition styles.** Equal-power is one curve. Bass-swap (kill the outgoing
  lows through the existing `af` chain), echo-out, and hard-cut-on-the-one are
  each a few lines in `effects.rs`, which is already a registry.
- **Hot cues and loops**, stored per track alongside the history from Tier 3.

## Tier 5 — Platform

*Foundation done:* a local library index with fuzzy search, and a history behind
it. The daemon and thin client, the unified cross-source catalogue and scrobbling
are untouched — and the daemon still subsumes much of Tier 3, which is now built.

Stop being one process with one frontend. Quarters.

- **A library.** Index a local music folder into a metadata database, fuzzy
  search across it, smart playlists. `youtube::local_entries` already walks
  directories; everything after that is new.
- **One catalogue across sources.** YouTube, SoundCloud, Spotify and local files
  are four resolvers behind one `Source` today; unified search with dedup across
  them is the natural conclusion.
- **Daemon plus thin client.** Split playback from the UI so a session survives
  the terminal closing — attach and detach like tmux. The TUI becomes one
  frontend, MPRIS another, an HTTP API a third. This is the change that makes
  Tier 3 and Tier 4 compose instead of compete.
- **Scrobbling.** Last.fm / ListenBrainz, on top of the history from Tier 3.

---

## Sequencing

- Tier 1's **ReplayGain** is a prerequisite for Tier 4 sounding like anything:
  beat-matching two tracks at different loudnesses is wasted effort.
- Tier 2's **integration tests** are a prerequisite for Tier 4 being safe. The
  deck pair has four moving parts (cue, pin, ramp, swap) and no automated
  coverage; adding beat detection on top of that without tests is how the
  crossfade breaks again without anyone noticing.
- Tier 2's **`Player` split** should happen before Tier 3, not after: MPRIS wants
  a clean view of playback state, and today that view is 51 fields wide.
- Tier 5's **daemon** subsumes a lot of Tier 3. If the platform direction is
  wanted at all, do it before investing in desktop integration that will have to
  move.
