# Changelog

All notable changes to YTM-Player. Format based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **The effects rack is 38 filters, rekordbox-style.** It was six. Delays (slapback,
  ping pong, a darkening tape delay), reverbs up to a cathedral, flanger, phaser,
  chorus and doubler, tremolo and vibrato, hard square `Trans` and `Stutter` gates,
  `Crush` and `Lo-fi` bit reduction, telephone/megaphone/radio band-limiting, DJ low-
  and high-cut, sub bass and air, `Wide` and `Mono`, `Pump` and `Glue` compression,
  saturation, gating, de-essing, and rate tricks from `Nightcore` through `Screwed` to
  `Half speed`. Every filter string is checked against a real ffmpeg, because mpv
  accepts an `af` it cannot parse and reports success - a typo in one of these would
  be silent, and silence is the one failure a list this long cannot afford. The view
  scrolls and drops its double-spacing once the rack outgrows the terminal, rather
  than drawing the first screenful and losing the rest.
- **A track's tempo is written into the file, and read back from it.** Analysing a
  track is an ffmpeg decode of the whole thing; a file that already says what it is has
  been paid for once already. Once a local file (or a downloaded one) has been
  surveyed, the BPM goes into its `TBPM`/`BPM` tags by remux - `-c copy`, so the audio
  is bit-identical, into a sibling temp file renamed over the original so a failure
  mid-write cannot destroy somebody's music. The library scan reads the tag back and
  seeds the analysis cache with it, so a scanned collection is ready to mix at t=0 and
  says how many tracks already knew their tempo. Unlike the player's own cache, a tag
  travels with the file when it is copied, backed up or moved - and every other DJ tool
  reads the same frame.
- **A track already saved to disk is not captured again.** The stream recorder exists so
  that pressing `d` on a stream is instant rather than a fresh download; a track already
  in the download index is past that, so it is no longer re-captured on every play.
- **An equalizer, on `g`, with a view of its own.** Nine presets over a fixed
  five-band shape - `Flat`, `Club`, `Deep`, `Warm`, `Bright`, `Vocal`, `Loudness`,
  `Laptop`, `Late night` - each named for the situation it is for rather than the
  numbers it holds. Presets rather than five faders on purpose: the answer a listener
  wants is "make this sound right on these speakers", not "set 3.5 kHz to +4".
  - The view draws the preset under the cursor as a curve about a zero line, at
    half-block resolution and with boosts and cuts in different colours, so a shape
    can be read before it is heard. The cursor previews; `Enter` commits; a `●` marks
    the one actually in use, because "where am I looking" and "what am I hearing" are
    two questions and this view has to answer both at once.
  - Anything that boosts pays for it first. A `+8 dB` shelf on a master already mixed
    to within an inch of full scale does not make a louder low end, it makes a clipped
    one, so each preset takes back a little over half its largest boost before
    applying any of it. `Flat` emits no filter at all rather than five filters at
    0 dB, so the default costs nothing and "off" is unambiguous.
  - It joins the same `af` graph the effects rack and the scopes already share, ahead
    of both: tone first, effects on top, the tap last, and the same order whether or
    not a transition is running - so turning one on cannot quietly re-order the chain
    underneath the listener. Every emitted filter form was run through a real ffmpeg
    and a real mpv.
- **A fade curve, over every transition at once.** `Fade curve` in settings -
  `Linear`, `Smooth`, `Bezier`, `Late`, `Early` - restyles all twenty-one scores and
  the plain fade with them, so it is worth something even with beat mixing off, and
  no `.mix` file has to know it exists. It warps the transition's *clock* rather than
  its values, which is the whole reason it can be applied to everything safely: a
  `power-down`/`power-up` pair is `cos`/`sin` of one angle, so warping the angle moves
  both and `out² + in²` stays pinned at one; a `hold` still holds, because holding
  does not read the clock, so no curve turns a cut into a fade; and a drop still drops
  on the key it was written on. `Bezier` is a real cubic Bézier solved for its
  parameter, not something shaped roughly like one. `Linear` is the default and a true
  no-op. Lanes are read through the curve and hooks are not - a fader move is exactly
  what a curve is for, and a beat roll that starts a third of a bar late is not a beat
  roll.
- **Three new transitions, each the shortest honest example of something the language
  could already say and nothing shipped using.** **Beat roll** catches the leaving
  track's last bar and retriggers it at halving intervals - half a bar, a quarter, an
  eighth - into the swap, which is `loop` and mpv's own `ab-loop`, not a control
  moving. **Gate out** chops the leaving track away on eighths, which is `every`
  writing a square wave in one line instead of thirty-two hand-written keyframes.
  **Half time** pulls it down to half speed - a ratio the music actually has, unlike
  the brake, which stops dead - and cuts on the bar with its top end already gone.
