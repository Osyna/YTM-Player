# Improvement tiers, round two

> **Status:** Tier A is done, including the `Player` split - it did not happen here,
> it happened in `IMPROVEMENTS-3.md`'s Tier 2, as `src/mixer.rs` and `src/measure.rs`.
> Tier B has smarter queueing and the metadata work; cover art is wired in for local
> files and, since that same round, for streams too. Tier C1 is done and is now the
> automix: five transition styles at the time, fourteen now, applied automatically,
> with beat alignment where the material allows.

Written after Tier 1, most of Tier 3, the Tier 5 foundation and half of Tier 2
shipped. The first roadmap is `IMPROVEMENTS.md`; this one supersedes it, because
what is worth doing next changed when those landed.

Measured state: 15,428 lines. `main.rs` is 3,296 of them and `Player` is **65
fields and 100 methods**. 119 unit tests plus 7 that drive a real process. Two mpv
decks, MPRIS, a library index, a play history, a resume point.

Three tiers, sharply different in ambition. Every item names the code it touches
and, where it matters, says what I saw go wrong while building the thing it is
about.

---

## Tier A — Pay for what the last four sessions bought

Nothing here is new capability. It is the cost of the features already shipped,
and it is due. Days to a week.

- **Split `Player`.** 65 fields and 100 methods. It has absorbed a crossfade
  state machine, a library pane, a filter, a history, a resume point and an MPRIS
  pump, and every one of those went in by adding fields to the same struct
  because that was the cheap move at the time. The seams have not changed:
  `session` (entries, resolution, playlist map, downloads), `engine` (the deck
  pair, tap, recorder, video), `browse` (library, history, filter), `input`. The
  live tests now make this safe in a way it was not before - that was the point
  of writing them first.
- **The MPRIS metadata is a placeholder and I put it there.**
  `artist: self.source.kind.label()` publishes the string "Local" or "YouTube" as
  the artist, and `album` is `""`. Every desktop popup and lock screen shows that.
  Real values exist already for local files - the library index reads them with
  ffprobe - and yt-dlp can print `artist`/`album` for streams. Plumbing them is
  half a day and fixes the most visible lie in the program.
- **Show that metadata in the player too.** The NOW panel has one line of
  `media-title` and nothing else. A music player that knows the artist and album
  and does not say so is odd, and the panel already has the row for it.
- **`cross_refused` is set in three places and surfaced in none.** When a
  transition declines to start - no spare deck, an entry with no URL - the user
  gets a hard cut and no explanation, which is exactly the complaint that started
  the crossfade work.
- **Crossfade and video are mutually exclusive** (`crossfade_tick` bails on
  `video_mode`) and nothing says so. Either say it in the chooser, or make the
  picture survive the swap by moving the `tct` output across with the deck.
- **Library roots are hardcoded** to `~/Music`, `~/music` and `downloads/`. There
  is no way to add one, which makes the pane useless to anyone whose music is on
  a second disk. A settings row and a path prompt.
- **No live test touches MPRIS**, which is 2,482 lines of hand-marshalled wire
  protocol - the code in this repository most likely to break silently against a
  desktop I cannot see. `playerctl` is scriptable; the test is twenty lines.

## Tier B — Make it a music player rather than a URL player

The library, the history and the tags exist but barely talk to each other. This
tier is about the collection, not the stream. Two to four weeks.

- **One catalogue.** Local files, history and search hits are three different
  lists rendered by two different panes today. They should be one ranked result
  set: type once, see what you own, what you played and what YouTube has, and
  pick. `browse_rows` already merges two of the three behind one row type.
- **Saved queues and playlists.** There is no way to keep the evening you just
  built. `store.rs` was written with this in mind - it is a third file next to
  the history and the resume point - and the queue pane is already the view.
- **Cover art in the terminal**, kitty/sixel, as a pane. The library knows the
  files, `ffmpeg` can extract the embedded picture, and the true-colour work for
  `tct` video already proved the terminal can take it.
- **Artwork and metadata for streams too.** yt-dlp prints thumbnail URLs; the
  recorder already fetches. This is what makes the MPRIS fix in Tier A look
  finished rather than partial.
- **Smarter queueing from the library.** Enter plays a track next and jumps to
  it; there is no "add to the end", no "play this album", no "queue this artist".
  The index has album and artist fields that nothing groups by.
- **Gapless album playback.** `gapless-audio` is on, but the crossfade will
  happily overlap two tracks of the same album that were mastered to run
  together. Album-aware transitions are a check against the index.

## Tier C — The leap, and there are two of them

Pick one. They pull in different directions and doing both halves each.

### C1 — The instrument
The two-deck architecture is unusual and currently only ever used to fade. The
tap already streams per-band energy 45 times a second, which is most of what beat
detection needs. Align the swap to a bar instead of a wall clock and a transition
stops being a fade and becomes a mix; then tempo-match with `speed`, put a real
crossfader in the centre panel, and add transition styles to the `effects.rs`
registry that is already a registry. This is the direction where nothing else in
a terminal is competing.

### C2 — The platform
Split playback from the interface so a session survives the terminal closing -
attach and detach like tmux, with the TUI as one frontend, the MPRIS layer as
another and an HTTP API as a third. This is the direction that makes the desktop
integration already built compose rather than compete, and it subsumes the
"saved queues" and "one catalogue" work from Tier B into a server that owns them.

Both are months. C1 is more distinctive; C2 is more useful. C1 is also much
better protected now that the live tests exist, because it edits the deck code
they cover.

---

## Sequencing

- **Tier A's `Player` split before anything in C.** Both leaps add state, and
  both would add it to a 65-field struct.
- **Tier A's MPRIS metadata is the cheapest visible win in the whole document**
  and should probably be done this week regardless of what else is chosen.
- **Tier B's "one catalogue" is a prerequisite for C2**, whose server has to own
  a catalogue rather than a queue.
- **Tier B's album awareness is a prerequisite for C1** sounding right: beat
  matching two tracks that were mastered to run together is worse than not
  transitioning at all.
