//! Sampling of a [`Gradient`] leaf's [`Paint`] onto the terminal cell grid.
//!
//! `Gradient` carries its paint in unit space (mesh gradients stay on the
//! engine path and reach this backend as a `SceneView`). A terminal has no
//! pixel fill primitive, so each cell is drawn as `▀`: its foreground is the
//! color sampled at the top half of the cell and its background the color
//! sampled at the bottom half, yielding two gradient rows per terminal row.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as CellRect;
use ratatui::style::{Color, Style};
use waterui_graphics::color::working::lerp;
use waterui_graphics::draw::{ColorStop, Extend, Paint};
use waterui_graphics::{Gradient, WorkingColor};

use crate::style::{cell_under, composite_over};

/// Samples `gradient` at normalized point `(u, v)` inside its frame.
///
/// The paint's coordinates are the view's normalized `[0, 1]` unit space;
/// the returned color is in the engine's working space.
#[must_use]
pub fn sample(gradient: &Gradient, u: f32, v: f32) -> WorkingColor {
    let (t, stops, extend) = match gradient.paint() {
        Paint::Linear(linear) => {
            let (dx, dy) = (linear.end.x - linear.start.x, linear.end.y - linear.start.y);
            let t = {
                let len2 = dx.mul_add(dx, dy * dy);
                if len2 <= f64::EPSILON {
                    0.0
                } else {
                    ((f64::from(u) - linear.start.x) * dx + (f64::from(v) - linear.start.y) * dy)
                        / len2
                }
            };
            (t, &linear.stops, linear.extend)
        }
        Paint::Radial(radial) => {
            // The parameter is the largest `t` solving
            // `|pd - t·dc| = r0 + t·dr`; the `Gradient::radial` constructor
            // makes coincident circles, where the quadratic degenerates to
            // the radial distance over the radius delta.
            let (dcx, dcy) = (
                radial.end_center.x - radial.start_center.x,
                radial.end_center.y - radial.start_center.y,
            );
            let (pdx, pdy) = (
                f64::from(u) - radial.start_center.x,
                f64::from(v) - radial.start_center.y,
            );
            let dr = radial.end_radius - radial.start_radius;
            let t = if dcx.abs() < 1e-12 && dcy.abs() < 1e-12 {
                if dr.abs() < 1e-12 {
                    f64::INFINITY
                } else {
                    (pdx.hypot(pdy) - radial.start_radius) / dr
                }
            } else {
                radial_t(pdx, pdy, dcx, dcy, radial.start_radius, dr)
            };
            (t, &radial.stops, radial.extend)
        }
        Paint::Sweep(sweep) => {
            let span = sweep.end_angle - sweep.start_angle;
            let t = if span.abs() < f64::EPSILON {
                f64::INFINITY
            } else {
                (f64::from(v) - sweep.center.y)
                    .atan2(f64::from(u) - sweep.center.x)
                    .rem_euclid(core::f64::consts::TAU)
                    / span
            };
            (t, &sweep.stops, sweep.extend)
        }
        // Solid, mesh, image, shader and transformed paints never sit inside
        // a `Gradient` leaf's non-mesh paint: mesh gradients are routed to
        // `static_mesh_view` before reaching this backend.
        _ => return WorkingColor::TRANSPARENT,
    };
    let Some(t) = extend_t(t as f32, extend) else {
        return WorkingColor::TRANSPARENT;
    };
    stop_color(stops, t)
}

/// The two-circle radial parameter: the largest `t` solving
/// `|pd - t·dc| = r0 + t·dr` whose circle radius is non-negative.
fn radial_t(pdx: f64, pdy: f64, dcx: f64, dcy: f64, r0: f64, dr: f64) -> f64 {
    let a = dcx.mul_add(dcx, dcy * dcy) - dr * dr;
    let b = 2.0 * r0.mul_add(dr, -(pdx * dcx + pdy * dcy));
    let c = pdx.mul_add(pdx, pdy * pdy) - r0 * r0;
    let mut best = f64::NEG_INFINITY;
    if a.abs() < 1e-12 {
        if b.abs() > 1e-12 {
            let t = -c / b;
            if r0 + t * dr >= 0.0 {
                best = t;
            }
        }
    } else {
        let disc = b.mul_add(b, -4.0 * a * c);
        if disc >= 0.0 {
            let root = disc.sqrt();
            for t in [(-b + root) / (2.0 * a), (-b - root) / (2.0 * a)] {
                if t.is_finite() && r0 + t * dr >= 0.0 {
                    best = best.max(t);
                }
            }
        }
    }
    best
}

/// Maps a paint parameter through its continuation mode; `None` outside the
/// range under `Extend::None`.
fn extend_t(t: f32, extend: Extend) -> Option<f32> {
    match extend {
        Extend::Pad => Some(t.clamp(0.0, 1.0)),
        Extend::Repeat => Some(t - t.floor()),
        Extend::Reflect => {
            let m = (t * 0.5).floor().mul_add(-2.0, t);
            Some(if m > 1.0 { 2.0 - m } else { m })
        }
        Extend::None => (0.0..=1.0).contains(&t).then_some(t),
    }
}

/// Interpolates the stops at `t` in `[0, 1]` in the working space.
fn stop_color(stops: &[ColorStop], t: f32) -> WorkingColor {
    match stops.binary_search_by(|stop| stop.offset.total_cmp(&t)) {
        Ok(index) => stops[index].color,
        Err(0) => stops[0].color,
        Err(index) if index >= stops.len() => stops[stops.len() - 1].color,
        Err(index) => {
            let (a, b) = (&stops[index - 1], &stops[index]);
            let factor = (t - a.offset) / (b.offset - a.offset);
            lerp(a.color, b.color, factor)
        }
    }
}

/// Paints `gradient` into the `clip` region of the frame at signed `origin`
/// with cell `size`, compositing each half-cell sample over the color already
/// underneath.
///
/// `origin` may be negative (scrolled content); `clip` is the part that
/// intersects the buffer. Sampling coordinates stay relative to the full
/// frame so a partially visible gradient does not shift its colors.
pub fn draw_gradient(
    gradient: &Gradient,
    origin: (i32, i32),
    size: (u16, u16),
    clip: CellRect,
    theme_bg: Color,
    buf: &mut Buffer,
) {
    let (fx, fy) = origin;
    let width = f32::from(size.0);
    let height = f32::from(size.1);
    for row in clip.top()..clip.bottom() {
        let dy = i32::from(row) - fy;
        let top = (dy as f32 * 2.0 + 0.5) / (height * 2.0);
        let bottom = (dy as f32 * 2.0 + 1.5) / (height * 2.0);
        for col in clip.left()..clip.right() {
            let u = ((i32::from(col) - fx) as f32 + 0.5) / width;
            let under = cell_under(buf[(col, row)].bg, theme_bg);
            let style = Style::default()
                .fg(composite_over(sample(gradient, u, top), under))
                .bg(composite_over(sample(gradient, u, bottom), under));
            buf.set_stringn(col, row, "▀", 1, style);
        }
    }
}