- **Beat mixing is now a setting of its own, and the two modes are honest about what
  they cost.** Off - the default - is a clean equal-power fade: no analysis, nothing
  decoded ahead, video works. On, both tracks are measured, matched and put in time
  with each other and the chosen score runs, and video is off while it is. The
  settings row says which you are getting and what it costs, and the transition
  chooser says so too rather than sitting there inert with no explanation.
- **Tempo is surveyed, not sampled.** Four windows spread through the track, and the
  answer is the one they agree on - folded across octaves, so 87 and 174 count as
  agreeing. One look is not agreement and is refused.

- **Cover art, in a box beside the title.** Embedded pictures, a `cover.jpg` next to
  the track, or one carried by a stream; a real image on terminals with kitty
  graphics and half-blocks everywhere else. No picture means no box - the title,
  progress and next-up take the full width they always had, rather than sitting
  beside a permanent empty square. `artwork.rs` had been written, verified and left
  unwired for exactly this reason; it is wired in now.
- **A queue entry that failed says which kind of failure it was.** One that will
  never play is dimmed in a dark red and marked `✗ unplayable` - dark enough to read
  as absence rather than as an alarm, because a dead entry in a long queue wants to
  be skipped over by the eye as it is by the player. One whose *name* could not be
  looked up still plays perfectly well, so it stays legible and is marked `✗ no name`
  in amber. Neither now sits there showing a row of dots as though something were
  still happening: the title resolver reports giving up instead of going quiet, so a
  raw id with no name is no longer indistinguishable from one still being fetched.
- **The automix skips what will not play.** mpv's own playlist already leaves out the
  entries that never resolved, but the fallback past the end of it walked our list,
  which still holds them - and a transition cued onto one of those is a transition
  into silence.

- **A peak meter on the volume rail**, two columns beside the fader: left and right,
  a solid bar to the level now and a mark left behind at the highest it has been.
  Green while there is headroom, amber where a mix starts to crowd, red at the top -
  a threshold to glance at rather than a gradient. Rise instantly, sit for a second,
  then fall: the ballistics are the ones every hardware meter uses, because a
  transient is a millisecond and a frame is forty, so the interesting part is almost
  always between frames.
- **The player says what it is doing to the next track.** `◈ ANALYSING Track Two ·
  reading its beat with ffmpeg` while the measurement runs, then `◈ 132.1 BPM ▸ Track
  Two · pulled -3.2% and locked`. This is the one part of the automix with no outward
  sign that it happened, and it is knowable several seconds before anyone hears it.
  It now outranks the loading notice, which happens at the same moment and says less.
- **The deck learns its own start lag.** Telling a paused deck to seek and then to
  play costs tens of milliseconds, so the arriving track used to land that far behind
  and spend its first seconds being pulled into line. The first landing of a session
  is measured and fed forward, eased rather than followed, and bounded at both ends -
  a landing far enough out is a misread grid rather than a slow deck, and is ignored
  rather than carried into every transition after it.

- **The sync holds.** Measured live between two decks, the arriving track now sits
  about five milliseconds off the one it is mixed with, where it was 234 ms and
  sliding by a further 400 ms across the blend. A flam is audible from about fifteen.
  Four faults, all the same shape - two quantities that should have been one:
  - The two tempos came from different estimators, so their quotient was 0.971 for a
    pair of identical files, and three per cent of drift followed. Both tracks now go
    through the same one, where the bias cancels instead of accumulating.
  - A grid is a bar line and a spacing, so using it far from where it was read
    multiplies the spacing's error by the bars in between. Both grids are now re-read
    every few seconds instead of once at the cue.
  - The correction loop compared a snapshot position against a live one and drove the
    difference to zero, which held the two decks apart by however old the snapshot
    was. Both are read together now.
  - The release waited for a bar line on one grid having seeked the deck on another,
    and the wait was quantised to the redraw tick besides. Nothing waits now: the
    phase is *chosen* by where the arriving track is started.

- **Sync, properly.** The arriving track is measured before it is audible, pulled to
  the playing one's tempo when the stretch is under six per cent, **and seeked onto
  one of its own bar lines** before being released on one of the playing track's - so
  the two count the same bars instead of merely running at the same speed. Then the
  score walks it back to its own tempo. Four new transitions built on it: `sync
  blend` (locked through the blend, eased back), `sync cut` (locked, then cut on the
  bar), `tempo ride` (arrives at the old tempo and climbs into its own) and `double
  drop` (both hollowed out and locked, then everything at once). Eighteen in total.
- **A sync is one intent and can no longer be declared two ways.** A `tempo` lane is
  what asks for it, because it is also the only thing that releases it; the header
  flag is now derived from the lane, and a header that disagrees is refused at read
  time. `slam.mix` shipped claiming a tempo match with no lane to release it, so it
  paid for a decode and did nothing - silently, which is exactly what that check now
  prevents.

- **Eight more transitions**, each a technique with a name: highpass out (thin the
  leaving track to its hats), filter in, brake (stop it dead, pitch and all), drop
  out, drop swap, slam, radio fade and vocal swap. Fourteen in total, all of them
  files in `transitions/` and all of them editable without a rebuild.
