//! Terminal rendering of `GpuSurface` content (images, mesh gradients,
//! shader surfaces).
//!
//! A `GpuSurface` owns a `GpuView` that only speaks wgpu, so the backend
//! rasterizes it through a persistent [`OffscreenSession`] on the shared
//! [`GpuRuntime`]. On kitty terminals the pixels are transmitted once onto a
//! stable image id and bound to the grid with unicode placeholders — resizes
//! only update the placement, never retransmit pixels, and the node deletes
//! the image on drop. Animated views re-render while the session reports
//! `needs_redraw` and retransmit in place onto the same image id; the event
//! loop's 80 ms frame timer is the throttle. On Sixel/iTerm2 the raster is
//! encoded through ratatui-image's [`Protocol`] once — those protocols
//! cannot re-place cheaply, so they keep the first frame; everywhere else
//! the pixels map onto `▀` half-block cells.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect as CellRect, Size};
use ratatui::style::Style;
use ratatui::widgets::Widget;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use waterui_core::Environment;
use waterui_graphics::{
    GpuRuntime, GpuSurface, OffscreenRenderConfig, OffscreenSession, OffscreenSize,
};

use crate::kitty::{KittyChannel, KittyImage, draw_placeholders};
use crate::style::{Theme, cell_under, composite_rgb8};

/// Largest side of a transmitted raster, in pixels. Fixed-resolution
/// transmission is what makes terminal resizes free — kitty rescales the
/// placement, the pixel data never changes.
const MAX_TRANSMIT_PX: u32 = 1024;

/// Fallback frame delta reported to the view on its very first render.
const FIRST_FRAME: Duration = Duration::from_micros(16_667);

/// One rasterized GPU frame, in `RGBA8` row-major pixels.
struct Raster {
    width: u32,
    height: u32,
    rgba8: Vec<u8>,
}

/// A `GpuSurface` node: holds the surface until its first frame is drawn,
/// then an [`OffscreenSession`] plus the rasterized pixels. On kitty the
/// session animates — each requested frame re-renders and retransmits onto
/// the same image id; on other protocols the first frame is a static
/// snapshot; without a graphics protocol the pixels map onto `▀` cells.
pub struct GpuState {
    surface: RefCell<Option<GpuSurface>>,
    session: RefCell<Option<OffscreenSession>>,
    runtime: Option<GpuRuntime>,
    env: Environment,
    /// Wake target installed on the session's `RedrawHandle` so the view can
    /// request frames between event-loop iterations.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Shared animation-source counter; `animating` tracks this node's own
    /// contribution so several sources can hold the frame timer at once.
    animated: Rc<Cell<u32>>,
    animating: Cell<bool>,
    raster: RefCell<Option<Raster>>,
    last_frame: Cell<Option<Instant>>,
    /// The transmitted kitty image plus the cell dims its virtual placement
    /// was last sized to.
    kitty: RefCell<Option<(KittyImage, (u16, u16))>>,
    protocol: RefCell<Option<Protocol>>,
    failed: Cell<bool>,
}

impl GpuState {
    /// Creates GPU-backed state for `surface`. `runtime` is `None` when the
    /// terminal host has no usable GPU adapter; the node then draws a
    /// placeholder marker.
    pub fn new(
        surface: GpuSurface,
        runtime: Option<GpuRuntime>,
        env: &Environment,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
        animated: Rc<Cell<u32>>,
    ) -> Self {
        Self {
            surface: RefCell::new(Some(surface)),
            session: RefCell::new(None),
            runtime,
            env: env.clone(),
            waker,
            animated,
            animating: Cell::new(false),
            raster: RefCell::new(None),
            last_frame: Cell::new(None),
            kitty: RefCell::new(None),
            protocol: RefCell::new(None),
            failed: Cell::new(false),
        }
    }

