# Transitions: what exists, and what the language still cannot say

Twenty-one transitions ship, each named for a real technique. This is a note on what the
score language can express today, what it cannot, and what it would take — written while
building them, so every gap below is one I actually walked into.

## Two modes

**Beat mixing off** — the default. A clean equal-power fade of whatever length is set. No
analysis, nothing decoded ahead, video works. This is not a degraded mode; it is what most
listening wants, and it costs nothing.

**Beat mixing on** — both tracks are measured, their tempos matched, their bars put in time
with each other, and the chosen score runs. Measuring the next track means decoding it
while this one plays, which is a different job from showing a picture, so **this turns video
off**. The two are exclusive by design rather than by accident, and video wins when both are
asked for: someone watching something has said what they want more clearly than a setting
left on from last week.

## What ships

| Transition | The technique | Notes |
|---|---|---|
| Crossfade | the plain equal-power fade | join in the middle of the span |
| Radio fade | linear, short, no ceremony | linear on purpose: constant power holds both up too long on speech |
| Bass swap | EQ swap on the low end | two kick drums never play at once |
| Vocal swap | the same trick on the midrange | voices never collide |
| Filter sweep | lowpass the leaving track away | |
| Highpass out | thin the leaving track to its hats | the more common move of the two on a busy floor |
| Filter in | the arriving track opens from a midrange whisper | both ends squeezed at once |
| Echo out | the leaving track trails off into a real, fading echo | `echo`, see below |
| Cut on beat | no overlap at all | |
| Slam | hold both, throw the new one in | with a top-end lift so it cuts through |
| Brake | stop the leaving track dead, pitch and all | `speed` to 0.05, pitch correction off |
| Drop out | a bar of silence, then the new track lands | |
| Drop swap | 8 bars of hollowing out, then both change place | tempo-matched |
| Long blend | 16 bars ending on a bass drop | the full DJ move |
| Beat roll | the last bar retriggered into the swap | `loop`, halving three times |
| Gate out | the leaving track chopped away on eighths | `every`, one line for a square wave |
| Half time | the leaving track pulled to half speed, then cut | `speed` on a musical ratio, not to a stop |

The last three exist as much to be read as to be played: `loop`, `every` and the `echo`
lane were all added to the language without a built-in using them, and a feature nothing
demonstrates is a feature nobody finds. Each of those three is now the shortest honest
example of one.

Two controls were added to write the first fourteen: **`highpass`** (four of them need it)
and **`speed`** (the brake is nothing else). Both went through the same route as everything
here — a form in `DeckState::filter`, a word in the parser, and every reachable string run
through a real mpv. All 62 of them are accepted; the stacked chain the player composes is
accepted too.

---

## What the language cannot say yet

### ~~1. Loop rolls, and anything else that repeats~~ — done

A beat roll — the last beat of a bar retriggered at halving intervals into the drop — is
one of the most-used moves there is, and it was not expressible at all: not automation of
a control, playback jumping backwards.

`HookAction::Loop { deck, bars }` fires as a hook, not a lane, because it is an event, not
a curve. It sets mpv's own `ab-loop-a` / `ab-loop-b` a bar-fraction behind the playhead of
whichever deck it names; a bare `loop off` clears both decks at once, so a score does not
have to remember which one was rolling to turn it off:

```
hook bar 14   loop out 1/2      # loop the leaving deck over half a bar
hook bar 15   loop out 1/4
hook bar 16   loop off
```

The bar length comes from the same grid every other bar-counted thing in a transition
uses (`Player::bar_seconds`), so a roll and a `bar`-counted lane agree about how long a
bar is without either of them asking the other. `deck_handle` resolves which mpv instance
a hook's deck means at the moment it fires, the same swap-aware rule `apply_shape` uses to
drop a lane once its deck has nothing left to say - a roll started before the decks trade
places and still running after does not silently end up rolling the wrong one.

### ~~2. Effects that are automated rather than switched~~ — partly done

A hook could turn the echo on and off, and that was all. A real echo-out rides the
*feedback* up as the fader comes down, not a fixed filter switched on for the duration.

`echo-out.mix` was a top-end lift and not an echo: an echo you cannot fade is a tail that
never stops, which is worse than none. That specific compromise is gone - `Param::Echo` is
a lane now, `0.0..=1.0` of `aecho` feedback at a fixed 350 ms delay, added exactly the way
`highpass` and `speed` were: a field in `DeckState`, a word in the parser, a real ffmpeg
and a real mpv checked against the literal string it emits. `echo-out.mix` rides it from
`0.0` to `0.55` as the gain rides down, so what plays out is a genuine, decaying series of
repeats bounded by the same fade that used to just switch a filter off.

What did **not** ship is the general case this section originally asked for: `Param`
gaining a generic `Effect(name)` lane over the *whole* `effects.rs` registry, so any of
those entries - not just one new fixed field - could be automated. That is still a
breaking change to the registry and about two days of work; `echo` alone needed neither,
because it is one more fixed control down the same road four others already used. A
resonant filter throw (the other example this section gave) would need the general form,
and still cannot be written.