- **Two new controls to write them with.** `highpass` - four of the new scores need
  it, and taking the bottom out is the more common move on a busy floor than taking
  the top - and `speed`, an absolute rate on top of any tempo match, with pitch
  correction *off*, because a brake whose pitch does not fall is not a brake. Every
  filter string the language can now emit, all 62 of them, was run through a real
  mpv; none was rejected.

- **Loop rolls.** A hook can now make a deck's playback jump backwards instead of only
  changing a control: `loop out 1/2` A-B loops the last half of a bar behind the
  playhead of the named deck, `loop out 1/4` tightens it, `loop off` lets it play on.
  Uses mpv's own `ab-loop-a`/`ab-loop-b` rather than decoding and feeding back a
  buffer, so it costs two property writes, and it is swap-aware - a roll started
  before the decks trade places and still running after does not end up looping the
  wrong one. The classic beat-roll build into a drop is three or four of these in a
  row, each fraction smaller than the last.
- **Pattern lanes.** `every 1/16 from bar 12 to bar 14   1.0 0.0   hold` writes a
  whole gate or stutter in one line - the same alternating keyframes as writing
  them out by hand (thirty-two of them, for that example), expanded by the parser
  before the engine ever sees them. `from`/`to` are always bars: a step is a
  fraction of a bar, and a fraction of the whole transition is not a length a step
  can repeat against.
- **Cover art for streams.** `artwork.rs` could always read a picture from a local
  file or a direct URL; nothing gave it one for a YouTube or SoundCloud page, so the
  cover box was local-only. The same `yt-dlp` process that resolves a stream's
  playable URL for analysis now asks for its thumbnail too - one more `--print`, no
  extra process - so a streamed track gets its cover the same as a downloaded one,
  and the resolution used for the beat-mixing engine and the one used for the cover
  box are, for once, the same network round trip.
- **A track measured once is ready to mix at t=0 forever.** Tempo and downbeat are
  now written to `$XDG_STATE_HOME/ytmplayer/analysis.jsonl` as they are learned, keyed
  by URL, and read back before a fresh probe is even considered: a track played in a
  previous session shows `analysed, ready to mix` on the first redraw instead of
  waiting on a network read it has already paid for once. The deck-start lag estimate
  is carried the same way, in the resume point, so a fresh session does not begin the
  correction loop at zero either.
- **`echo-out` is an actual echo.** It used to be a top-end lift wearing the name - a
  fixed EQ shape, switched on for the whole move, because a hook could turn a filter
  on and off and nothing more. `echo` is a lane now, `0.0`..=`1.0` of `aecho` feedback
  at a fixed 350 ms delay, and `echo-out.mix` rides it up from nothing as the gain
  rides down: a genuine, decaying series of repeats bounded by the same fade that used
  to just switch a filter off, instead of a tail that either never ran or never
  stopped. Added the same way `highpass` and `speed` were - a field, a parser word, a
  filter string run through a real ffmpeg and a real mpv.
- **A track already saved to disk says so, and is used.** A download used to be
  indistinguishable from the recorder's own ephemeral stream cache once it finished -
  both just sat there until the next thing overwrote them. The status line now says
  `⬇ DOWNLOADED` instead of `⚡ CACHED` for a track that has actually been saved
  (`$XDG_STATE_HOME/ytmplayer/downloads.jsonl`, one entry per URL, the same shape and
  the same reasons as the analysis cache), the queue marks it `✓` from the first
  redraw of a session that reopens a playlist partly downloaded in an earlier one, and
  the analysis probe reads the saved file instead of resolving and streaming the URL
  again - so mixing into a track that has already been paid for costs a disk read,
  not a network round trip.

### Changed

- **The README is a pitch now, not just a reference.** The old one opened straight
  into a wall of bullet points; this one leads with a hero screenshot, a nine-line
  "what it does," and a six-image gallery (now playing, queue, scope, library,
  the FX rack, the equalizer) before any of the detail. The full feature list is
  still there in full, just behind a `<details>` fold instead of first. All seven
  screenshots are freshly captured against the current build - the old four
  predated the effects rack expansion, the equalizer and the keybar rework, and
  two views (the library browser, the effects rack) had never been screenshotted
  at all. Two stale facts caught in the process and fixed: "three views" (it is
  four, the library pane included) and "five dependencies" (`unicode-width`
  makes it six).

- **The keybar reads `icon Label (key)` now, grouped into zones.** It used to be
  `(key) icon Label`, evenly spaced with nothing to say where one idea stopped and
  the next began - seven controls in a row look like seven equally important
  things even when they are not. Playback, seek and volume are now three visibly
  separate clusters on the transport row; panel actions, downloading, the panel
  switches and quit are four on the feature row, each pair split by a thin `│`.
  The least essential controls still drop first on a crowded terminal, exactly as
  before; if that empties a whole zone, its divider goes with it rather than
  being left to mark a group that is no longer there.
