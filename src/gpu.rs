//! Terminal rendering of GPU and scene content (images, mesh gradients,
//! shader surfaces, canvases).
//!
//! `GpuContentView` content only speaks wgpu, so the backend rasterizes it
//! through a persistent [`GpuContentRenderer`] on the shared [`GpuRuntime`]
//! and copies the presented texture back each frame. `SceneView` content
//! records into a Cherenkov engine, so it renders through a CPU
//! [`OffscreenRenderer`] — the same raster feeds every terminal path either
//! way. On kitty terminals the pixels are transmitted once onto a stable
//! image id and bound to the grid with unicode placeholders — resizes only
//! update the placement, never retransmit pixels, and the node deletes the
//! image on drop. Animated content re-renders while it asks for frames —
//! the engine's `Next`, a `RedrawHandle` request, or a scene invalidation —
//! and retransmits in place onto the same image id; the event loop's 80 ms
//! frame timer is the throttle. On Sixel/iTerm2 the raster is encoded
//! through ratatui-image's [`Protocol`] once — those protocols cannot
//! re-place cheaply, so they keep the first frame; everywhere else the
//! pixels map onto `▀` half-block cells.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect as CellRect, Size};
use ratatui::style::Style;
use ratatui::widgets::Widget;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use waterui_core::Environment;
use waterui_graphics::cherenkov::{Display, FrameTime, Next};
use waterui_graphics::cherenkov_cpu::Raster;
use waterui_graphics::gpu::GpuContentRenderer;
use waterui_graphics::wgpu;
use waterui_graphics::{
    GpuContentView, GpuRuntime, OffscreenRenderer, OffscreenSize, RedrawHandle, SceneView,
};

use crate::kitty::{KittyChannel, KittyImage, draw_placeholders};
use crate::style::{Theme, cell_under, composite_rgb8};

/// Largest side of a transmitted raster, in pixels. Fixed-resolution
/// transmission is what makes terminal resizes free — kitty rescales the
/// placement, the pixel data never changes.
const MAX_TRANSMIT_PX: u32 = 1024;

/// `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`; the name is not re-exported.
const ROW_ALIGNMENT: u32 = 256;

/// One rasterized frame, in `RGBA8` row-major pixels.
struct Raster8 {
    width: u32,
    height: u32,
    rgba8: Vec<u8>,
}

/// Environment marker carried by every non-topmost child of a
/// `FixedContainer` — i.e. the layers a `background(image)` puts behind its
/// content. On kitty the image becomes a *real* placement at z-index −1
/// instead of placeholder cells, so text drawn over it shows the image
/// through the cells' default background. Other graphics paths ignore the
/// marker: pixels land in cells and later siblings overwrite them, which
/// degrades to the same z-order at cell granularity.
pub struct ImageUnderlay;

/// The transmitted kitty image plus its on-screen placement state.
struct KittySlot {
    image: KittyImage,
    /// The cell dims the virtual placement was last sized to (placeholder
    /// path).
    cells: (u16, u16),
    /// The real placement currently on screen (underlay path): the visible
    /// cell rectangle and the source pixel rect it shows. `None` while no
    /// placement exists.
    placed: Option<Placed>,
}

/// Signature of a real placement on screen: which cells it occupies and
/// which sub-rect of the transmitted image those cells show.
#[derive(Clone, Copy, PartialEq)]
struct Placed {
    cells: (u16, u16, u16, u16),
    src: (u32, u32, u32, u32),
}

/// The leaf waiting for its first frame.
enum Pending {
    Gpu(GpuContentView),
    Scene(SceneView),
}

/// A retained `GpuContentView` renderer presenting into an owned texture.
struct GpuSession {
    view: GpuContentView,
    renderer: GpuContentRenderer,
    /// Target `present` draws into and `readback_frame` copies out.
    texture: wgpu::Texture,
    /// Staging buffer for the readback.
    buffer: wgpu::Buffer,
    /// `buffer`'s bytes per row, 256-aligned.
    stride: u32,
}

/// The retained renderer installed by the first draw.
enum Session {
    /// `GpuContentView` content presented into an owned texture.
    Gpu(Box<GpuSession>),
    /// `SceneView` content rendered by the CPU raster engine.
    Scene {
        view: SceneView,
        renderer: OffscreenRenderer<Raster>,
        /// The pixel target `render` draws into and reads back.
        size: OffscreenSize,
    },
}

