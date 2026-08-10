# Improvements, third round: DRY, optimised, lightweight

> **Status:** Tiers 1-4 shipped in full. Tier 5 is three of its four items, and the
> fourth is why it is three and not four rather than an oversight: loop rolls and
> pattern lanes are in at the "hours" and "half a day" the language work below turned
> out to actually cost, and `echo-out` is a real echo now - but by adding one more
> fixed control the way `highpass` and `speed` already were, not the general
> `Effect(name)` lane `TRANSITIONS-STATUS.md` scoped at two days and a breaking
> change. Structure detection did not ship at all: that document's own lowest-ranked
> item, weeks of uncertain DSP work everything else is worth more than. `main.rs`
> split into `mixer.rs` and `measure.rs` as planned (5,604 lines → 3,332, with two new
> ~1,100 and ~1,400-line modules, every function moved once and none lost - verified
> by counting them before and after); the release binary measured 1.79 MB → 1.66 MB
> with `panic = "abort"`, within the estimate. The live suite split found a real bug
> on the way - MPRIS's well-known D-Bus name is shared by every player process the
> suite spawns, so that one test stayed serial despite matching the doc's own
> "functional" category, which the "profile decides, not the itch" rule below was
> written to allow.

Grounded in measurement, not taste. Current state: 22,237 lines across 17 modules, the
largest being `main.rs` at 5,604; release binary 1.79 MB with `lto = true`,
`codegen-units = 1`, `strip = true` already set; live suite 430 s, dominated by real-time
playback waits, not compute. The costs below are named so a tier can be stopped mid-way
without losing its value.

## Tier 1 — hours: delete the duplication that is actively breeding

**One per-entry cache instead of five.** `probed`, `surveyed`, `analysed`,
`resolved_media` and `ahead_tried` are five parallel `Vec<(usize, T)>` keyed by the same
entry index, each with its own copy of the same retain/push/cap dance, written at five
different times. This is the exact "one fact, two orderings" shape that produced the
settings-row bug, the fade-range bug and the 0.971 tempo ratio - it is just wearing five
hats. Replace with one `Vec<(usize, TrackFacts)>` where `TrackFacts { tempo, measured,
frames, media_url, ahead_tried }`; one eviction policy, one invalidation point on playlist
mutation (which today must be remembered five times).

**One yt-dlp run per track instead of three.** Ten spawn sites in `youtube.rs`. Per
streamed track the player may run yt-dlp for the title (batch resolver), again for the
media URL (`-g`), and mpv runs its own at load. A single `-J` dump returns title, format
list *and* thumbnail URL in one process and one network round trip; the title resolver,
media resolution and the currently-dead stream-artwork path all become readers of one
cached blob. Fewer processes, fewer round trips, and the artwork box starts working for
streams as a side effect.

## Tier 2 — a day: the structural debt

**Split `Player`.** ~70 fields, 5,604 lines. The mixing engine (cross, probes, sync loop,
release lag) is a coherent unit with a narrow interface to the rest - extract
`src/mixer.rs`; the measurement pipeline (survey, consensus, measure_track, caches) is
another - extract `src/measure.rs`. Not for aesthetics: every session this week collided
with unrelated state while editing main.rs, and the compiler re-checks 5,600 lines for
every one-line change.

**Persist the analysis.** Tempo, downbeat and release-lag are re-learned every session.
`store.rs` already persists history; a `(url-hash -> bpm, downbeat, analysed_at)` table
makes every previously-played track "ready to mix" at t=0 forever, and seeds the lag
estimator from last session instead of from zero. Tiny code, permanent payoff.

## Tier 3 — a day: measured lightweight wins

- **`panic = "abort"` in the release profile**: drops unwind tables from a binary that
  treats every panic as fatal anyway. Measure the delta; expect ~5-10%.
- **Quantise cached whole-song frames to u8.** `Vec<[f32; 16]>` is 68 B/frame; levels are
  0..=1 with 8-bit perceptual resolution. 7.7 MB per cached track becomes ~2 MB, ×3
  cached. Convert at the `feed_window` boundary; the tracker never notices.
- **Gate per-frame string building on `dirty`.** Status spans and settings rows allocate
  every redraw tick; build them only when state changed. Profile first - at a 200 ms text
  cadence this may be noise, and the profile decides, not the itch.

## Tier 4 — two or three days: iteration speed and robustness

- **Split the live suite into timing-sensitive and functional groups.** The functional
  half (settings, queue, MPRIS, covers) tolerates parallelism and cuts the 430 s wall
  time roughly in half; the beat-alignment tests keep their serial, load-shy world.
- **Fixture cache keyed by parameters** (`/tmp/ytmfix_<hash>.mp3`): every test re-encodes
  its tracks with lame today; encoding once per parameter set saves a re-run tax on every
  iteration.
- **Event-driven waits in tests**: several tests pump fixed durations where they could
  wait on a predicate; the suite's floor is real playback time, but the padding on top is
  removable.

## Tier 5 — a week each: the features the framework is still missing

In value order, from `TRANSITIONS-STATUS.md`, all unchanged in shape:
1. **Loop rolls** (`HookAction::Loop` over mpv's ab-loop) - the highest-value musical gap,
   ~40 lines.
2. **Pattern lanes** (`every 1/16 from bar 12 to 14`) - parser-only; makes gates and
   stutters writable.
3. **Parameterised effects** - lets `echo-out` be an actual echo instead of an honest
   compromise.
4. **Structure detection** - the drop-aligned double drop; expensive, uncertain, gate it
   behind confidence like everything else.

## What not to do

No new crates for any of this - the 6-crate budget holds. No mpv replacement: every fault
found across seven sessions was in analysis or in reconciling two sources of truth, never
in transport. And no optimisation ahead of its measurement: the binary is 1.8 MB, idle
CPU is bounded by a 50 ms poll, and the only measured hot spot is process spawning -
which Tier 1 already halves.