- **A white rule now separates the keybar from the status line under it.** Every
  other border in the chrome is a panel's own faint edge; this one is not framing
  anything, it is the split between controls (something a hand reaches for) and
  status (something read, not pressed), so it is `BRIGHT` rather than `faint()` -
  same weight and the same `─` as everywhere else, just not the same colour. In
  both text mode and video mode, one row.
- **The next track is analysed the moment it is known, over the whole song, in one
  read.** Three changes with one cause. The analysis used to start at the cue - six
  seconds before the overlap - and read four seeked windows, each its own ffmpeg run,
  and for a stream its own HTTP connection and its own chance to fail. Now the whole
  track is read once, sequentially - the one access pattern every server handles -
  starting as soon as the track becomes next, with the entire length of the current
  song to finish in. A four minute track yields eight or nine voting windows instead
  of four fixed corners, the decoded frames are kept so every later question - the
  bar line near the release, the mid-mix grid refreshes - is answered from memory
  instead of the network, and the cue retries once if the early attempt hit a blip.
  Measured: a three minute stream analysed in 2.5 s, tempo within 0.2%, over one
  connection. The status line shows `◈ 91.8 BPM ▸ next · analysed, ready to mix`
  from early in the track, so "did the analysis work" is answered while there is
  still time to care; "no steady beat found" is only ever said during a cued
  transition, after the retry, about a track that genuinely has none.

- **The score language refuses two more mistakes at read time**, both of which used
  to be silent:
  - **A moment the transition never reaches.** `bar 20` in a sixteen bar score is not
    a late instruction, it is one that never runs - and the endpoint check passed it
    happily, because the value written there was correct. A file that said the leaving
    deck reached silence, and a leaving deck cut off at full volume every time.
  - **An effect switched on and never off.** The effect rack belongs to the player,
    not to the transition, so one left on is left on for the rest of the night. The
    engine now also puts back anything a score switched on when a transition is
    abandoned part-way, which is the case no read-time check can cover.

- **Fades can now run to thirty seconds**, up from twenty. Eight bars either side of
  the join at 128 BPM is about thirty; past that the two tracks stop being mixed and
  start being played at once. The player still declines to overlap at all unless the
  track is at least twice the span, so a thirty second fade wants a track of a minute.
  The range is read from the bounds now rather than written out beside them - the
  settings note said `3–20 s` in three places, and the test that guarded the ceiling
  asserted a copy of the number instead of the constant, so both had to be edited by
  hand every time it moved.

- **A fade length is now the whole move, and the score says where the join falls
  inside it.** Fifteen seconds means fifteen seconds: seven and a half before the
  old track ends and seven and a half after, for a plain fade. What decides the
  split is where a score's outgoing fader reaches zero, because that moment has to
  land on the end of the outgoing track - a deck cut off with its fader still up is
  a hard stop no curve can hide. The maximum is now 20 s, and the settings row shows
  the split (`15 s = 7.5+7.5`) rather than a number that could mean either thing.
- **The decks trade places when the leaving track runs out, not when the score
  finishes.** They are different moments for any score with a tail: the long blend
  spends its last four bars on the arriving track alone, and until now the interface
  went on naming the old one throughout them.

- **Transitions are files now** (`transitions/*.mix`, `src/score.rs`). Every one,
  including the six that ship, is written in a small readable language and read by
  the same parser at startup - so the built-ins are also the examples, and what you
  copy is what runs. Drop a `.mix` file in `~/.config/ytmplayer/transitions` and it
  appears in the settings menu; give it the name of a built-in and it replaces that
  one. No rebuild, no Rust.
- **Hooks.** Lanes cover what a mixer's controls do; a hook covers the rest, so a
  score can express a whole move: `hook bar 12 effect Echo on`,
  `hook at 1.0 toast here it comes`. Each fires once on the way past.
- **A file with a mistake in it is refused at the line, and says so on screen.**
  Unknown words are errors rather than defaults - an ease is the difference between
  a fade and a drop, and guessing would make a typo sound like a decision - and the
  rules that used to be tests are now read-time checks: keys in time order, a fader
  for both decks, exact endpoints, one lane per control, and no claiming constant
  power while emptying out. One bad file of yours costs you that file.

- **Transitions are a small declarative language now** (`src/transitions.rs`,
  `docs/WRITING-TRANSITIONS.md`). A transition is a `Recipe`: lanes of keyframes
  over named controls - gain, bass, mid, high, lowpass, tempo - on either deck,
  timed in bars or in fractions, with easings including a hold-and-jump for a drop
  and a `cos`/`sin` pair for anything that has to keep its loudness. Adding one is
  adding a `Recipe` to a list; there is no match arm to extend and no maths to
  write. Six tests enforce the rules a new score has to obey - keys in time order,
  a fader for both decks, exact endpoints, constant power where it is claimed,
  stable filter chains, and nothing emitted outside the vocabulary that has been
  checked against a real mpv.