/// A GPU/scene node: holds the leaf until its first frame is drawn, then
/// the retained renderer plus the rasterized pixels. On kitty the session
/// animates — each requested frame re-renders and retransmits onto the same
/// image id; on other protocols the first frame is a static snapshot;
/// without a graphics protocol the pixels map onto `▀` cells.
pub struct GpuState {
    pending: RefCell<Option<Pending>>,
    session: RefCell<Option<Session>>,
    runtime: Option<GpuRuntime>,
    env: Environment,
    /// Wake target handed to the app so redraw requests from another thread
    /// reach the event loop between frames.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Set by the `RedrawHandle`/scene invalidator when content asks for a
    /// frame; consumed by `pump`.
    requested: Arc<AtomicBool>,
    /// The engine's answer for when the content next needs a frame.
    next: RefCell<Next>,
    /// Shared animation-source counter; `animating` tracks this node's own
    /// contribution so several sources can hold the frame timer at once.
    animated: Rc<Cell<u32>>,
    animating: Cell<bool>,
    raster: RefCell<Option<Raster8>>,
    /// The transmitted kitty image plus its placement state.
    kitty: RefCell<Option<KittySlot>>,
    protocol: RefCell<Option<Protocol>>,
    failed: Cell<bool>,
}

impl GpuState {
    /// Creates GPU-backed state for a `GpuContentView`. `runtime` is `None`
    /// when the terminal host has no usable GPU adapter; the node then draws
    /// a placeholder marker.
    pub fn new(
        view: GpuContentView,
        runtime: Option<GpuRuntime>,
        env: &Environment,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
        animated: Rc<Cell<u32>>,
    ) -> Self {
        Self::pending(Pending::Gpu(view), runtime, env, waker, animated)
    }

    /// Creates raster state for a `SceneView`; the CPU raster engine needs
    /// no GPU adapter.
    pub fn scene(
        view: SceneView,
        env: &Environment,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
        animated: Rc<Cell<u32>>,
    ) -> Self {
        Self::pending(Pending::Scene(view), None, env, waker, animated)
    }

    fn pending(
        pending: Pending,
        runtime: Option<GpuRuntime>,
        env: &Environment,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
        animated: Rc<Cell<u32>>,
    ) -> Self {
        Self {
            pending: RefCell::new(Some(pending)),
            session: RefCell::new(None),
            runtime,
            env: env.clone(),
            waker,
            requested: Arc::new(AtomicBool::new(false)),
            next: RefCell::new(Next::Idle),
            animated,
            animating: Cell::new(false),
            raster: RefCell::new(None),
            kitty: RefCell::new(None),
            protocol: RefCell::new(None),
            failed: Cell::new(false),
        }
    }

