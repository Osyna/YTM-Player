# Writing a transition

A transition is a file. Copy one, change it, restart the player — no rebuild, no Rust.

```
~/.config/ytmplayer/transitions/my-mix.mix
```

Anything in that folder is loaded at startup. A file whose name matches a built-in
(`long-blend.mix`, `crossfade.mix`, …) **replaces** it, so you can edit the shipped ones by
copying them out of the player's `transitions/` folder and changing your copy. Anything
else is a new transition and appears in the settings menu next to the rest.

The file name is the identity: `Long-Blend.mix` is the transition `long_blend`. The `name`
line inside is only what the menu displays.

## The shape of a file

```
name    Slam
note    holds, then throws the new one in
bars    8            # optional: this move is counted, not timed
align   beat         # beat | free  - should the swap land on a bar line
tempo   match        # match | free - pull the arriving track to this one's speed first
power   free         # constant | free - is this a level blend

lane out gain
    at 0.0   1.0   hold
    at 0.8   1.0   smooth
    at 1.0   0.0   hold

lane in gain
    at 0.0   0.0   hold
    at 0.8   1.0   hold
    at 1.0   1.0   hold

hook at 0.8   toast SLAM
hook at 0.0   effect Echo on
hook at 1.0   effect Echo off
```

Comments start with `#`, anywhere. Blank lines are nothing.

## Lanes

`lane <deck> <control>`, then one line per keyframe: `<when> <value> <ease>`.

| deck | |
|---|---|
| `out` | the track that is leaving |
| `in` | the track that is arriving |

| control | units | when nothing touches it |
|---|---|---|
| `gain` | 0.0–1.0, multiplies your own volume | 1.0 |
| `bass` | dB on a low shelf at 200 Hz | 0 |
| `mid` | dB on a bell at 1.2 kHz | 0 |
| `high` | dB on a high shelf at 4 kHz | 0 |
| `lowpass` | corner in Hz; `0` means none | 0 |
| `highpass` | corner in Hz; `0` means none | 0 |
| `speed` | absolute playback rate, `1.0` the file's own, pitch falls with it | 1.0 |
| `tempo` | `0` locked to the other deck, `1` its own speed | 1 |
| `echo` | 0.0–1.0, an `aecho` feedback at a fixed 350 ms delay; `0` means no filter at all | 0 |

| when | |
|---|---|
| `bar 12` | bar twelve of the transition |
| `at 0.75` | three quarters of the way through |

| ease | how the value travels to the next keyframe |
|---|---|
| `hold` | stay here, then jump. **A drop is a hold and a jump** |
| `linear` | straight line |
| `smooth` | slow at both ends; what a hand on a knob does |
| `power-down` | `cos` of the eased angle — a fader leaving |
| `power-up` | `sin` of the eased angle — a fader arriving |

`power-down` and `power-up` are a pair. Two overlapping tracks are uncorrelated, so their
powers add, and this pair is what holds `out² + in²` at exactly one. Use them together for
anything that is a *blend*; use `smooth` when you want the mix to empty out on purpose.

`tempo` is relative because a file is written long before anyone knows which two tracks it
will join. `0` means "whatever the other deck is doing", `1` means "your own speed", and
the player converts that with the ratio it measured.

### Repeating keyframes

`every <fraction> from <when> to <when>   <v0> <v1>   <ease>` writes a whole gate or
stutter in one line instead of one keyframe per pulse:

```
lane out gain
    bar 0    1.0   hold
    every 1/16 from bar 12 to bar 14   1.0 0.0   hold
    bar 16   0.0   hold
```

That expands to the same thirty-two alternating keyframes as writing them out by hand -
one every sixteenth of a bar from bar 12 up to (not including) bar 14, toggling between
`1.0` and `0.0` with the ease given. `from`/`to` are always bars: a fraction of a bar is
meaningless against a fraction of the whole transition, so `every ... from at ... to at
...` is refused. Everything else about the result is an ordinary run of keyframes -
`every` is a shorthand for typing them, not a new kind of lane.

## Sync

A `tempo` lane is the whole of it:

```
lane in tempo
    bar 0     0.0    hold      # locked to the playing track
    bar 8     0.0    smooth    # ...for as long as both are audible
    bar 16    1.0    hold      # then walked back to its own speed
```

Writing that lane is what asks for a sync, and three things follow from it:

- **The arriving track's tempo is measured before it is audible.** The tap only ever hears
  the deck that is playing, so a slice of the next one is decoded separately (`preview.rs`),
  about ninety milliseconds for a local file, on its own thread, started at the cue.
- **It is pulled to the playing track's speed**, but only if the stretch is under six per
  cent. Beyond that it stops sounding like a mix and starts sounding like a fault, so the
  player declines and the lane simply does nothing.
- **It is started at the point that matches**, rather than at a moment that is waited for.
  Where the playing track is in its bar at the instant of release decides where in its own
  bar the arriving one begins — which is what dropping a record on the right spot has
  always been, as opposed to waiting for the right moment to press play. Only ever a small
  skip: a lead-in, not a verse.
- **It is held there.** Both grids are re-read every few seconds, the phase error between
  them is a subtraction, and what goes back is a lean of well under one per cent on the
  arriving deck's speed — the same correction a hand on a platter makes, and for the same
  reason. Measured live, the two sit about **five milliseconds** apart; a flam starts to be
  audible around fifteen.

The player shows its working: `◈ ANALYSING Track Two · reading its beat with ffmpeg`
while the measurement runs, then the tempo it found and what was done with it.