- **The long blend**, sixteen bars, written in that language and played out in
  full. Both tracks run together from the first bar, tempo-locked, the arriving one
  silent with its bass killed and its middle pulled back. Four bars take the low end
  out of the leaving track, four take its top while the arriving one comes up and is
  restored, four sweep it out under a lowpass and fade it down - then four bars of
  the new track alone with no bass at all, and on the sixteenth downbeat its low end
  arrives in one step. It is let back to its own tempo as it goes.
- **Tempo matching** (`src/preview.rs`). The tap only hears the deck that is
  playing, so the arriving track is decoded separately - one ffmpeg process running
  the same sixteen bandpasses the tap does, agreeing with libavfilter's own output
  to 0.002 dB and with the live tap to a correlation of 1.00000 at matched rates.
  About ninety milliseconds for a local file and a third of a second over the
  network, on its own thread, started at the cue. The arriving deck is pulled to
  the playing track's tempo only when the stretch is under six per cent, because
  beyond that it stops sounding like a mix and starts sounding like a fault.

- **An automix with transition styles** (`src/transitions.rs`, `src/analysis.rs`).
  Crossfade is no longer one curve: it is a choice of five, set in the settings
  menu and applied by the player itself - crossfade, bass swap, filter sweep, echo
  out, cut on the beat. Each returns gains *and* an mpv filter chain per deck, so
  a bass swap really does hold the arriving track's low end back while the leaving
  one's is pulled out. Every style is trigonometric in a warped angle rather than a
  warped pair of gains, so `out² + in²` stays pinned at 1 and none of them can dip
  in the middle. Filter strings are quantised, so a transition rebuilds mpv's graph
  a handful of times rather than thirty times a second.
- **Transitions land on the bar.** For the styles that swap rather than blend, the
  player reads a beat grid out of the band levels the scopes already measure -
  spectral flux, autocorrelation for tempo, a comb for phase - and holds the
  transition, never more than a bar and never past the end of the track, so the
  swap falls on a downbeat. `♪ 128 BPM` in the status bar when it has a grid,
  `♪ ON THE BEAT` when it used one. The tracker refuses to answer on silence,
  noise or a held tone rather than inventing a tempo, because a player that cuts a
  song on an invented one is worse than a player that never cuts on the beat.
- **Real artist and album, everywhere.** mpv carries the container tags; they now
  reach the NOW panel (`♪ Aphex Twin · Windowlicker`) and MPRIS. The desktop was
  previously being told the artist was "Local" or "YouTube", which is what every
  lock screen in the house was showing.
- **The library takes folders it could not have guessed** - `A` in the library
  pane asks for one, and the index remembers it, so a collection on a second disk
  is one keystroke away rather than impossible.
- **`+` queues from the library** instead of only playing next: picking a track
  and building an evening are different intentions.
- **A refused transition says why.** `cross_refused` was set in three places and
  surfaced in none, so a crossfade that declined to happen was indistinguishable
  from one that was broken - which is the complaint that started the whole
  subsystem.
- **Three more live tests**: the automix applies the chosen style to the decks,
  it lands a transition on the beat, and the desktop can see and drive the player
  over MPRIS.

- **Integration tests that start a real player** (`tests/`). Six of them drive the
  binary through a real pseudo-terminal - a small screen model parses the frames,
  a second client on mpv's own socket says what the decks are really doing - and
  then assert the things a buffer test cannot see: that a crossfade has *both*
  decks audible at once and then trades them over, that a shuffle leaves the two
  decks holding the same order (they cue by index, so a disagreement plays the
  wrong track), that a resume lands on the track and the second, that the centre
  panel cycles and its controls follow, that the library indexes and filters.
  `libc` is a dev-dependency only, for `openpty`; nothing reaches the binary or
  the packages. On a machine without mpv the tests say so and skip, and CI now
  installs mpv and ffmpeg so they cannot skip there.
- **A wedged mpv is reported instead of drawn over.** Every IPC call site
  discards its `Result` on purpose - a volume write that fails once is noise -
  but a run of missed replies is not a busy player, it is a stuck one, and the UI
  had no way to know. `⚠ MPV NOT RESPONDING` now outranks everything in the
  status bar except a toast, after five consecutive misses so it cannot cry wolf
  at a track change, and clears the moment mpv answers again. Tested by
  `SIGSTOP`ping the real mpv mid-playback.

- **MPRIS2 over D-Bus** (`src/mpris.rs`), hand-marshalled from the wire protocol
  up so it costs no new dependency and the static binary stays static. Media
  keys, the shell's now-playing popup and `playerctl` all drive the player:
  play/pause, next, previous, seek, absolute position, volume, `OpenUri`,
  `Raise`, `Quit`. Publishing is diffed, so nothing is emitted while nothing
  changes; commands are drained before each frame is published. No session bus
  is not an error, it is absence.