### 3. Cue points and structure

`Drop swap` counts bars from where the transition begins, not from where the *drop* is.
Landing a double drop means knowing where the drop of each track is, and nothing here does.

**What it needs.** `analysis.rs` already produces onset flux; a section detector — sustained
energy change over eight bars or more — would give "the drop is at 1:47" well enough to
align to. `preview.rs` already decodes the arriving track offline, so the incoming half is
free. Weeks, and it would be wrong often enough to need a confidence gate like the tempo
one.

### ~~4. Patterns~~ — done

A gate or stutter is a square wave on the gain at a sixteenth. Writing that out was
thirty-two hand-written keyframes, and unreadable.

```
lane out gain
    every 1/16 from bar 12 to bar 14   1.0 0.0   hold
```

Parser-only, as expected - `score::parse` expands `every` into exactly the alternating
`Key` sequence hand-writing it would have produced, through the same push that refuses an
out-of-order keyframe, so a pattern cannot desync from a written one written next to it.
`from`/`to` are bars only; a step is a fraction of a bar, and a fraction of the whole
transition is not one. No lane evaluator, mixer, or engine code changed at all.

### ~~8. Tempo that is right rather than confident~~ — done

One look at one place in a track is not a measurement. Tracks open with something that is
not the track — eight bars of pad, a spoken intro, a filtered build — and a window that
lands there does not return "no idea", it returns a confident answer about the wrong thing.
Measured: a 92 BPM track's first twelve seconds gave **149.88 BPM at 0.98 confidence**, and
twenty seconds further in gave 92.40 at 0.996.

So the tempo is now surveyed from four places spread through the track, and the answer is
the one they agree on, folded across octaves so 87 and 174 count as agreement. A single
usable look is not agreement and is refused. A window with no onsets in it — a held chord
autocorrelates perfectly and the tracker will name a tempo for it, 61 BPM from ninety
seconds of one chord — does not get a vote, which is the same rule that outvotes intros.

Across a corpus of house, drum and bass, a slow track with a twelve second intro, a short
edit and a drone: every tempo within 0.2%, and the drone refused.

For a stream the page URL is first resolved to a media URL with yt-dlp - the same step
mpv performs internally at load time, which is why mpv could always play what the probe
could never open. The reading itself is one sequential pass over the whole song, started the moment the
track becomes next - not four seeked windows at the cue. For a stream that is one HTTP
connection instead of four to six, each of which was its own chance to fail; for the
survey it is eight or nine voting windows on a four minute track instead of four fixed
corners; and the decoded frames are kept, so the bar line near the release and every
mid-mix grid refresh is a slice of memory rather than another network read. A three
minute track costs about 2.5 seconds, with the whole length of the current song to spend.

The answer then degrades in steps rather than falling off a cliff. Tempo and phase both
measured is the full sync; tempo without a bar line - every phase window landed on a
breakdown - is a tempo match, not a timer; strict agreement missing but a loose cluster
present (played music drifts a few per cent) is a match to the middle of the drift, with
the correction loop doing the rest; and only genuine absence, a drone or a dead stream, is
refused. A re-read mid-mix that comes back empty changes nothing rather than erasing the
grid the loop is holding to, and no reader anywhere can fabricate a bar line from a pad,
because the onset gate is on the way in for all of them alike.

### ~~5. Phase lock~~ — done

The arriving deck is now seeked onto one of its own bar lines before it is released on one
of the playing track's, so the two count the same bars rather than merely running at the
same speed. Only a small skip, capped at six seconds, because a mix that begins eight
seconds into the next song is not a mix.

Four transitions use it: `sync-blend`, `sync-cut`, `tempo-ride` and `double-drop`, plus
`long-blend` and `drop-swap` from before.

What remains here is that the seek is to the *first* bar line in the sampled window, not to
a chosen phrase — so a track whose intro is eight bars of nothing still starts at its first
beat rather than at the point a person would have dropped it. That needs the structure
detection in §3.

### 6. Reverse

No spinback, no backspin. mpv cannot play backwards, and neither can any filter graph you
can hang off it — it would mean decoding to a buffer and feeding it back, which is a
different program. `Brake` is the honest substitute and sounds like a brake, not a spin.

### 7. Key awareness

Harmonic mixing — only blending tracks in compatible keys — needs chromagram estimation and
a Camelot wheel. `preview.rs` is the natural home. It would change *which* transition is
chosen rather than what a transition can do, so it belongs with the automix, not here.

---

## Suggested order

1. ~~**Loop rolls**~~ — done: biggest musical return for the least work, and hooks already existed.
2. ~~**Patterns**~~ — done: parser-only, and unlocks gates and stutters immediately.
3. ~~**Phase lock**~~ — done (see "Tempo that is right rather than confident" above):
   the measurement was already there; only the seek was missing.
4. ~~**Parameterised effects**~~ — partly done: `echo-out` is honest now; the general
   `Effect(name)` lane behind it is not.
5. **Structure detection** — expensive, uncertain, and everything else is worth more first.

Reverse and key awareness are not worth doing.
