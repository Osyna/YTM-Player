//! Native terminal visualizers, drawn by us from live tap data.
//!
//! Nothing here touches mpv: [`crate::viz::Tap`] measures the playing audio inside
//! mpv's own filter chain and hands renderers a [`VizSnapshot`] ~45 times a second.
//! Each visualizer is an ordinary widget painting into the ratatui buffer, so it
//! composes with the rest of the UI - panes, borders, clicks - instead of owning the
//! screen the way the old mpv `tct` graphs did.
//!
//! Adding one is a struct with a [`Visualizer`] impl plus one line in [`all`]. State
//! (peak caps, smoothing) lives in the struct; the snapshot is pure data.

use crate::viz::{BAND_COUNT, BAND_FLOOR, VizSnapshot};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::symbols::Marker;
use ratatui::widgets::Widget;
use ratatui::widgets::canvas::{Canvas, Line as CLine};

/// One selectable visualizer: the tag shown in the pane title, and a stateful renderer.
pub trait Visualizer {
    fn name(&self) -> &'static str;
    fn render(&mut self, viz: &VizSnapshot, area: Rect, buf: &mut Buffer);
}

/// Every visualizer, in the order `c` cycles through them. Extend here to add one -
/// nothing else in the player needs to change.
pub fn all() -> Vec<Box<dyn Visualizer>> {
    vec![
        Box::new(Spectrum::default()),
        Box::new(Scope),
        Box::new(Vu::default()),
    ]
}

/// Vertical eighth-block ramp, empty to full.
const V8: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Horizontal eighth-block ramp, empty to full.
const H8: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Linear blend between two RGB colours.
fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(c(a.0, b.0), c(a.1, b.1), c(a.2, b.2))
}

/// Height gradient shared by the level-driven visualizers: deep green through cyan
/// into hot white, with everything past `0.9` reading as "red zone".
fn heat(t: f32) -> Color {
    if t < 0.55 {
        lerp((0, 160, 110), (0, 210, 235), t / 0.55)
    } else if t < 0.9 {
        lerp((0, 210, 235), (160, 245, 255), (t - 0.55) / 0.35)
    } else {
        lerp((255, 120, 120), (255, 60, 90), (t - 0.9) / 0.1)
    }
}

// ---------------------------------------------------------------------------
// Spectrum: per-band bars with falling peak caps
// ---------------------------------------------------------------------------

/// dB range the spectrum spreads across, measured down from the running reference.
/// Everything quieter than `reference - AGC_SPAN` reads as an empty bar.
const AGC_SPAN: f32 = 42.0;
/// Headroom: the loudest band of the frame sits here rather than jammed at the ceiling,
/// so a band that then gets louder still has somewhere to go.
const AGC_TOP: f32 = 0.94;
/// How fast the reference falls (dB per frame) when nothing is that loud any more.
/// At the tap's ~45 fps this crosses a 20 dB drop in roughly five seconds.
const AGC_DECAY_DB: f32 = 0.09;
/// The reference never falls below this. Silence measures around -64 dB per band,
/// which is more than [`AGC_SPAN`] below this floor, so quiet passages and room tone
/// stay near the baseline instead of being amplified into a full-height wall.
const AGC_REF_MIN_DB: f32 = -20.0;

/// Undo [`crate::viz::BAND_FLOOR`]'s fixed normalisation back into dB.
fn band_db(level: f32) -> f32 {
    level.clamp(0.0, 1.0).mul_add(BAND_FLOOR, -BAND_FLOOR)
}

/// Level the bars against a slowly-falling loudness reference.
///
/// The tap normalises each band against a fixed 64 dB floor, which is right for
/// programme material mastered near the design target and useless for anything
/// louder: every band lands in the top few percent and the spectrum flattens into a
/// wall. This re-spreads the frame relative to its own recent maximum - fast attack
/// (the reference jumps to any louder band immediately), slow release, floored so a
/// quiet passage is not blown up to full height.
fn agc_bands(reference_db: &mut f32, bands: &[f32; BAND_COUNT]) -> [f32; BAND_COUNT] {
    let frame_max_db = bands.iter().copied().fold(f32::NEG_INFINITY, |a, b| {
        let db = band_db(b);
        if db > a { db } else { a }
    });
    *reference_db = (*reference_db - AGC_DECAY_DB)
        .max(frame_max_db)
        .max(AGC_REF_MIN_DB);

    let mut out = [0.0f32; BAND_COUNT];
    for (slot, &level) in out.iter_mut().zip(bands.iter()) {
        *slot = ((band_db(level) - *reference_db) / AGC_SPAN)
            .mul_add(AGC_TOP, AGC_TOP)
            .clamp(0.0, 1.0);
    }
    out
}