- **Shuffle, repeat and loudness matching.** Shuffle reorders mpv's real
  playlist and re-derives our own index map from the result, so the crossfade's
  parked deck - which cues *by index* - is rebuilt in the same order rather than
  shuffled separately into a different one. Repeat covers the track and the
  queue. Normalisation is mpv's `replaygain`, pushed to both decks: an overlap
  between a quiet master and a loud one is a volume jump in the middle of the
  transition, however good the curve.
- **Saved files are tagged.** `--embed-metadata --embed-thumbnail` plus
  `--parse-metadata` fallbacks from `artist`/`uploader` and
  `album`/`playlist_title`, so downloads land with an artist, an album and cover
  art instead of a bare title. Gated on ffmpeg, like the MP3 transcode already
  was; without it a save is still a correct file, just an untagged one.
- **Type a search instead of a link.** Anything that is neither a URL nor a path
  becomes `ytsearch12:`, which yt-dlp returns in exactly the shape a playlist
  has - so the first hit plays and the rest are in the queue, and nothing
  downstream had to learn that searching exists. Works in the URL bar, in
  `(o) Add`, and on the command line, where multiple words are rejoined so
  `ytmplayer daft punk` needs no quotes.
- **A command line.** `--shuffle`, `--volume <0-150>`, `--version`, `--` to end
  flag parsing. Unknown flags are an error rather than a silently ignored typo.
- **A library pane, and a play history behind it** (`src/library.rs`,
  `src/store.rs`). `R` indexes `~/Music` and `downloads/` through `ffprobe` on a
  cancellable background thread, capped at eight concurrent probes; the index is
  cached by path and mtime, so a rescan of a thousand unchanged files makes no
  probe calls at all. `/` searches it as you type, ranked across four tiers so a
  typo or half an artist name still lands. With nothing indexed the pane lists
  what has actually been played instead.
- **Resume.** Where playback stopped is written every five seconds and offered on
  the next bare launch as `(Tab) ↺ Resume`, which reopens the session, jumps to
  the entry and seeks to the second. A queue that ran out clears the point rather
  than offering to replay its own ending.
- **Settings grew to ten rows** and now single-space and scroll rather than
  demanding a 26-row terminal.

- **A real crossfade: the next track starts while the current one is still
  playing.** mpv decodes one track at a time, so the player now runs two of them
  and they trade places at every transition. Both decks hold the same queue, so
  the one taking over already knows where it is and carries on advancing. The
  parked deck is cued seconds ahead and waits paused at zero - which is what lets
  a streamed entry, one yt-dlp resolve per advance, arrive on time - then the two
  are ramped past each other on an equal-power `cos`/`sin` pair, the curve that
  holds loudness flat where a linear one dips. The outgoing deck is pinned with
  `keep-open=always` for the length of the overlap so it cannot race into the
  entry the other deck is already playing. Pausing freezes the transition with
  both decks and resumes it where it left off; skipping by hand still cuts;
  video mode plays the seam straight, since the picture belongs to one deck.
  Crossfade off spawns no second process at all.
- **The centre panel: one box, three views, chosen on `v`.** The queue used to
  be a full-screen view you left the player to visit, the scope used to be the
  only thing the main view's middle panel could hold, and video used to be an
  unrelated mode. They are one choice now. `v` opens a chooser popup over the
  main view - queue / scope / video, with the scope's style on `h`/`l` - clicking
  the panel steps to the next view, and `p` still jumps straight to the queue.
  The chooser greys out and explains whatever it cannot show: an empty queue, a
  scope on a terminal too small for one, a track with no picture.
- **The controls zone follows the panel.** The feature row is rebuilt from what
  the centre panel is showing: the queue brings `(E) ✎ Edit` and `(a) ⬇ All`,
  and swaps both for `(K) ▲ Up` / `(J) ▼ Down` / `(E) ✓ Done` in reorder mode;
  the scope and video panes leave the row to the player's own controls. One
  builder, shared by the text and video rendering paths.
- **The queue is navigable without leaving the main view** - `↑`/`↓`,
  `PgUp`/`PgDn`, `Enter` to play, and the wheel while the pointer is over the
  list - with the NOW panel, transport and status bar on screen throughout.


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
  `(E) ✓ Done` - plus the panel tag shows where the grabbed row sits
  (`EDIT ▸ 2/62`) and the row wears a `↕` while it moves.

- **Live scopes, drawn natively in the TUI.** A 16-band spectrum
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
- **Queue edit mode.** `E` with the queue in the centre panel, then `J`/`K` (or
  the buttons) move the selected track; mpv's playlist mirrors every move, including the
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