Getting there took four separate faults out, all of the same kind — two quantities that
should have been one. The tempo of one track measured through the live tap and the other
through ffmpeg, so their quotient was 0.971 for two identical files. A grid read at the cue
and used most of a minute later, extrapolated over twenty-odd bars. A correction loop
comparing a stale snapshot position against a live one, which held the decks apart by
exactly the snapshot's age. And a release timed on one grid having seeked on another. Every
one of them looked correct in isolation.

There is no separate switch. A score that automates tempo wants a sync by definition, and
one that does not, does not; saying `tempo match` in the header as well is allowed as
documentation but **disagreeing with the lane is refused at read time**. That check exists
because a shipped file once claimed a match, had no lane to release it, and therefore paid
for a decode and did nothing — silently, for as long as nobody looked.

## Hooks

`hook <when> <action>` — for the parts of a move that are not a fader.

```
hook bar 12   effect Echo on
hook at 1.0   effect Echo off
hook bar 0    toast here it comes
hook bar 14   loop out 1/2
hook bar 15   loop out 1/4
hook bar 16   loop off
```

| action | |
|---|---|
| `effect <name> on\|off` | switch an entry of the effects rack (`e` in the player) |
| `toast <text>` | say something in the status bar |
| `loop <deck> <fraction>` | A-B loop `deck` (`out` or `in`) over the last `<fraction>` of a bar - `1/2`, `1/4`, or a bare decimal |
| `loop off` | cancel a loop, whichever deck it is on |

Each fires once, on the way past. One whose moment has already gone when the transition
starts is skipped, not fired late.

A roll is a few of these in a row - each fraction smaller than the last, so the loop
tightens as it runs, ending in a `loop off` so the deck plays on rather than looping
forever. Which deck is looping does not have to be repeated to turn it off.

## Bars, and what happens without a tempo

Write `bars 16` and the moves are counted against the beat grid: sixteen bars is thirty
seconds at 128 BPM and twenty-two at 174, and the player stretches the whole transition to
fit. It finds the tempo in the run-up, and lifts its own redraw rate while it does — the
redraw rate *is* the beat tracker's sample rate.

When the tempo cannot be read — heavily limited masters often cannot — `bar 8` of a
`bars 16` score is simply read as halfway. The moves happen in the same order at the wrong
tempo. That is a compromise on purpose: a transition that has to be abandoned whenever the
tempo is unreadable is one nobody can rely on.

## The fade curve, which is not yours to set

`Fade curve` in settings — `Linear`, `Smooth`, `Bezier`, `Late`, `Early` — restyles every
transition at once, this one included, and no `.mix` file can see it or opt out of it. It
is deliberately not part of this language: a score says *what* happens and in what order,
and the curve says *when along the way*, and keeping those apart is what lets one setting
apply to twenty-one scores without any of them knowing it exists.

It works by warping the clock, not the values, which is why it is safe to apply to
everything you can write here:

- `power-down`/`power-up` is `cos`/`sin` of one angle, so warping the angle moves both
  together and `out² + in²` stays pinned at one whatever the curve does.
- `hold` still holds, because holding does not read the clock. No curve turns a cut into
  a fade.
- A drop still drops on the key it was written on. Only *when the clock gets there*
  moves.

Lanes are read through the curve; **hooks are not**. A lane is a fader move and moving it
is the entire point; a hook is an event pinned to a musical moment, and a beat roll that
starts a third of a bar late is not a beat roll. So write anything that must land on the
beat as a hook, and it will, under every curve.

`Linear` is the default and is a true no-op: every score plays exactly as written.

## What is refused, and why

A file with a mistake is refused when it is read, with its name and line number, and the
player says so in the status bar rather than only on a stderr line that the interface
covers a second later. One bad file of yours costs you that file, not the player.

- **Unknown words are errors, never defaults.** An `ease` is the difference between a fade
  and a drop; guessing it would make a typo sound like a decision.
- **Keys must be in time order.** Out of order, the evaluator would read the wrong pair and
  do it silently.
- **Both decks need a `gain` lane.** Without one a deck sits at its neutral 1.0, which for
  the arriving track means starting at full volume.
- **Endpoints are exact.** `out` starts at 1 and ends at 0; `in` does the reverse. Anything
  else strands a deck for the rest of the night.
- **One lane per control per deck.** Two would mean one is silently ignored.
- **`bars` with only `at` keyframes** is a contradiction, so it is an error rather than a
  guess about which you meant.
- **A blend that claims `power constant`** may not stray more than 1.5 dB from unit power
  anywhere. Say `power free` for a score that empties out — the long blend does.

## Two things worth knowing

**Filter values are quantised on the way out.** dB to 4 dB steps, lowpass corners to seven
octave-spaced values. Every distinct filter string rebuilds mpv's filter graph
mid-playback, so a continuously computed sweep would pay that on every frame instead of
seven times across a transition. Write the curve you want; the player coarsens it.

**mpv accepts a filter string it cannot parse and reports success** — no error, no log, no
effect. That is why `lane` takes named controls rather than raw filter text: everything the
player can emit is built from four forms that have been run through a real mpv, where a bad
filter does exit non-zero. It is a deliberate limit, and the reason a typo in your file is
caught at the line rather than heard as silence at a party.

## Adding a control

`Param` in `src/transitions.rs`, one arm in `DeckState::filter`, one word in
`score::parse_param`. That last file is the single place mpv's filter syntax is written,
and therefore the single place it has to be checked against a real mpv.