pub struct Spectrum {
    /// Displayed level per band, chased toward the live value for a little inertia.
    smooth: [f32; BAND_COUNT],
    /// Peak cap per band, falling slowly until the bar pushes it back up.
    caps: [f32; BAND_COUNT],
    /// Running loudness reference in dB for [`agc_bands`].
    reference_db: f32,
}

impl Default for Spectrum {
    fn default() -> Self {
        Spectrum {
            smooth: [0.0; BAND_COUNT],
            caps: [0.0; BAND_COUNT],
            reference_db: AGC_REF_MIN_DB,
        }
    }
}

impl Visualizer for Spectrum {
    fn name(&self) -> &'static str {
        "SPECTRUM"
    }

    fn render(&mut self, viz: &VizSnapshot, area: Rect, buf: &mut Buffer) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        let slot = (area.width / BAND_COUNT as u16).max(1);
        let bar_w = if slot > 2 { slot - 1 } else { slot };
        let used = slot * BAND_COUNT as u16;
        let x0 = area.x + area.width.saturating_sub(used) / 2;
        let h = area.height;

        let leveled = agc_bands(&mut self.reference_db, &viz.bands);
        for (i, &target) in leveled.iter().enumerate() {
            let target = if viz.live { target } else { 0.0 };
            // Fast attack, slower release: punchy but not jittery.
            let s = &mut self.smooth[i];
            let rate = if target > *s { 0.55 } else { 0.25 };
            *s += (target - *s) * rate;
            self.caps[i] = (self.caps[i] - 0.015).max(*s);

            let eighths = (*s * f32::from(h) * 8.0).round() as u16;
            let (full, part) = (eighths / 8, (eighths % 8) as usize);
            let x = x0 + i as u16 * slot;
            if x + bar_w > area.x + area.width {
                break;
            }
            for row in 0..h {
                let y = area.y + h - 1 - row;
                let (ch, frac) = if row < full {
                    ('█', f32::from(row) / f32::from(h))
                } else if row == full && part > 0 {
                    (V8[part], f32::from(row) / f32::from(h))
                } else {
                    continue;
                };
                let color = heat(frac);
                for dx in 0..bar_w {
                    buf[(x + dx, y)].set_char(ch).set_fg(color);
                }
            }
            // The cap sits one row above its level and never below the bar top.
            if viz.live && self.caps[i] > 0.02 {
                let cap_row = (self.caps[i] * f32::from(h))
                    .round()
                    .clamp(0.0, f32::from(h)) as u16;
                let y = area.y + h - cap_row.clamp(1, h);
                for dx in 0..bar_w {
                    buf[(x + dx, y)]
                        .set_char('▁')
                        .set_fg(Color::Rgb(255, 90, 160));
                }
            }
        }
        if !viz.live {
            baseline(area, buf);
        }
    }
}

// ---------------------------------------------------------------------------
// Scope: stereo envelope, braille, newest at the right edge
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Scope;

impl Visualizer for Scope {
    fn name(&self) -> &'static str {
        "SCOPE"
    }

    fn render(&mut self, viz: &VizSnapshot, area: Rect, buf: &mut Buffer) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        let n = (area.width as usize) * 2;
        let wave = &viz.wave;
        let len = wave.len();

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, n as f64])
            .y_bounds([-1.0, 1.0])
            .paint(|ctx| {
                // Centre axis, dim, so silence still reads as a scope at rest.
                ctx.draw(&CLine {
                    x1: 0.0,
                    y1: 0.0,
                    x2: n as f64,
                    y2: 0.0,
                    color: Color::Rgb(0, 70, 70),
                });
                if !viz.live || len < 2 {
                    return;
                }
                // Newest sample pinned to the right edge; the trace grows leftward.
                // Signed math: the segment one step past the left edge goes negative
                // and the canvas clips it, rather than usize wrapping.
                let x_at = |k: usize| n as f64 - (len - 1 - k) as f64;
                for k in (1..len).rev() {
                    if len - 1 - k > n + 1 {
                        break;
                    }
                    let (l0, r0) = wave[k - 1];
                    let (l1, r1) = wave[k];
                    // Left channel above the axis, right below, filled to the axis so
                    // the envelope reads as a solid waveform, not a hairline.
                    ctx.draw(&CLine {
                        x1: x_at(k),
                        y1: 0.0,
                        x2: x_at(k),
                        y2: f64::from(l1) * 0.96,
                        color: Color::Rgb(0, 95, 115),
                    });
                    ctx.draw(&CLine {
                        x1: x_at(k),
                        y1: 0.0,
                        x2: x_at(k),
                        y2: f64::from(-r1) * 0.96,
                        color: Color::Rgb(95, 0, 105),
                    });
                    ctx.draw(&CLine {
                        x1: x_at(k - 1),
                        y1: f64::from(l0) * 0.96,
                        x2: x_at(k),
                        y2: f64::from(l1) * 0.96,
                        color: Color::Rgb(0, 230, 255),
                    });
                    ctx.draw(&CLine {
                        x1: x_at(k - 1),
                        y1: f64::from(-r0) * 0.96,
                        x2: x_at(k),
                        y2: f64::from(-r1) * 0.96,
                        color: Color::Rgb(255, 80, 220),
                    });
                }
            });
        canvas.render(area, buf);
    }
}