- **`Player`'s mixing engine and measurement pipeline are their own modules.**
  `main.rs` was 5,604 lines with one 3,900-line `impl` block; the two halves of the
  automix - the crossfade lifecycle and sync loop, and the probe/analysis pipeline
  that feeds it - collided on unrelated state in the same block every time either was
  touched. They are `src/mixer.rs` and `src/measure.rs` now, `main.rs` is 3,319
  lines, and not one function moved without moving whole: every function that existed
  before this exists after it, once each.
- **Five parallel per-entry caches are one.** `probed`, `surveyed`, `analysed`,
  `resolved_media` and `ahead_tried` each ran their own retain/push/cap dance, at
  five different times, on the same entry index - the shape that produced the
  settings-row bug, the fade-range bug and the 0.971 tempo ratio, wearing five hats.
  One `Vec<(usize, TrackFacts)>` now, one eviction policy, one place playlist
  mutation has to invalidate instead of five (and toggling beat mixing off now
  actually clears all of it, which it silently did not before).
- **One `yt-dlp` process resolves a stream instead of two.** The media URL and the
  thumbnail used to be separate concerns; `--print "%(url)s" --print "%(thumbnail)s"`
  in the one process that already ran gets both, the same trade video mode already
  made for its own resolution.
- **The release binary is about 9% smaller.** `panic = "abort"` in the release
  profile drops unwind tables from a binary that already treats every panic as
  fatal - nothing catches one. Measured: 1.79 MB → 1.66 MB.
- **Cached analysis frames use a quarter of the memory.** Whole-song band levels were
  kept as `f32`, four bytes for a value that is 0.0..=1.0 and only ever compared or
  differenced - `u8` quantisation (dequantised back to `f32` at the one place
  anything reads a level) drops a cached track from about 7.7 MB to about 2 MB, times
  the three tracks the cache holds. The beat tracker's own accuracy is unaffected;
  it never sees the difference.
- **The live test suite is split by whether it cares about wall-clock time**, and the
  half that does not (settings, queue, covers, fault handling) now runs with cargo's
  default parallelism instead of one thread. Fixture tracks are also cached by their
  generation parameters (`/tmp/ytmfix_<hash>.mp3`) rather than re-encoded with `lame`
  on every run.

### Fixed

- **The fade-length row said `20 s = 13.5+6.5` and was read as two tracks.** It is one
  span with the old track's end inside it - both audible for 13.5 s, then 6.5 s shaping
  the new one alone - but "13.5+6.5" reads as "13.5 seconds of the outgoing track plus
  6.5 of the incoming", so checking the new track after a 20 s move and finding it 20 s
  in looked like the setting being ignored. Measured against a live pair of decks it was
  not: the overlap begins exactly `span × out_end` before the old track ends, to within a
  poll interval, and the new deck is at that same figure when the old one finishes. The
  numbers were right and the label was wrong, so the row now reads `20 s ▸ join 13.5`
  and the note says which track each number belongs to.
- **A gated fader traded the decks on its first chop.** "Where the leaving track falls
  silent" was read as the first keyframe at zero, which is right for every score whose
  fader goes down once and stays there - and wrong for any that comes back up, which is
  exactly what a gate is. It is the first moment it falls silent *and stays* silent
  now: the last audible keyframe, and the silence begins at the one after it. Nothing
  that shipped before this behaved differently; the new gate would have swapped three
  quarters of a bar in and run the rest of itself on a deck nobody could hear.
- **A loop roll could outlive the transition that started it.** A score ends its own
  rolls with `loop off` and every built-in one does, but a score only gets to finish if
  the transition does - and one abandoned half way would have left an `ab-loop` set on
  a deck about to be parked and re-used, so the next track cued onto it would play half
  a bar of itself forever. Cleared on teardown now, whether the score got there or not,
  which is the same thing that was already true of effects a score switches on.
- **Streamed tracks were never analysed at all.** mpv resolves page URLs itself, at
  load time, through its own yt-dlp hook - so the player's playlist is page URLs, and
  every probe handed one to ffmpeg, which has no hook and cannot open a SoundCloud
  page however hopefully it is asked. Every streamed mix therefore said "no steady
  beat found" while every local file measured perfectly, which is exactly the split a
  hook explains. The probe now does what mpv does: resolves the page with yt-dlp
  once, off-thread, caches the stream URL per track, and decodes that. Verified
  against the reporter's own SoundCloud set: analysed and ready three seconds into
  playback, no timer fallback.
- **A DRM-refused track is now marked unplayable the moment the resolver says so**,
  rather than being cued into. The refusal is a fact about the entry - mpv's own
  loader hits the same wall - so the queue dims it and the automix skips it instead
  of starting a transition into a track that will never load.

