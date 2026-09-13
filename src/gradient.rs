//! Sampling of [`ResolvedGradient`] onto the terminal cell grid.
//!
//! WaterUI resolves linear, radial and angular gradients into a
//! `ResolvedGradient` raw view (mesh gradients stay on the GPU path and reach
//! this backend as a `GpuSurface`). A terminal has no pixel fill primitive,
//! so each cell is drawn as `▀`: its foreground is the color sampled at the
//! top half of the cell and its background the color sampled at the bottom
//! half, yielding two gradient rows per terminal row.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as CellRect;
use ratatui::style::{Color, Style};
use waterui_graphics::color::ResolvedColor;
use waterui_graphics::gradient_renderer::{GradientType, ResolvedGradient};

use crate::style::{cell_under, composite_over};

/// Samples `gradient` at normalized point `(u, v)` inside its frame.
///
/// Points and radii are in the gradient's normalized `[0, 1]` space, matching
/// the `ResolvedGradient` contract; the returned color is in linear space.
#[must_use]
pub fn sample(gradient: &ResolvedGradient, u: f32, v: f32) -> ResolvedColor {
    let t = match gradient.gradient_type {
        GradientType::Linear => {
            let [sx, sy] = gradient.start_point;
            let (dx, dy) = (gradient.end_point[0] - sx, gradient.end_point[1] - sy);
            let len2 = dx.mul_add(dx, dy * dy);
            if len2 <= f32::EPSILON {
                0.0
            } else {
                ((u - sx) * dx + (v - sy) * dy) / len2
            }
        }
        GradientType::Radial => {
            let [cx, cy] = gradient.start_point;
            let distance = (u - cx).hypot(v - cy);
            (distance - gradient.start_value) / (gradient.end_value - gradient.start_value)
        }
        GradientType::Angular => {
            let [cx, cy] = gradient.start_point;
            let sweep = gradient.end_value - gradient.start_value;
            let relative =
                ((v - cy).atan2(u - cx) - gradient.start_value).rem_euclid(core::f32::consts::TAU);
            if relative > sweep {
                1.0
            } else {
                relative / sweep
            }
        }
        // Mesh gradients are constructed as `GpuSurface`s upstream and never
        // reach `ResolvedGradient`; the discriminant is exhaustive anyway.
        GradientType::Mesh => 0.0,
    };
    stop_color(gradient, t.clamp(0.0, 1.0))
}

/// Interpolates the gradient stops at `t` in `[0, 1]`.
fn stop_color(gradient: &ResolvedGradient, t: f32) -> ResolvedColor {
    let stops = &gradient.stops;
    match stops.binary_search_by(|stop| stop.position.total_cmp(&t)) {
        Ok(index) => stops[index].color,
        Err(0) => stops[0].color,
        Err(index) if index >= stops.len() => stops[stops.len() - 1].color,
        Err(index) => {
            let (a, b) = (stops[index - 1], stops[index]);
            let factor = (t - a.position) / (b.position - a.position);
            a.color.lerp(b.color, factor)
        }
    }
}

/// Paints `gradient` into the `clip` region of `frame`, compositing each
/// half-cell sample over the color already underneath.
///
/// `frame` is the node's full cell frame; `clip` is the part that intersects
/// the buffer. Sampling coordinates stay relative to `frame` so a partially
/// visible gradient does not shift its colors.
pub fn draw_gradient(
    gradient: &ResolvedGradient,
    frame: CellRect,
    clip: CellRect,
    theme_bg: Color,
    buf: &mut Buffer,
) {
    let width = f32::from(frame.width);
    let height = f32::from(frame.height);
    for row in clip.top()..clip.bottom() {
        let top = (f32::from(row - frame.y) * 2.0 + 0.5) / (height * 2.0);
        let bottom = (f32::from(row - frame.y) * 2.0 + 1.5) / (height * 2.0);
        for col in clip.left()..clip.right() {
            let u = (f32::from(col - frame.x) + 0.5) / width;
            let under = cell_under(buf[(col, row)].bg, theme_bg);
            let style = Style::default()
                .fg(composite_over(sample(gradient, u, top), under))
                .bg(composite_over(sample(gradient, u, bottom), under));
            buf.set_stringn(col, row, "▀", 1, style);
        }
    }
}