// ---------------------------------------------------------------------------
// VU: two channel meters with peak-hold needles and a dB scale
// ---------------------------------------------------------------------------

pub struct Vu {
    hold: [f32; 2],
}

impl Default for Vu {
    fn default() -> Self {
        Vu { hold: [0.0; 2] }
    }
}

impl Visualizer for Vu {
    fn name(&self) -> &'static str {
        "VU"
    }

    fn render(&mut self, viz: &VizSnapshot, area: Rect, buf: &mut Buffer) {
        if area.width < 12 || area.height < 3 {
            return;
        }
        // Meters vertically centred: L at y0, R two rows below, then a scale row
        // when there's room. The R meter needs y0+2, hence the height-3 floor.
        let rows_needed = if area.height >= 5 { 4 } else { 3 };
        let y0 = area.y + (area.height - rows_needed) / 2;
        let label_w = 2u16;
        let value_w = 7u16;
        let meter_w = area.width - label_w - value_w;

        for ch in 0..2 {
            let y = y0 + ch as u16 * 2;
            let rms = if viz.live { viz.rms[ch] } else { 0.0 };
            let peak = if viz.live { viz.peak[ch] } else { 0.0 };
            self.hold[ch] = (self.hold[ch] - 0.006).max(peak);

            buf[(area.x, y)]
                .set_char(if ch == 0 { 'L' } else { 'R' })
                .set_fg(Color::Rgb(120, 130, 140));

            let x = area.x + label_w;
            let eighths = (rms * f32::from(meter_w) * 8.0).round() as u16;
            let (full, part) = (eighths / 8, (eighths % 8) as usize);
            for dx in 0..meter_w {
                let frac = f32::from(dx) / f32::from(meter_w);
                let cell = &mut buf[(x + dx, y)];
                if dx < full {
                    cell.set_char('█').set_fg(heat(frac));
                } else if dx == full && part > 0 {
                    cell.set_char(H8[part]).set_fg(heat(frac));
                } else {
                    cell.set_char('╌').set_fg(Color::Rgb(40, 46, 52));
                }
            }
            // Instant peak in white, held peak in magenta - classic twin needles.
            let needle = |v: f32| ((v * f32::from(meter_w)) as u16).min(meter_w - 1);
            if viz.live && peak > 0.01 {
                buf[(x + needle(peak), y)]
                    .set_char('▌')
                    .set_fg(Color::Rgb(235, 245, 255));
            }
            if viz.live && self.hold[ch] > 0.01 {
                buf[(x + needle(self.hold[ch]), y)]
                    .set_char('▏')
                    .set_fg(Color::Rgb(255, 90, 200));
            }
            // dB read-out, right-aligned: -60.0 floor maps back from the norm.
            let db = if rms > 0.0 { (rms - 1.0) * 60.0 } else { -60.0 };
            let text = format!("{db:>6.1}");
            for (k, c) in text.chars().enumerate() {
                buf[(x + meter_w + 1 + k as u16, y)]
                    .set_char(c)
                    .set_fg(Color::Rgb(120, 130, 140));
            }
        }

        if rows_needed == 4 {
            // Scale ticks under the meters at the usual broadcast marks.
            let y = y0 + 3;
            let x = area.x + label_w;
            for (db, label) in [(-40.0, "-40"), (-20.0, "-20"), (-10.0, "-10"), (-3.0, "-3")] {
                let frac = ((db + 60.0) / 60.0) as f32;
                let tx = x + ((frac * f32::from(meter_w)) as u16).min(meter_w - 1);
                buf[(tx, y)].set_char('┴').set_fg(Color::Rgb(70, 78, 86));
                for (k, c) in label.chars().enumerate() {
                    let cx = tx + 1 + k as u16;
                    if cx < x + meter_w {
                        buf[(cx, y)].set_char(c).set_fg(Color::Rgb(70, 78, 86));
                    }
                }
            }
        }
    }
}