    /// The `RedrawHandle`/invalidator that wakes this node and the app's
    /// event loop when content asks for a frame.
    fn wake(&self) -> impl Fn() + Send + Sync + 'static {
        let requested = Arc::clone(&self.requested);
        let waker = self.waker.clone();
        move || {
            requested.store(true, Ordering::Release);
            if let Some(waker) = &waker {
                waker();
            }
        }
    }

    /// Installs the renderer on first draw and queues the first frame.
    fn ensure_session(&self, cols: u32, sub_rows: u32) {
        if self.session.borrow().is_some() || self.failed.get() {
            return;
        }
        let Some(pending) = self.pending.borrow_mut().take() else {
            self.failed.set(true);
            return;
        };
        let size = OffscreenSize::try_from_pixels(cols, sub_rows)
            .expect("raster target is clamped to be non-empty");
        match pending {
            Pending::Gpu(mut view) => {
                let Some(runtime) = self.runtime.as_ref() else {
                    self.failed.set(true);
                    return;
                };
                let content = view.take_engine_content();
                let (texture, buffer, stride) = readback_target(runtime, size);
                match GpuContentRenderer::new(
                    runtime,
                    runtime.context(),
                    content,
                    size,
                    RedrawHandle::new(self.wake()),
                ) {
                    Ok(renderer) => {
                        *self.session.borrow_mut() = Some(Session::Gpu(Box::new(GpuSession {
                            view,
                            renderer,
                            texture,
                            buffer,
                            stride,
                        })));
                    }
                    Err(error) => {
                        tracing::warn!("GPU content renderer failed to start: {error}");
                        self.failed.set(true);
                    }
                }
            }
            Pending::Scene(view) => match OffscreenRenderer::<Raster>::cpu() {
                Ok(renderer) => {
                    let mut view = view;
                    view.content_mut()
                        .set_invalidator(Some(Rc::new(self.wake())));
                    *self.session.borrow_mut() = Some(Session::Scene {
                        view,
                        renderer,
                        size,
                    });
                }
                Err(error) => {
                    tracing::warn!("scene offscreen renderer failed to start: {error}");
                    self.failed.set(true);
                }
            },
        }
    }

    /// Renders the next frame into the raster when the session asks for it,
    /// and retransmits onto the live kitty image. Animation only runs on
    /// kitty — the other paths keep their first frame.
    fn pump(&self) {
        if self.env.get::<KittyChannel>().is_none() {
            return;
        }
        let mut session = self.session.borrow_mut();
        let Some(session) = session.as_mut() else {
            return;
        };
        let want_frame = self.requested.swap(false, Ordering::Acquire)
            || matches!(*self.next.borrow(), Next::At { .. });
        if want_frame {
            match session {
                Session::Gpu(session) => {
                    session.view.frame();
                    match session.renderer.present(
                        &session.texture,
                        Display {
                            scale: 1.0,
                            headroom: 1.0,
                        },
                        FrameTime::now(),
                    ) {
                        Ok(next) => {
                            *self.next.borrow_mut() = next;
                            if let Some(raster) = readback_frame(
                                self.runtime.as_ref(),
                                &session.texture,
                                &session.buffer,
                                session.stride,
                            ) {
                                self.publish(&raster);
                            }
                        }
                        Err(error) => {
                            tracing::warn!("GPU content frame failed: {error}");
                            self.failed.set(true);
                        }
                    }
                }
                Session::Scene {
                    view,
                    renderer,
                    size,
                } => match renderer.render(view.content_mut(), *size, 1.0) {
                    Ok(image) => {
                        let raster = Raster8 {
                            width: image.width,
                            height: image.height,
                            rgba8: image.premultiplied_rgba8(),
                        };
                        self.publish(&raster);
                    }
                    Err(error) => {
                        tracing::warn!("scene offscreen frame failed: {error}");
                        self.failed.set(true);
                    }
                },
            }
        }
        // The session keeps the loop's frame timer alive while the view asks
        // for frames; once it settles, release this node's contribution so the
        // loop can sleep again.
        let wants = matches!(*self.next.borrow(), Next::At { .. })
            || self.requested.load(Ordering::Acquire);
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

    /// Stores the raster and retransmits it onto the live kitty image.
    fn publish(&self, raster: &Raster8) {
        if let Some(slot) = self.kitty.borrow().as_ref()
            && let Some(channel) = self.env.get::<KittyChannel>()
        {
            channel.send(if slot.placed.is_some() || self.is_underlay() {
                slot.image.store(&raster.rgba8)
            } else {
                slot.image.frame(&raster.rgba8, slot.cells.0, slot.cells.1)
            });
        }
        *self.raster.borrow_mut() = Some(Raster8 {
            width: raster.width,
            height: raster.height,
            rgba8: raster.rgba8.clone(),
        });
    }

    /// Whether this node was dispatched as a non-topmost `FixedContainer`
    /// child — the background layer a `background(image)` view produces.
    fn is_underlay(&self) -> bool {
        self.env.get::<ImageUnderlay>().is_some()
    }

    /// Kitty path: transmit the raster once onto a stable image id. Ordinary
    /// images bind to the grid through unicode placeholders — a resize sends
    /// `a=p,U=1` only, and the cells clip and scroll with the layout. An
    /// [`ImageUnderlay`] instead becomes a real placement at z-index −1
    /// below the text layer, re-anchored and re-clipped whenever its
    /// visible cell rect moves. Returns `true` when the image took over the
    /// area.
    fn draw_kitty(
        &self,
        raster: &Raster8,
        size: (u16, u16),
        area: CellRect,
        origin: (i32, i32),
        buf: &mut Buffer,
    ) -> bool {
        let Some(channel) = self.env.get::<KittyChannel>() else {
            return false;
        };
        let underlay = self.is_underlay();
        let mut slot = self.kitty.borrow_mut();
        match slot.as_mut() {
            Some(slot) if !underlay && slot.cells != size => {
                channel.send(slot.image.resize_placement(size.0, size.1));
                slot.cells = size;
            }
            None => {
                let image = KittyImage::new(channel.alloc(), raster.width, raster.height);
                channel.send(if underlay {
                    image.store(&raster.rgba8)
                } else {
                    image.create(&raster.rgba8, size.0, size.1)
                });
                *slot = Some(KittySlot {
                    image,
                    cells: size,
                    placed: None,
                });
            }
            _ => {}
        }
        let slot = slot.as_mut().unwrap();
        if underlay {
            self.draw_underlay(slot, raster, size, area, origin, channel);
        } else {
            let skipped = (
                u16::try_from(i32::from(area.left()) - origin.0).unwrap_or(0),
                u16::try_from(i32::from(area.top()) - origin.1).unwrap_or(0),
            );
            draw_placeholders(&slot.image, size.0, size.1, area, skipped, buf);
        }
        true
    }

    /// Keeps a real kitty placement anchored under the visible part of the
    /// frame at z-index −1: the image shows through cells with a default
    /// background, so later siblings' text draws over it. `area` is the
    /// already-clipped visible rect; the placement's source rect crops the
    /// image to match, and the bytes move the cursor to `area`'s origin —
    /// they are emitted before the cell diff, which repositions the cursor
    /// for its own writes. No cell writes happen here; the image lives
    /// entirely below the text layer.
    fn draw_underlay(
        &self,
        slot: &mut KittySlot,
        raster: &Raster8,
        size: (u16, u16),
        area: CellRect,
        origin: (i32, i32),
        channel: &KittyChannel,
    ) {
        let placed = (area.width > 0 && area.height > 0).then(|| Placed {
            cells: (area.x, area.y, area.width, area.height),
            src: (
                (i64::from(area.x) - i64::from(origin.0)) as u32 * raster.width
                    / u32::from(size.0).max(1),
                (i64::from(area.y) - i64::from(origin.1)) as u32 * raster.height
                    / u32::from(size.1).max(1),
                u32::from(area.width) * raster.width / u32::from(size.0).max(1),
                u32::from(area.height) * raster.height / u32::from(size.1).max(1),
            ),
        });
        if slot.placed == placed {
            return;
        }
        match placed {
            Some(placed) => {
                channel.send(format!("\x1b[{};{}H", area.y + 1, area.x + 1).into_bytes());
                channel.send(slot.image.place_clipped(
                    0,
                    placed.src,
                    placed.cells.2,
                    placed.cells.3,
                    -1,
                ));
            }
            None => channel.send(slot.image.delete_placement(0)),
        }
        slot.placed = placed;
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
    /// The content is rasterized at a fixed pixel resolution
    /// (`MAX_TRANSMIT_PX` cap); `clip` only bounds which cells are written.
    /// On kitty the placeholders clip and scroll with the grid at any
    /// visibility; on Sixel/iTerm2 a real image is used only when the whole
    /// frame is visible, since those protocols cannot clip mid-image.
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

/// Creates the texture `GpuContentRenderer::present` draws into and the
/// staging buffer its pixels are copied back through.
fn readback_target(
    runtime: &GpuRuntime,
    size: OffscreenSize,
) -> (wgpu::Texture, wgpu::Buffer, u32) {
    let stride = (size.width() * 4).div_ceil(ROW_ALIGNMENT) * ROW_ALIGNMENT;
    let texture = runtime.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("waterui-tui gpu readback"),
        size: wgpu::Extent3d {
            width: size.width(),
            height: size.height(),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let buffer = runtime.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("waterui-tui gpu readback staging"),
        size: u64::from(stride) * u64::from(size.height()),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    (texture, buffer, stride)
}

/// Copies the presented texture into the staging buffer and maps it back as
/// tightly packed `RGBA8` rows.
fn readback_frame(
    runtime: Option<&GpuRuntime>,
    texture: &wgpu::Texture,
    buffer: &wgpu::Buffer,
    stride: u32,
) -> Option<Raster8> {
    let runtime = runtime?;
    let device = runtime.device();
    let (width, height) = (texture.width(), texture.height());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("waterui-tui gpu readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    runtime.queue().submit(Some(encoder.finish()));
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("wgpu device poll for readback map succeeds");
    rx.recv()
        .expect("map_async always answers")
        .expect("readback buffer maps");
    let packed = {
        let mapped = slice.get_mapped_range().expect("readback buffer maps");
        mapped
            .chunks(stride as usize)
            .take(height as usize)
            .flat_map(|row| row[..width as usize * 4].iter().copied())
            .collect()
    };
    buffer.unmap();
    Some(Raster8 {
        width,
        height,
        rgba8: packed,
    })
}

impl Drop for GpuState {
    /// Orders deletion of the transmitted kitty image (`a=d`). The bytes go
    /// through the channel outbox — `Drop` cannot reach the terminal itself,
    /// so the app loop emits whatever is queued before leaving the screen.
    fn drop(&mut self) {
        if let Some(slot) = self.kitty.get_mut().take()
            && let Some(channel) = self.env.get::<KittyChannel>()
        {
            channel.send(slot.image.delete());
        }
        if self.animating.get() {
            self.animated.set(self.animated.get() - 1);
        }
    }
}