    /// Starts the offscreen session on first draw, installs the wake target
    /// on its redraw handle, and renders the pending first frame.
    fn ensure_session(&self, cols: u32, sub_rows: u32) {
        if self.session.borrow().is_some() || self.failed.get() {
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
        match pollster::block_on(surface.start_offscreen(runtime, config, &mut env)) {
            Ok(session) => {
                if let Some(waker) = &self.waker {
                    session.redraw_handle().set_waker(Some(Arc::clone(waker)));
                }
                *self.session.borrow_mut() = Some(session);
            }
            Err(error) => {
                tracing::warn!("offscreen GPU session failed to start: {error}");
                self.failed.set(true);
            }
        }
    }

    /// Renders one frame into the raster when the session asks for it, and
    /// retransmits onto the live kitty image. Animation only runs on kitty —
    /// the other paths keep their first frame.
    fn pump(&self) {
        if self.env.get::<KittyChannel>().is_none() {
            return;
        }
        let mut slot = self.session.borrow_mut();
        let Some(session) = slot.as_mut() else { return };
        if session.needs_redraw() {
            let now = Instant::now();
            let delta = self
                .last_frame
                .replace(Some(now))
                .map_or(FIRST_FRAME, |last| now - last);
            session.render(delta);
            match pollster::block_on(session.readback_rgba8()) {
                Ok(output) => {
                    if let Some((image, cells)) = self.kitty.borrow().as_ref()
                        && let Some(channel) = self.env.get::<KittyChannel>()
                    {
                        channel.send(image.frame(&output.rgba8, cells.0, cells.1));
                    }
                    *self.raster.borrow_mut() = Some(Raster {
                        width: output.width,
                        height: output.height,
                        rgba8: output.rgba8,
                    });
                }
                Err(error) => {
                    tracing::warn!("offscreen GPU readback failed: {error}");
                    self.failed.set(true);
                }
            }
        }
        // The session keeps the loop's frame timer alive while the view asks
        // for frames; once it settles, release this node's contribution so the
        // loop can sleep again.
        let wants = session.needs_redraw();
        match (self.animating.get(), wants) {
            (false, true) => {
                self.animating.set(true);
                self.animated.set(self.animated.get() + 1);
            }
            (true, false) => {
                self.animating.set(false);
                self.animated.set(self.animated.get() - 1);
            }
            _ => {}
        }
    }

    /// Kitty path: transmit the raster once onto a stable image id, then keep
    /// the virtual placement sized to the current cell frame — a resize sends
    /// `a=p,U=1` only, never pixels. Placeholders clip and scroll with the
    /// grid, so partial visibility needs no special casing. Returns `true`
    /// when the image took over the area.
    fn draw_kitty(
        &self,
        raster: &Raster,
        size: (u16, u16),
        area: CellRect,
        origin: (i32, i32),
        buf: &mut Buffer,
    ) -> bool {
        let Some(channel) = self.env.get::<KittyChannel>() else {
            return false;
        };
        let mut slot = self.kitty.borrow_mut();
        if let Some((image, cells)) = slot.as_mut() {
            if *cells != size {
                channel.send(image.resize_placement(size.0, size.1));
                *cells = size;
            }
        } else {
            let image = KittyImage::new(channel.alloc(), raster.width, raster.height);
            channel.send(image.create(&raster.rgba8, size.0, size.1));
            *slot = Some((image, size));
        }
        let (image, _) = slot.as_ref().unwrap();
        let skipped = (
            u16::try_from(i32::from(area.left()) - origin.0).unwrap_or(0),
            u16::try_from(i32::from(area.top()) - origin.1).unwrap_or(0),
        );
        draw_placeholders(image, size.0, size.1, area, skipped, buf);
        true
    }

    /// Encodes the raster into a terminal graphics protocol when `picker`
    /// reports one. The protocol is rebuilt when the frame's cell size
    /// changes. Returns `true` when a real image protocol took over the area.
    fn draw_protocol(&self, picker: &Picker, frame: CellRect, buf: &mut Buffer) -> bool {
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
                Resize::Scale(Some(ratatui_image::FilterType::Triangle)),
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
    /// of the frame at signed `origin` with cell `size`.
    ///
    /// The surface is rasterized at a fixed pixel resolution (`MAX_TRANSMIT_PX`
    /// cap); `clip` only bounds which cells are written. On kitty the
    /// placeholders clip and scroll with the grid at any visibility; on
    /// Sixel/iTerm2 a real image is used only when the whole frame is visible,
    /// since those protocols cannot clip mid-image.
    pub fn draw(
        &self,
        origin: (i32, i32),
        size: (u16, u16),
        clip: CellRect,
        picker: Option<&Picker>,
        theme: &Theme,
        buf: &mut Buffer,
    ) {
        let (fx, fy) = origin;
        let graphics =
            picker.filter(|picker| !matches!(picker.protocol_type(), ProtocolType::Halfblocks));
        let (width, height) = match graphics {
            Some(picker) => {
                let font = picker.font_size();
                (
                    (u32::from(size.0).max(1) * u32::from(font.width)).min(MAX_TRANSMIT_PX),
                    (u32::from(size.1).max(1) * u32::from(font.height)).min(MAX_TRANSMIT_PX),
                )
            }
            None => (u32::from(size.0).max(1), u32::from(size.1).max(1) * 2),
        };
        self.ensure_session(width, height);
        self.pump();
        let raster = self.raster.borrow();
        if raster.is_none() {
            let muted = Style::default().fg(theme.muted);
            buf.set_stringn(clip.x, clip.y, "[gpu]", 5, muted);
            return;
        }
        let raster_ref = raster.as_ref().unwrap();
        if let Some(picker) = graphics
            && matches!(picker.protocol_type(), ProtocolType::Kitty)
            && self.draw_kitty(raster_ref, size, clip, origin, buf)
        {
            return;
        }
        let frame =
            (fx >= 0 && fy >= 0).then(|| CellRect::new(fx as u16, fy as u16, size.0, size.1));
        if let (Some(frame), Some(picker)) = (frame, graphics)
            && clip == frame
            && self.draw_protocol(picker, frame, buf)
        {
            return;
        }
        let raster = raster.as_ref().unwrap();
        let cell_w = f32::from(size.0);
        let cell_h = f32::from(size.1);
        let pixel = |x: u32, y: u32| -> [u8; 4] {
            let offset = ((y * raster.width + x) * 4) as usize;
            raster.rgba8[offset..offset + 4].try_into().unwrap()
        };
        for row in clip.top()..clip.bottom() {
            let dy = i32::from(row) - fy;
            let top = (dy as f32 * 2.0 + 0.5) / (cell_h * 2.0);
            let bottom = (dy as f32 * 2.0 + 1.5) / (cell_h * 2.0);
            let py_top = ((top * raster.height as f32) as u32).min(raster.height - 1);
            let py_bottom = ((bottom * raster.height as f32) as u32).min(raster.height - 1);
            for col in clip.left()..clip.right() {
                let u = ((i32::from(col) - fx) as f32 + 0.5) / cell_w;
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

impl Drop for GpuState {
    /// Orders deletion of the transmitted kitty image (`a=d`). The bytes go
    /// through the channel outbox — `Drop` cannot reach the terminal itself,
    /// so the app loop emits whatever is queued before leaving the screen.
    fn drop(&mut self) {
        if let Some((image, _)) = self.kitty.get_mut().take()
            && let Some(channel) = self.env.get::<KittyChannel>()
        {
            channel.send(image.delete());
        }
        if self.animating.get() {
            self.animated.set(self.animated.get() - 1);
        }
    }
}