- **"No steady beat found" is now reserved for music that actually has none.** Four
  separate paths led there and none of them deserved to:
  - A surveyed tempo was thrown away when the *phase* window failed. Tempo and phase
    are different sizes of answer: a breakdown under one twelve-second window used to
    cost the whole sync, and now costs only the bar lock - the mix still runs
    tempo-matched, which no timer does.
  - Played music drifts. Four honest windows on a live record come back 91, 94, 96,
    and the machine-width agreement rule read that as disagreement. When nothing
    agrees within 1.5%, the question is now asked again at the width a human plays
    to, and the mean of that cluster is the answer.
  - A grid re-read that landed on a breakdown replaced a good grid with nothing, and
    the loop holding the two decks together lost its reference mid-mix. A re-read now
    only ever trades something for something.
  - The phase read had no onset gate, so a breakdown's pad could donate a fabricated
    bar line - which the release and the correction loop would then have held the
    whole mix to, faithfully and wrongly. Every reader now goes through the same
    gate, and a tempo without a measured bar line degrades to a tempo match instead
    of inventing a grid anchored at zero.

- **The Beat mixing switch did nothing, and took the Transition row with it.** The
  settings menu was a list to draw and a `match` on row numbers to act on - two
  orderings of one thing, and therefore two orderings that can disagree. Inserting a
  row in the middle desynchronised everything below it: each row kept its own label
  and picked up its neighbour's action, so the new switch cycled the transition and
  the transition row did nothing at all. Rows are named now rather than numbered, the
  list is built from that one order, and a test asserts the drawn order is the order
  acted on - so a row can be moved without anything having to be renumbered.
- **Toggling beat mixing while a video is playing now actually switches.** It needs
  the whole audio path and no video, which mpv can only do by reloading, so the track
  stops for as long as that takes and resumes where it was. A visible half-second is
  better than a setting that silently does nothing.

- **"No steady beat found" on tracks that plainly have one.** The cause was looking in
  one place: a twelve second window from the top of the file, which on most records is
  the intro. It did not fail there, which would have been fine - it returned a
  confident answer about the pad. A 92 BPM track measured 149.88 at a confidence of
  0.98 from its first twelve seconds and 92.40 from twenty seconds further in. Now
  four windows vote, a window with no onsets in it does not get a vote at all, and a
  track that genuinely has no beat is refused rather than guessed at.

- **Piping the player somewhere now says so.** A full-screen interface needs a
  screen; without one the first thing to fail is raw mode, which reports
  `os error 6` - true, and no help at all to someone wondering why nothing
  appeared. `--help` and `--version` still work anywhere.
- **A launch argument now says what it is doing before the UI exists.** Resolution
  is what decides what the interface will show, so it happens first - and until it
  lands the terminal has nothing on it at all. For a local file that is
  milliseconds; for a sixty-track SoundCloud set behind a slow yt-dlp it is the
  difference between "starting" and "broken". One line, naming the case:
  `Searching YouTube for …`, `Opening …`, `Resolving the playlist…`.
- **Crossfade was never a crossfade.** It ramped one track down and the next one
  up around a cut, with no overlap at all - and, because mpv publishes no
  `time-pos` while it opens the next entry, the ramp back up was skipped
  outright: every transition was a hard cut with a pointless fade-out in front of
  it. It is now a real overlap (see Added), verified end to end on a streamed
  183-track queue: both decks audible for the whole 7 s, outgoing 100 → 1,
  incoming 9 → 100, zero silence at the seam where there used to be 1.25 s.
- **A failed video switch left half a video bar on screen.** Resolving a picture
  is slow enough to be worth a "loading" frame, and that frame goes out through
  the video-mode painter - rows ratatui has no record of. The text UI now starts
  from a clean screen when the switch falls back, and a track that has no picture
  is remembered so the panel stops offering it one.


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
- **`crossfade_secs` is the overlap, not a budget split across a cut.** The two
  tracks are audible together for the whole of it. Playlist prefetch on the
  playing deck is off while crossfade is on - the second deck does that job, and
  two prefetches of the same entry is a wasted yt-dlp run per track.

- **The playlist view and the scope pane became the same thing**, and `v` became
  the way to choose between them. `c` is gone: cycling scopes was a top-level
  key for one of three panel contents, and the style now lives in the `v`
  chooser next to the pane it restyles. `v` no longer toggles video directly.
- **The "NEXT ▸ …" line is no longer a click target.** It sits one row under the
  seek bar, where a mis-aimed seek would swap the centre panel out from under
  the pointer.
- **Hit-testing reads as z-order**: the click map resolves the *last* zone drawn
  at a point rather than the first, so a panel can register its own body and
  still let the rows and popups drawn on top of it win.


- `src/viz.rs` is now `src/audio_tap.rs`: capture (`audio_tap`) and rendering
  (`visualizer`) no longer sit behind two filenames one letter apart. `curl`,
  which reads Spotify links, is declared as an optional dependency in the Arch,
  Debian and RPM packaging.
- Play/pause moved from `p` to `Space` (a click on the status line still works);
  `p` now shows the queue in the centre panel.
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
- Visualizers no longer render through mpv's `tct` video path at all - the scope
  is pure UI state now, so switching styles is instant, works while a video is
  loading, never tears the terminal down, and resize just works. Real ASCII
  video keeps the `tct` path.
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