/// Dim dotted floor for a dead tap, so an idle pane still looks deliberate.
fn baseline(area: Rect, buf: &mut Buffer) {
    let y = area.y + area.height - 1;
    for dx in 0..area.width {
        if dx % 2 == 0 {
            buf[(area.x + dx, y)]
                .set_char('·')
                .set_fg(Color::Rgb(50, 58, 66));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(level: f32) -> VizSnapshot {
        VizSnapshot {
            bands: [level; BAND_COUNT],
            rms: [level; 2],
            peak: [level; 2],
            wave: vec![(level, level); 64],
            live: true,
        }
    }

    /// Level in the tap's normalisation for a given per-band dB.
    fn at_db(db: f32) -> f32 {
        (db + BAND_FLOOR) / BAND_FLOOR
    }

    #[test]
    fn agc_spreads_a_hot_frame_instead_of_flattening_it() {
        // Loud modern master: every band within a few dB of the top, which the fixed
        // 64 dB floor squeezes into an indistinguishable wall.
        let mut bands = [at_db(-6.0); BAND_COUNT];
        bands[0] = at_db(-3.0);
        bands[8] = at_db(-18.0);
        bands[15] = at_db(-40.0);
        assert!(
            bands[0] - bands[1] < 0.05,
            "premise: raw levels are nearly identical"
        );

        let mut reference = AGC_REF_MIN_DB;
        let out = agc_bands(&mut reference, &bands);
        assert!(
            (reference - (-3.0)).abs() < 0.001,
            "reference tracks the peak"
        );
        assert!(out[0] > out[1], "loudest band must still lead");
        assert!(
            out[1] - out[8] > 0.2,
            "12 dB apart should be visibly apart: {} vs {}",
            out[1],
            out[8]
        );
        assert!(out[15] < 0.15, "a dead band must read as near-empty");
        assert!(out[0] <= 1.0 && out[0] >= AGC_TOP - 0.01, "headroom kept");
    }

    #[test]
    fn agc_does_not_amplify_silence() {
        let mut reference = AGC_REF_MIN_DB;
        let quiet = [at_db(-70.0); BAND_COUNT];
        for _ in 0..200 {
            let out = agc_bands(&mut reference, &quiet);
            assert!(
                out.iter().all(|&v| v < 0.1),
                "silence must stay flat, got {out:?}"
            );
        }
        assert!(reference >= AGC_REF_MIN_DB);
    }

    #[test]
    fn agc_attacks_instantly_and_releases_slowly() {
        let mut reference = AGC_REF_MIN_DB;
        agc_bands(&mut reference, &[at_db(-2.0); BAND_COUNT]);
        assert!((reference - (-2.0)).abs() < 0.001, "instant attack");

        // One quiet frame must not immediately rescale the whole display.
        let quiet = [at_db(-30.0); BAND_COUNT];
        agc_bands(&mut reference, &quiet);
        assert!(reference > -3.0, "release is gradual, not a jump");

        // But a sustained quiet passage does settle - down to the floor at most.
        for _ in 0..1000 {
            agc_bands(&mut reference, &quiet);
        }
        assert!(
            (reference - AGC_REF_MIN_DB).abs() < 0.5,
            "reference should settle at the floor, got {reference}"
        );
    }

    #[test]
    fn registry_names_are_unique_and_nonempty() {
        let vs = all();
        assert!(vs.len() >= 3);
        let names: Vec<&str> = vs.iter().map(|v| v.name()).collect();
        let mut dedup = names.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), names.len(), "duplicate visualizer names");
        assert!(names.iter().all(|n| !n.is_empty()));
    }

    #[test]
    fn spectrum_paints_loud_and_clears_silent() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 48, 12));
        let area = *buf.area();
        let mut s = Spectrum::default();
        // A couple of frames lets the smoothing catch up.
        for _ in 0..8 {
            s.render(&snap(0.9), area, &mut buf);
        }
        let painted = buf.content().iter().filter(|c| c.symbol() != " ").count();
        assert!(painted > 100, "loud spectrum barely painted: {painted}");

        let mut quiet = Buffer::empty(Rect::new(0, 0, 48, 12));
        let mut s2 = Spectrum::default();
        s2.render(&VizSnapshot::default(), area, &mut quiet);
        let painted = quiet.content().iter().filter(|c| c.symbol() != " ").count();
        assert!(
            painted <= 48,
            "silent spectrum should be near-empty: {painted}"
        );
    }

    #[test]
    fn scope_and_vu_render_without_panicking_on_tiny_areas() {
        for (w, h) in [(1u16, 1u16), (2, 2), (13, 2), (80, 1), (3, 40)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            let area = *buf.area();
            Scope.render(&snap(0.7), area, &mut buf);
            Vu::default().render(&snap(0.7), area, &mut buf);
            Spectrum::default().render(&snap(0.7), area, &mut buf);
        }
    }

    #[test]
    fn vu_holds_peaks_between_frames() {
        let mut vu = Vu::default();
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 6));
        let area = *buf.area();
        vu.render(&snap(0.95), area, &mut buf);
        let held = vu.hold[0];
        vu.render(&snap(0.1), area, &mut buf);
        assert!(
            vu.hold[0] > 0.8,
            "hold collapsed instantly: {held} -> {}",
            vu.hold[0]
        );
    }
}
