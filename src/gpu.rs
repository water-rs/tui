//! Terminal rendering of `GpuSurface` content (images, mesh gradients,
//! shader surfaces).
//!
//! A `GpuSurface` owns a `GpuView` that only speaks wgpu, so the backend
//! rasterizes it once into an offscreen `RGBA8` texture through the shared
//! [`GpuRuntime`], then maps the pixels onto `▀` half-block cells — each cell
//! shows two vertically stacked source pixels as its fore- and background
//! colors. The raster happens lazily at the first drawn frame, when the
//! cell-quantized size is known; later draws resample the cached pixels, so
//! animated `GpuView`s appear as a still frame.

use std::cell::{Cell, RefCell};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as CellRect;
use ratatui::style::Style;
use waterui_core::Environment;
use waterui_graphics::{GpuRuntime, GpuSurface, OffscreenRenderConfig, OffscreenSize};

use crate::style::{Theme, cell_under, composite_rgb8};

/// One rasterized GPU frame, in `RGBA8` row-major pixels.
struct Raster {
    width: u32,
    height: u32,
    rgba8: Vec<u8>,
}

/// A `GpuSurface` node: holds the surface until its first frame is drawn,
/// then the rasterized pixels.
pub struct GpuState {
    surface: RefCell<Option<GpuSurface>>,
    runtime: Option<GpuRuntime>,
    env: Environment,
    raster: RefCell<Option<Raster>>,
    failed: Cell<bool>,
}

impl GpuState {
    /// Creates GPU-backed state for `surface`. `runtime` is `None` when the
    /// terminal host has no usable GPU adapter; the node then draws a
    /// placeholder marker.
    pub fn new(surface: GpuSurface, runtime: Option<GpuRuntime>, env: &Environment) -> Self {
        Self {
            surface: RefCell::new(Some(surface)),
            runtime,
            env: env.clone(),
            raster: RefCell::new(None),
            failed: Cell::new(false),
        }
    }

    /// Rasterizes the surface once at the current cell size.
    fn rasterize(&self, cols: u32, sub_rows: u32) {
        if self.raster.borrow().is_some() || self.failed.get() {
            return;
        }
        let (Some(runtime), Some(surface)) =
            (self.runtime.as_ref(), self.surface.borrow_mut().take())
        else {
            self.failed.set(true);
            return;
        };
        let size = OffscreenSize::try_from_pixels(cols, sub_rows)
            .expect("raster target is clamped to be non-empty");
        let config = OffscreenRenderConfig::new(size);
        let mut env = self.env.clone();
        match pollster::block_on(surface.render_offscreen(runtime, config, &mut env)) {
            Ok(output) => {
                *self.raster.borrow_mut() = Some(Raster {
                    width: output.width,
                    height: output.height,
                    rgba8: output.rgba8,
                });
            }
            Err(error) => {
                tracing::warn!("offscreen GPU rasterization failed: {error}");
                self.failed.set(true);
            }
        }
    }

    /// Draws the rasterized pixels (or a placeholder) into the `clip` region
    /// of `frame`.
    ///
    /// The surface is rasterized at `frame`'s full size; `clip` only bounds
    /// which cells are written, so a partially visible image shows a window
    /// into the full content rather than a rescaled copy.
    pub fn draw(&self, frame: CellRect, clip: CellRect, theme: &Theme, buf: &mut Buffer) {
        self.rasterize(
            u32::from(frame.width).max(1),
            u32::from(frame.height).max(1) * 2,
        );
        let raster = self.raster.borrow();
        let Some(raster) = raster.as_ref() else {
            let muted = Style::default().fg(theme.muted);
            buf.set_stringn(clip.x, clip.y, "[gpu]", 5, muted);
            return;
        };
        let cell_w = f32::from(frame.width);
        let cell_h = f32::from(frame.height);
        let pixel = |x: u32, y: u32| -> [u8; 4] {
            let offset = ((y * raster.width + x) * 4) as usize;
            raster.rgba8[offset..offset + 4].try_into().unwrap()
        };
        for row in clip.top()..clip.bottom() {
            let top = (f32::from(row - frame.y) * 2.0 + 0.5) / (cell_h * 2.0);
            let bottom = (f32::from(row - frame.y) * 2.0 + 1.5) / (cell_h * 2.0);
            let py_top = ((top * raster.height as f32) as u32).min(raster.height - 1);
            let py_bottom = ((bottom * raster.height as f32) as u32).min(raster.height - 1);
            for col in clip.left()..clip.right() {
                let u = (f32::from(col - frame.x) + 0.5) / cell_w;
                let px = ((u * raster.width as f32) as u32).min(raster.width - 1);
                let under = cell_under(buf[(col, row)].bg, theme.background);
                let [r, g, b, a] = pixel(px, py_top);
                let fg = composite_rgb8(r, g, b, a, under);
                let [r, g, b, a] = pixel(px, py_bottom);
                let bg = composite_rgb8(r, g, b, a, under);
                buf.set_stringn(col, row, "▀", 1, Style::default().fg(fg).bg(bg));
            }
        }
    }
}
