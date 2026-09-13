//! Terminal rendering of `GpuSurface` content (images, mesh gradients,
//! shader surfaces).
//!
//! A `GpuSurface` owns a `GpuView` that only speaks wgpu, so the backend
//! rasterizes it once into an offscreen `RGBA8` texture through the shared
//! [`GpuRuntime`]. On terminals with a graphics protocol the pixels are
//! encoded once into a ratatui-image [`Protocol`] (Kitty, Sixel, or iTerm2)
//! and drawn as a real image; everywhere else they map onto `▀` half-block
//! cells — each cell shows two vertically stacked source pixels as its fore-
//! and background colors. The raster happens lazily at the first drawn frame,
//! when the cell-quantized size is known; later draws resample the cached
//! pixels, so animated `GpuView`s appear as a still frame.

use std::cell::{Cell, RefCell};

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect as CellRect, Size};
use ratatui::style::Style;
use ratatui::widgets::Widget;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
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
/// then the rasterized pixels. On terminals with a graphics protocol the
/// pixels are encoded once into a [`Protocol`] and drawn as a real image;
/// otherwise they map onto `▀` half-block cells.
pub struct GpuState {
    surface: RefCell<Option<GpuSurface>>,
    runtime: Option<GpuRuntime>,
    env: Environment,
    raster: RefCell<Option<Raster>>,
    protocol: RefCell<Option<Protocol>>,
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
            protocol: RefCell::new(None),
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

    /// Encodes the raster into a terminal graphics protocol when `picker`
    /// reports one. The protocol is rebuilt when the frame's cell size
    /// changes. Returns `true` when a real image protocol took over the area.
    fn draw_protocol(&self, picker: &Picker, frame: CellRect, buf: &mut Buffer) -> bool {
        if matches!(picker.protocol_type(), ProtocolType::Halfblocks) {
            return false;
        }
        let raster = self.raster.borrow();
        let Some(raster) = raster.as_ref() else {
            return false;
        };
        let size = Size::new(frame.width, frame.height);
        let mut slot = self.protocol.borrow_mut();
        let stale = slot.as_ref().is_none_or(|proto| proto.size() != size);
        if stale {
            let Some(image) =
                image::RgbaImage::from_raw(raster.width, raster.height, raster.rgba8.clone())
            else {
                return false;
            };
            match picker.new_protocol(
                image::DynamicImage::ImageRgba8(image),
                size,
                Resize::Fit(None),
            ) {
                Ok(protocol) => *slot = Some(protocol),
                Err(error) => {
                    tracing::warn!("terminal image protocol encoding failed: {error}");
                    return false;
                }
            }
        }
        let Some(protocol) = slot.as_ref() else {
            return false;
        };
        Image::new(protocol).render(frame, buf);
        true
    }

    /// Draws the rasterized pixels (or a placeholder) into the `clip` region
    /// of `frame`.
    ///
    /// The surface is rasterized at `frame`'s full size; `clip` only bounds
    /// which cells are written, so a partially visible image shows a window
    /// into the full content rather than a rescaled copy. A terminal graphics
    /// protocol is used only when the whole frame is visible — Sixel and
    /// iTerm2 cannot clip mid-image.
    pub fn draw(
        &self,
        frame: CellRect,
        clip: CellRect,
        picker: Option<&Picker>,
        theme: &Theme,
        buf: &mut Buffer,
    ) {
        self.rasterize(
            u32::from(frame.width).max(1),
            u32::from(frame.height).max(1) * 2,
        );
        let raster = self.raster.borrow();
        if raster.is_none() {
            let muted = Style::default().fg(theme.muted);
            buf.set_stringn(clip.x, clip.y, "[gpu]", 5, muted);
            return;
        }
        if clip == frame
            && let Some(picker) = picker
            && self.draw_protocol(picker, frame, buf)
        {
            return;
        }
        let raster = raster.as_ref().unwrap();
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
