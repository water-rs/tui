//! The retained terminal node tree.
//!
//! [`crate::TuiRenderer`] dispatches WaterUI views into `Node`s. A node is both
//! a layout leaf — it implements [`SubView`] so shared layout algorithms can
//! measure and place it — and a drawable cell region. Layout happens in logical
//! points; the cell rect is materialized by [`Node::set_frame`].

use std::cell::{Cell, RefCell};
use std::ops::RangeInclusive;
use std::rc::Rc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nami::watcher::BoxWatcherGuard;
use nami::{Binding, Computed, Signal};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect as CellRect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;
use waterui_controls::button::ButtonStyle;
use waterui_controls::toggle::ToggleStyle;
use waterui_core::Retain;
use waterui_core::handler::BoxedAction;
use waterui_core::id::Id;
use waterui_core::layout::{
    HorizontalAlignment, Layout, Point, ProposalSize, Rect as PtRect, Size, StretchAxis, SubView,
    ViewDimensions, measure_layout,
};
use waterui_core::views::SharedAnyViews;
use waterui_core::{AnyView, Environment};
use waterui_form::secure::Secure;
use waterui_graphics::color::ResolvedColor;
use waterui_graphics::gradient_renderer::ResolvedGradient;
use waterui_internal::component::progress::ProgressStyle;
use waterui_layout::scroll::Axis as ScrollAxis;
use waterui_text::styled::StyledStr;

use crate::gpu::GpuState;
use crate::gradient::draw_gradient;
use crate::style::{Theme, chunk_style, tui_color};
use crate::units::{LINE_HEIGHT, PT_PER_ROW, cols_for, rows_for, to_cells};

/// Everything a frame draw needs that nodes cannot own themselves.
pub struct DrawCtx<'a> {
    /// The environment the subtree was dispatched in.
    pub env: &'a Environment,
    /// Theme slots resolved for this frame.
    pub theme: &'a Theme,
    /// The currently focused node id, if any.
    pub focused: Option<u32>,
    /// Receives the terminal cursor position when a focused field draws.
    pub cursor: &'a Cell<Option<(u16, u16)>>,
    /// Terminal graphics protocol picker, when the terminal supports one.
    /// `None` (headless buffers, terminals without graphics) falls back to
    /// half-block rendering.
    pub picker: Option<&'a ratatui_image::picker::Picker>,
    /// Animation frame counter, advanced by the event loop while animated
    /// nodes (spinners) exist.
    pub tick: u64,
}

/// A live editable [`TextField`](waterui_controls::text_field::TextField) node.
pub struct FieldState {
    /// Semantic label, drawn plain before the editable text.
    pub label: Computed<StyledStr>,
    /// The bound value; edits write back through the binding.
    pub value: Binding<StyledStr>,
    /// Placeholder shown while the value is empty.
    pub prompt: Computed<StyledStr>,
    /// Cursor position as a char index into the plain text.
    pub cursor: Cell<usize>,
}

/// A live editable `SecureField` node — like [`FieldState`] but the binding
/// carries a zeroizing [`Secure`] and the display is masked.
pub struct SecureState {
    /// Semantic label, drawn plain before the editable text.
    pub label: Computed<StyledStr>,
    /// The bound secret; edits write back through the binding.
    pub value: Binding<Secure>,
    /// Cursor position as a char index into the secret.
    pub cursor: Cell<usize>,
}

/// Per-tab chrome state for a [`Kind::Tabs`] node.
pub struct TabEntry {
    /// Stable tab identifier matched against the selection binding.
    pub id: Id,
    /// Whether the tab can be selected.
    pub enabled: Computed<bool>,
    /// Optional badge count drawn after the label.
    pub badge: Option<Computed<i32>>,
}

/// A lazily populated container (`LazyContainer`) node.
pub struct LazyState {
    /// The layout algorithm placing the children.
    pub layout: Box<dyn Layout>,
    /// The view collection; retained so updates can rebuild children.
    pub contents: SharedAnyViews<AnyView>,
    /// The materialized children, rebuilt in place on collection updates.
    pub children: Rc<RefCell<Vec<Node>>>,
}

/// A scrollable viewport (`ScrollView`) node.
///
/// The child keeps its frame in *content* coordinates (unshifted); `offset`
/// is applied as a draw-time shift and a hit-test translation, so cell rects
/// never need negative coordinates.
pub struct ScrollState {
    /// Which directions scroll.
    pub axis: ScrollAxis,
    /// Current scroll offset in cell columns/rows.
    pub offset: Cell<(i32, i32)>,
    /// Content extent in cell columns/rows, refreshed by `set_frame`.
    pub extent: Cell<(u16, u16)>,
    /// Programmatic scroll target in cells, set by a `ScrollController` watch.
    pub requested: Rc<Cell<Option<(i32, i32)>>>,
}

/// The rendering payload of a node.
pub enum Kind {
    /// Nothing to draw; still participates in layout.
    Empty,
    /// A text paragraph.
    Text {
        /// The (possibly styled) contents.
        content: Computed<StyledStr>,
        /// Per-line paragraph alignment.
        alignment: Computed<HorizontalAlignment>,
        /// Maximum number of rendered lines.
        line_limit: Option<usize>,
    },
    /// A clickable button. `children[0]` is the label node.
    Button {
        /// The action invoked on activation.
        action: RefCell<BoxedAction>,
        /// The requested button style.
        style: ButtonStyle,
    },
    /// A boolean toggle. `children[0]` is the label node.
    Toggle {
        /// The bound value.
        value: Binding<bool>,
        /// The requested toggle style.
        style: ToggleStyle,
    },
    /// A single-line text field.
    Field(FieldState),
    /// A masked text field (`SecureField`); same editing, `•` display.
    Secure(SecureState),
    /// A value slider. `children[0..2]` are label/min/max label nodes;
    /// `track` is the cell column range written by `set_frame`.
    Slider {
        /// The bound value.
        value: Binding<f64>,
        /// Allowed range.
        range: RangeInclusive<f64>,
        /// Track cell columns `[start, end)`, assigned during `set_frame`.
        track: Cell<(u16, u16)>,
    },
    /// A `[-] v [+]` stepper. `children[0]` is the label node.
    Stepper {
        /// The bound value.
        value: Binding<i32>,
        /// Increment per activation.
        step: Computed<i32>,
        /// Allowed range.
        range: RangeInclusive<i32>,
        /// Optional formatted display value layered over the raw number.
        formatter: Option<Computed<StyledStr>>,
    },
    /// A progress indicator. `children[0..1]` are label and value-label nodes;
    /// `bar` is the cell column range written by `set_frame`.
    Progress {
        /// Determinate fraction `0.0..=1.0`.
        value: Computed<f64>,
        /// Linear bar, circular spinner, or morphing loader.
        style: ProgressStyle,
        /// Bar cell columns `[start, end)`, assigned during `set_frame`.
        bar: Cell<(u16, u16)>,
    },
    /// A scrollable viewport; `children[0]` is the content root.
    Scroll(ScrollState),
    /// A tab container: `children[0..n]` are tab label nodes and
    /// `children[n..]` the matching content roots; only the selected content
    /// is laid out and drawn.
    Tabs {
        /// The selected tab id.
        selection: Binding<Id>,
        /// The tabs in bar order.
        tabs: Vec<TabEntry>,
    },
    /// A `NavigationView` bar: `children[0]` is the title, `children[1]` the
    /// content. The bar row collapses when `hidden` holds.
    NavBar {
        /// Whether the title row is hidden.
        hidden: Computed<bool>,
    },
    /// A separating line; `vertical` selects `│` over `─`.
    Divider {
        /// Draw a vertical instead of horizontal rule.
        vertical: bool,
    },
    /// Fills its frame with a color.
    Fill(Computed<ResolvedColor>),
    /// A fixed container: `layout` places `children`.
    Container(Box<dyn Layout>),
    /// A lazy container: `state.children` holds materialized nodes.
    Lazy(LazyState),
    /// A view slot that is re-dispatched when its signal updates.
    Dynamic(Rc<RefCell<Node>>),
    /// Paints `children[0]`'s frame with a background color underneath.
    Background(Computed<ResolvedColor>),
    /// A linear/radial/angular gradient filling its frame.
    Gradient(ResolvedGradient),
    /// GPU-rendered content (images, mesh gradients, shader surfaces)
    /// rasterized into the cell grid.
    Gpu(GpuState),
}

/// A dispatched view: layout leaf, drawable region, and event target.
///
/// Nodes are retained for the lifetime of their parent; dropping a node drops
/// its watcher guards and retained metadata.
pub struct Node {
    /// The rendering payload.
    pub kind: Kind,
    /// Fixed children (containers, control labels, metadata content).
    pub children: Vec<Node>,
    /// The stretch axis reported to parent layouts.
    pub stretch: StretchAxis,
    /// Layout priority from `Metadata<LayoutPriority>`.
    pub priority: i32,
    /// The quantized cell frame assigned by [`Node::set_frame`].
    pub frame: Cell<CellRect>,
    /// Focus id for interactive kinds.
    pub focus: Option<u32>,
    /// Two-way `.focused()` binding; written when the subtree gains or loses
    /// focus, watched to request focus from code.
    pub focus_signal: Option<Binding<bool>>,
    /// Visual offset in points from `Metadata<Offset>`; shifts the whole
    /// subtree without affecting layout.
    pub offset: Cell<(f32, f32)>,
    /// Watcher guards keeping signal subscriptions alive.
    pub guards: Vec<BoxWatcherGuard>,
    /// `Retain` metadata values kept alive with the node.
    pub retained: Vec<Retain>,
    /// The environment the subtree was dispatched in (for actions/hooks).
    pub env: Environment,
}

impl Node {
    /// Creates an empty leaf node.
    pub fn empty(env: &Environment) -> Self {
        Self {
            kind: Kind::Empty,
            children: Vec::new(),
            stretch: StretchAxis::None,
            priority: 0,
            frame: Cell::new(CellRect::ZERO),
            focus: None,
            focus_signal: None,
            offset: Cell::new((0.0, 0.0)),
            guards: Vec::new(),
            retained: Vec::new(),
            env: env.clone(),
        }
    }

    /// Creates a node with the given payload.
    pub fn new(kind: Kind, env: &Environment) -> Self {
        let mut node = Self::empty(env);
        node.kind = kind;
        node
    }

    fn child_refs(children: &[Node]) -> Vec<&dyn SubView> {
        children.iter().map(|child| child as &dyn SubView).collect()
    }

    /// Assigns the point-space frame, quantizes it to cells, and places children.
    ///
    /// `Layout::place` works in the parent's coordinate space, so frames are
    /// absolute point coordinates; every child quantizes independently.
    /// `self.offset` shifts the whole subtree visually without affecting
    /// layout (`Metadata<Offset>`).
    pub fn set_frame(&mut self, frame: PtRect) {
        let offset = self.offset.get();
        let frame = PtRect::new(
            Point::new(frame.x() + offset.0, frame.y() + offset.1),
            Size::new(frame.width(), frame.height()),
        );
        self.frame.set(to_cells(frame));
        match &mut self.kind {
            Kind::Container(layout) => {
                let refs = Self::child_refs(&self.children);
                let frames = layout.place(frame, &refs);
                for (child, child_frame) in self.children.iter_mut().zip(frames) {
                    child.set_frame(child_frame);
                }
            }
            Kind::Lazy(state) => {
                let frames = {
                    let children = state.children.borrow();
                    let refs = Self::child_refs(&children);
                    state.layout.place(frame, &refs)
                };
                let mut children = state.children.borrow_mut();
                for (child, child_frame) in children.iter_mut().zip(frames) {
                    child.set_frame(child_frame);
                }
            }
            Kind::Dynamic(slot) => slot.borrow_mut().set_frame(frame),
            Kind::Background(_) => {
                for child in &mut self.children {
                    child.set_frame(frame);
                }
            }
            Kind::Button { style, .. } => {
                if let Some(label) = self.children.first_mut() {
                    let inset = if bordered(*style) { 2.0 } else { 0.0 };
                    label.set_frame(frame.inset(0.0, 0.0, inset, 0.0));
                }
            }
            Kind::Toggle { .. } => {
                if let Some(label) = self.children.first_mut() {
                    label.set_frame(frame.inset(0.0, 0.0, 4.0, 0.0));
                }
            }
            Kind::Slider { track, .. } => {
                // label [min] ──track── [max]: the trailing child hugs the
                // right edge, the rest flow from the left, and the track
                // takes whatever remains between them.
                let n = self.children.len();
                let widths: Vec<f32> = self
                    .children
                    .iter()
                    .map(|child| {
                        child
                            .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                            .size
                            .width
                    })
                    .collect();
                let mut x = frame.x();
                for (index, child) in self
                    .children
                    .iter_mut()
                    .enumerate()
                    .take(n.saturating_sub(1))
                {
                    child.set_frame(PtRect::new(
                        Point::new(x, frame.y()),
                        Size::new(widths[index], LINE_HEIGHT),
                    ));
                    x += widths[index] + 1.0;
                }
                let last_w = widths.last().copied().unwrap_or(0.0);
                let lx = (frame.x() + frame.width() - last_w).max(x);
                if let Some(last) = self.children.last_mut() {
                    last.set_frame(PtRect::new(
                        Point::new(lx, frame.y()),
                        Size::new(last_w, LINE_HEIGHT),
                    ));
                }
                track.set((cols_for(x), cols_for(lx).max(cols_for(x))));
            }
            Kind::Progress { style, bar, .. } if !matches!(*style, ProgressStyle::Linear) => {
                // Spinner/ring glyph at the leading cell; labels flow after it.
                let mut x = frame.x() + 2.0;
                for child in &mut self.children {
                    let natural = child
                        .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                        .size
                        .width;
                    child.set_frame(PtRect::new(
                        Point::new(x, frame.y()),
                        Size::new(natural, LINE_HEIGHT),
                    ));
                    x += natural + 1.0;
                }
                bar.set((0, 0));
            }
            Kind::Progress { bar: track, .. } => {
                // label ──bar── [value]: the trailing child hugs the right
                // edge and the bar takes whatever remains.
                let n = self.children.len();
                let widths: Vec<f32> = self
                    .children
                    .iter()
                    .map(|child| {
                        child
                            .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                            .size
                            .width
                    })
                    .collect();
                let mut x = frame.x();
                for (index, child) in self
                    .children
                    .iter_mut()
                    .enumerate()
                    .take(n.saturating_sub(1))
                {
                    child.set_frame(PtRect::new(
                        Point::new(x, frame.y()),
                        Size::new(widths[index], LINE_HEIGHT),
                    ));
                    x += widths[index] + 1.0;
                }
                let last_w = widths.last().copied().unwrap_or(0.0);
                let lx = (frame.x() + frame.width() - last_w).max(x);
                if let Some(last) = self.children.last_mut() {
                    last.set_frame(PtRect::new(
                        Point::new(lx, frame.y()),
                        Size::new(last_w, LINE_HEIGHT),
                    ));
                }
                track.set((cols_for(x), cols_for(lx).max(cols_for(x))));
            }
            Kind::Stepper { .. } => {
                if let Some(label) = self.children.first_mut() {
                    let natural = label
                        .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                        .size
                        .width;
                    label.set_frame(PtRect::new(
                        Point::new(frame.x(), frame.y()),
                        Size::new(natural, LINE_HEIGHT),
                    ));
                }
            }
            Kind::Secure(_) | Kind::Field(_) => {}
            Kind::Scroll(scroll) => {
                // Content is measured with the scroll axis unbounded and keeps
                // its frame in content coordinates; the offset is applied as
                // a draw-time shift.
                let proposal = match scroll.axis {
                    ScrollAxis::Vertical => ProposalSize::new(Some(frame.width()), None),
                    ScrollAxis::Horizontal => ProposalSize::new(None, Some(frame.height())),
                    _ => ProposalSize::new(None, None),
                };
                if let Some(child) = self.children.first_mut() {
                    let natural = child.measure(proposal).size;
                    let horizontal = !matches!(scroll.axis, ScrollAxis::Vertical);
                    let vertical = !matches!(scroll.axis, ScrollAxis::Horizontal);
                    let size = Size::new(
                        if horizontal {
                            natural.width.max(frame.width())
                        } else {
                            frame.width()
                        },
                        if vertical {
                            natural.height.max(frame.height())
                        } else {
                            frame.height()
                        },
                    );
                    child.set_frame(PtRect::new(Point::new(frame.x(), frame.y()), size));
                    scroll
                        .extent
                        .set((cols_for(size.width), rows_for(size.height)));
                }
            }
            Kind::Tabs { selection, tabs } => {
                let n = tabs.len();
                let mut x = frame.x();
                for label in self.children.iter_mut().take(n) {
                    let natural = label
                        .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                        .size
                        .width;
                    label.set_frame(PtRect::new(
                        Point::new(x, frame.y()),
                        Size::new(natural, LINE_HEIGHT),
                    ));
                    x += natural + 2.0;
                }
                let selected = tabs
                    .iter()
                    .position(|tab| tab.id == selection.get())
                    .unwrap_or(0);
                let content = PtRect::new(
                    Point::new(frame.x(), frame.y() + LINE_HEIGHT),
                    Size::new(frame.width(), (frame.height() - LINE_HEIGHT).max(0.0)),
                );
                for (index, child) in self.children.iter_mut().enumerate().skip(n) {
                    child.set_frame(if index - n == selected {
                        content
                    } else {
                        PtRect::new(
                            Point::new(frame.x(), frame.y() + LINE_HEIGHT),
                            Size::new(0.0, 0.0),
                        )
                    });
                }
            }
            Kind::NavBar { hidden } => {
                let bar = if hidden.get() { 0.0 } else { 2.0 * LINE_HEIGHT };
                if let Some(title) = self.children.first_mut() {
                    title.set_frame(PtRect::new(
                        Point::new(frame.x(), frame.y()),
                        Size::new(frame.width(), bar),
                    ));
                }
                if let Some(content) = self.children.get_mut(1) {
                    content.set_frame(PtRect::new(
                        Point::new(frame.x(), frame.y() + bar),
                        Size::new(frame.width(), (frame.height() - bar).max(0.0)),
                    ));
                }
            }
            _ => {}
        }
    }

    /// Draws the node into the buffer. Children draw after their chrome.
    pub fn render(&self, buf: &mut Buffer, ctx: &DrawCtx) {
        self.render_shifted(buf, ctx, buf.area, (0, 0));
    }

    /// Draws the node shifted by `shift` cell columns/rows and clipped to
    /// `clip`. Scrolling applies a negative shift to content; everything else
    /// passes `(0, 0)` and the parent's clip straight through.
    fn render_shifted(&self, buf: &mut Buffer, ctx: &DrawCtx, clip: CellRect, shift: (i32, i32)) {
        // Content can legitimately overflow the screen or a scroll viewport;
        // cell access panics out of bounds, so clip first. Sampled paints
        // (gradient/GPU) still need the unclipped frame to keep their
        // coordinates anchored to the full node, not the window.
        let frame = self.frame.get();
        let fx = i32::from(frame.x) + shift.0;
        let fy = i32::from(frame.y) + shift.1;
        let area = visible(fx, fy, frame.width, frame.height, clip);
        if area.is_empty() && !matches!(self.kind, Kind::Container(_) | Kind::Lazy(_)) {
            return;
        }
        let focused = self.focus.is_some() && self.focus == ctx.focused;
        match &self.kind {
            Kind::Text {
                content,
                alignment,
                line_limit,
            } => {
                draw_text(
                    &content.get(),
                    alignment.get(),
                    *line_limit,
                    area,
                    ctx.theme.text(),
                    buf,
                    ctx,
                );
            }
            Kind::Fill(color) => {
                buf.set_style(area, Style::default().bg(tui_color(color.get())));
            }
            Kind::Divider { vertical } => {
                let style = Style::default().fg(ctx.theme.border);
                if *vertical {
                    for y in area.top()..area.bottom() {
                        buf.set_stringn(area.x, y, "│", 1, style);
                    }
                } else {
                    buf.set_stringn(
                        area.x,
                        area.y,
                        "─".repeat(area.width as usize),
                        area.width as usize,
                        style,
                    );
                }
            }
            Kind::Button { style, .. } => {
                draw_button_chrome(*style, focused, area, buf, ctx);
            }
            Kind::Toggle { value, style } => {
                let on = value.get();
                let glyph = match style {
                    ToggleStyle::Switch => {
                        if on {
                            "(*)"
                        } else {
                            "( )"
                        }
                    }
                    _ => {
                        if on {
                            "[x]"
                        } else {
                            "[ ]"
                        }
                    }
                };
                let mut style = ctx.theme.text();
                if focused {
                    style = style.fg(ctx.theme.accent).add_modifier(Modifier::BOLD);
                }
                buf.set_stringn(area.x, area.y, glyph, 3, style);
            }
            Kind::Field(field) => {
                draw_field(field, focused, area, buf, ctx);
            }
            Kind::Secure(state) => {
                draw_secure(state, focused, area, buf, ctx);
            }
            Kind::Slider {
                value,
                range,
                track,
            } => {
                let (x0, x1) = track.get();
                let span = *range.end() - *range.start();
                let frac = if span.abs() > f64::EPSILON {
                    (value.get() - range.start()) / span
                } else {
                    0.0
                }
                .clamp(0.0, 1.0);
                let width = x1.saturating_sub(x0);
                let thumb = x0 + (f32::from(width.saturating_sub(1)) * frac as f32).round() as u16;
                let mut style = ctx.theme.text();
                if focused {
                    style = style.fg(ctx.theme.accent).add_modifier(Modifier::BOLD);
                }
                if area.y < clip.bottom() && width > 0 {
                    buf.set_stringn(
                        x0,
                        area.y,
                        "─".repeat(width as usize),
                        width as usize,
                        Style::default().fg(ctx.theme.muted),
                    );
                    buf.set_stringn(thumb, area.y, "●", 1, style);
                }
            }
            Kind::Stepper {
                value,
                range,
                formatter,
                ..
            } => {
                let shown = formatter
                    .as_ref()
                    .map(|f| f.get().to_plain().into_string())
                    .unwrap_or_else(|| format!("{}", value.get()));
                let digits = (shown.width() as u16).max(stepper_digits(range));
                let cluster = 3 + 1 + digits + 1 + 3;
                let cx = (area.x + area.width).saturating_sub(cluster);
                let mut style = ctx.theme.text();
                if focused {
                    style = style.fg(ctx.theme.accent).add_modifier(Modifier::BOLD);
                }
                buf.set_stringn(cx, area.y, "[-]", 3, style);
                buf.set_stringn(cx + 4, area.y, shown, digits as usize, ctx.theme.text());
                buf.set_stringn(cx + 4 + digits + 1, area.y, "[+]", 3, style);
            }
            Kind::Progress {
                value,
                style: progress_style,
                bar,
            } => {
                draw_progress(*progress_style, value.get(), bar.get(), area, ctx, buf);
            }
            Kind::Empty | Kind::Container(_) | Kind::Lazy(_) | Kind::Dynamic(_) => {}
            Kind::Background(color) => {
                buf.set_style(area, Style::default().bg(tui_color(color.get())));
            }
            Kind::Gradient(gradient) => {
                draw_gradient(
                    gradient,
                    (fx, fy),
                    (frame.width, frame.height),
                    area,
                    ctx.theme.background,
                    buf,
                );
            }
            Kind::Gpu(state) => {
                state.draw(
                    (fx, fy),
                    (frame.width, frame.height),
                    area,
                    ctx.picker,
                    ctx.theme,
                    buf,
                );
            }
            Kind::Scroll(scroll) => {
                draw_scrollbar(scroll, frame, area, ctx, buf);
            }
            Kind::Tabs { selection, tabs } => {
                let selected = tabs
                    .iter()
                    .position(|tab| tab.id == selection.get())
                    .unwrap_or(0);
                for (index, entry) in tabs.iter().enumerate() {
                    let label = self.children[index].frame.get();
                    if index == selected {
                        buf.set_style(
                            label,
                            Style::default()
                                .fg(ctx.theme.accent)
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        );
                    } else if !entry.enabled.get() {
                        buf.set_style(label, Style::default().fg(ctx.theme.muted));
                    }
                    if let Some(badge) = &entry.badge {
                        let text = format!(" {}", badge.get());
                        buf.set_stringn(
                            label.x + label.width,
                            label.y,
                            text,
                            usize::from(area.right().saturating_sub(label.x + label.width)),
                            Style::default().fg(ctx.theme.muted),
                        );
                    }
                }
            }
            Kind::NavBar { hidden } => {
                if !hidden.get() && area.height > 1 {
                    // Title row is children[0]; the second bar row is a rule.
                    buf.set_stringn(
                        area.x,
                        area.y + 1,
                        "─".repeat(area.width as usize),
                        area.width as usize,
                        Style::default().fg(ctx.theme.border),
                    );
                }
            }
        }

        match &self.kind {
            Kind::Lazy(state) => {
                for child in state.children.borrow().iter() {
                    child.render_shifted(buf, ctx, clip, shift);
                }
            }
            Kind::Dynamic(slot) => slot.borrow().render_shifted(buf, ctx, clip, shift),
            Kind::Scroll(scroll) => {
                // Apply a programmatic scroll request, then draw content
                // shifted by the offset and clipped to the visible viewport.
                if let Some(target) = scroll.requested.take() {
                    scroll.offset.set(self.clamp_offset(target));
                }
                let (ox, oy) = scroll.offset.get();
                let inner = (shift.0 - ox, shift.1 - oy);
                for child in &self.children {
                    child.render_shifted(buf, ctx, area, inner);
                }
            }
            Kind::Tabs { selection, tabs } => {
                let n = tabs.len();
                let selected = tabs
                    .iter()
                    .position(|tab| tab.id == selection.get())
                    .unwrap_or(0);
                for (index, child) in self.children.iter().enumerate() {
                    if index < n || index - n == selected {
                        child.render_shifted(buf, ctx, clip, shift);
                    }
                }
            }
            _ => {
                for child in &self.children {
                    child.render_shifted(buf, ctx, clip, shift);
                }
            }
        }
    }

    /// Collects focusable ids in pre-order.
    pub fn collect_focus(&self, out: &mut Vec<u32>) {
        if let Some(id) = self.focus {
            out.push(id);
        }
        for child in &self.children {
            child.collect_focus(out);
        }
        match &self.kind {
            Kind::Lazy(state) => {
                for child in state.children.borrow().iter() {
                    child.collect_focus(out);
                }
            }
            Kind::Dynamic(slot) => slot.borrow().collect_focus(out),
            _ => {}
        }
    }

    /// Returns the focus id of the topmost focusable node containing a cell.
    pub fn hit(&self, col: u16, row: u16) -> Option<u32> {
        self.hit_at(i32::from(col), i32::from(row))
    }

    /// Hit-test in the caller's coordinate space. Scroll content is stored in
    /// unshifted content coordinates, so a scroll node translates the point by
    /// its offset before recursing.
    fn hit_at(&self, col: i32, row: i32) -> Option<u32> {
        let area = self.frame.get();
        let inside = i32::from(area.x) <= col
            && col < i32::from(area.x) + i32::from(area.width)
            && i32::from(area.y) <= row
            && row < i32::from(area.y) + i32::from(area.height);
        if !inside {
            return None;
        }
        // Children draw above their parent; later children draw above earlier
        // ones, so hit-test in reverse order.
        match &self.kind {
            Kind::Lazy(state) => {
                for child in state.children.borrow().iter().rev() {
                    if let hit @ Some(_) = child.hit_at(col, row) {
                        return hit;
                    }
                }
            }
            Kind::Dynamic(slot) => {
                if let hit @ Some(_) = slot.borrow().hit_at(col, row) {
                    return hit;
                }
            }
            Kind::Scroll(scroll) => {
                let (ox, oy) = scroll.offset.get();
                for child in self.children.iter().rev() {
                    if let hit @ Some(_) = child.hit_at(col + ox, row + oy) {
                        return hit;
                    }
                }
                return self.focus;
            }
            _ => {
                for child in self.children.iter().rev() {
                    if let hit @ Some(_) = child.hit_at(col, row) {
                        return hit;
                    }
                }
            }
        }
        self.focus
    }

    /// Delivers a key press to the node owning `focused`.
    pub fn handle_key(&mut self, focused: u32, key: &KeyEvent) -> bool {
        if self.focus == Some(focused) {
            return match &mut self.kind {
                Kind::Button { action, .. }
                    if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) =>
                {
                    (action.borrow_mut())(&self.env);
                    true
                }
                Kind::Toggle { value, .. }
                    if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) =>
                {
                    value.set(!value.get());
                    true
                }
                Kind::Field(field) => {
                    if key.code == KeyCode::Enter
                        && let Some(submit) = self.env.get::<crate::OnSubmit>()
                    {
                        (submit.0.borrow_mut())(&self.env);
                        return true;
                    }
                    field_key(field, key)
                }
                Kind::Secure(state) => {
                    if key.code == KeyCode::Enter
                        && let Some(submit) = self.env.get::<crate::OnSubmit>()
                    {
                        (submit.0.borrow_mut())(&self.env);
                        return true;
                    }
                    secure_key(state, key)
                }
                Kind::Slider { value, range, .. } => {
                    let step = (*range.end() - *range.start()) / 20.0;
                    match key.code {
                        KeyCode::Left | KeyCode::Down | KeyCode::Char('-') => {
                            value.set((value.get() - step).max(*range.start()));
                            true
                        }
                        KeyCode::Right | KeyCode::Up | KeyCode::Char('+') => {
                            value.set((value.get() + step).min(*range.end()));
                            true
                        }
                        KeyCode::Home => {
                            value.set(*range.start());
                            true
                        }
                        KeyCode::End => {
                            value.set(*range.end());
                            true
                        }
                        _ => false,
                    }
                }
                Kind::Stepper {
                    value, step, range, ..
                } => match key.code {
                    KeyCode::Left | KeyCode::Char('-') => {
                        value.set((value.get() - step.get()).max(*range.start()));
                        true
                    }
                    KeyCode::Right | KeyCode::Char('+') => {
                        value.set((value.get() + step.get()).min(*range.end()));
                        true
                    }
                    _ => false,
                },
                Kind::Scroll(scroll) => {
                    let page = i32::from(self.frame.get().height.saturating_sub(1));
                    let v = matches!(scroll.axis, ScrollAxis::Vertical | ScrollAxis::All);
                    let h = matches!(scroll.axis, ScrollAxis::Horizontal | ScrollAxis::All);
                    match key.code {
                        KeyCode::Up if v => self.scroll_by(0, -1),
                        KeyCode::Down if v => self.scroll_by(0, 1),
                        KeyCode::Left if h => self.scroll_by(-1, 0),
                        KeyCode::Right if h => self.scroll_by(1, 0),
                        KeyCode::PageUp if v => self.scroll_by(0, -page),
                        KeyCode::PageDown if v => self.scroll_by(0, page),
                        KeyCode::Home => self.scroll_to(0, 0),
                        KeyCode::End => self.scroll_to(i32::MAX, i32::MAX),
                        _ => false,
                    }
                }
                Kind::Tabs { selection, tabs } => match key.code {
                    KeyCode::Left | KeyCode::Right => {
                        let current = tabs
                            .iter()
                            .position(|tab| tab.id == selection.get())
                            .unwrap_or(0) as i32;
                        let dir = if key.code == KeyCode::Left { -1 } else { 1 };
                        let n = tabs.len() as i32;
                        for step in 1..=n {
                            let index = (current + dir * step).rem_euclid(n);
                            let tab = &tabs[index as usize];
                            if tab.enabled.get() {
                                selection.set(tab.id);
                                return true;
                            }
                        }
                        false
                    }
                    _ => false,
                },
                _ => false,
            };
        }
        for child in &mut self.children {
            if child.handle_key(focused, key) {
                return true;
            }
        }
        match &mut self.kind {
            Kind::Lazy(state) => {
                let mut children = state.children.borrow_mut();
                children
                    .iter_mut()
                    .any(|child| child.handle_key(focused, key))
            }
            Kind::Dynamic(slot) => slot.borrow_mut().handle_key(focused, key),
            _ => false,
        }
    }

    /// Clamps a scroll offset (cells) to the content extent.
    fn clamp_offset(&self, target: (i32, i32)) -> (i32, i32) {
        let Kind::Scroll(scroll) = &self.kind else {
            return target;
        };
        let viewport = self.frame.get();
        let (cw, ch) = scroll.extent.get();
        (
            target.0.clamp(0, i32::from(cw) - i32::from(viewport.width)),
            target
                .1
                .clamp(0, i32::from(ch) - i32::from(viewport.height)),
        )
    }

    /// Scrolls the node by a cell delta, when it is a scroll viewport.
    fn scroll_by(&self, dx: i32, dy: i32) -> bool {
        let Kind::Scroll(scroll) = &self.kind else {
            return false;
        };
        let next = self.clamp_offset((scroll.offset.get().0 + dx, scroll.offset.get().1 + dy));
        if next == scroll.offset.get() {
            return false;
        }
        scroll.offset.set(next);
        true
    }

    /// Scrolls the node to an absolute cell offset, when it is a scroll
    /// viewport.
    fn scroll_to(&self, x: i32, y: i32) -> bool {
        let Kind::Scroll(scroll) = &self.kind else {
            return false;
        };
        let next = self.clamp_offset((x, y));
        if next == scroll.offset.get() {
            return false;
        }
        scroll.offset.set(next);
        true
    }

    /// Delivers a mouse wheel scroll at a screen cell; returns `true` when a
    /// scroll viewport under the point consumed it.
    pub fn scroll_at(&self, col: u16, row: u16, dx: i32, dy: i32) -> bool {
        self.scroll_at_inner(i32::from(col), i32::from(row), dx, dy)
    }

    fn scroll_at_inner(&self, col: i32, row: i32, dx: i32, dy: i32) -> bool {
        let area = self.frame.get();
        let inside = i32::from(area.x) <= col
            && col < i32::from(area.x) + i32::from(area.width)
            && i32::from(area.y) <= row
            && row < i32::from(area.y) + i32::from(area.height);
        if !inside {
            return false;
        }
        match &self.kind {
            Kind::Scroll(scroll) => {
                let (ox, oy) = scroll.offset.get();
                for child in self.children.iter().rev() {
                    if child.scroll_at_inner(col + ox, row + oy, dx, dy) {
                        return true;
                    }
                }
                self.scroll_by(dx, dy)
            }
            Kind::Lazy(state) => {
                for child in state.children.borrow().iter().rev() {
                    if child.scroll_at_inner(col, row, dx, dy) {
                        return true;
                    }
                }
                false
            }
            Kind::Dynamic(slot) => slot.borrow().scroll_at_inner(col, row, dx, dy),
            _ => {
                for child in self.children.iter().rev() {
                    if child.scroll_at_inner(col, row, dx, dy) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Delivers a mouse press (`drag = false`) or left-button drag at a screen
    /// cell. Returns the focus id the press landed on, if any.
    pub fn mouse(&mut self, col: u16, row: u16, drag: bool) -> Option<u32> {
        self.mouse_at(i32::from(col), i32::from(row), drag)
    }

    fn mouse_at(&mut self, col: i32, row: i32, drag: bool) -> Option<u32> {
        let area = self.frame.get();
        let inside = i32::from(area.x) <= col
            && col < i32::from(area.x) + i32::from(area.width)
            && i32::from(area.y) <= row
            && row < i32::from(area.y) + i32::from(area.height);
        if !inside {
            return None;
        }
        match &mut self.kind {
            Kind::Lazy(state) => {
                for child in state.children.borrow_mut().iter_mut().rev() {
                    if let hit @ Some(_) = child.mouse_at(col, row, drag) {
                        return hit;
                    }
                }
            }
            Kind::Dynamic(slot) => {
                if let hit @ Some(_) = slot.borrow_mut().mouse_at(col, row, drag) {
                    return hit;
                }
            }
            Kind::Scroll(scroll) => {
                let (ox, oy) = scroll.offset.get();
                for child in self.children.iter_mut().rev() {
                    if let hit @ Some(_) = child.mouse_at(col + ox, row + oy, drag) {
                        return hit;
                    }
                }
                return self.focus;
            }
            _ => {
                for child in self.children.iter_mut().rev() {
                    if let hit @ Some(_) = child.mouse_at(col, row, drag) {
                        return hit;
                    }
                }
            }
        }
        match &mut self.kind {
            Kind::Button { action, .. } if !drag => {
                (action.borrow_mut())(&self.env);
            }
            Kind::Toggle { value, .. } if !drag => value.set(!value.get()),
            Kind::Slider {
                value,
                range,
                track,
            } => {
                let (x0, x1) = track.get();
                let width = i32::from(x1) - i32::from(x0);
                if width > 0 {
                    let frac =
                        ((col - i32::from(x0)) as f64 / f64::from(width - 1)).clamp(0.0, 1.0);
                    value.set(range.start() + frac * (range.end() - range.start()));
                }
            }
            Kind::Stepper {
                value,
                step,
                range,
                formatter,
            } if !drag => {
                let shown = formatter
                    .as_ref()
                    .map(|f| f.get().to_plain().width() as u16)
                    .unwrap_or(0);
                let digits = shown.max(stepper_digits(range));
                let cluster = 3 + 1 + digits + 1 + 3;
                let cx = i32::from(area.x) + i32::from(area.width) - i32::from(cluster);
                let minus = (cx..cx + 3).contains(&col);
                let plus = (cx + i32::from(cluster) - 3..cx + i32::from(cluster)).contains(&col);
                if minus {
                    value.set((value.get() - step.get()).max(*range.start()));
                } else if plus {
                    value.set((value.get() + step.get()).min(*range.end()));
                }
            }
            Kind::Tabs { selection, tabs } if !drag && row == i32::from(area.y) => {
                for (index, entry) in tabs.iter().enumerate() {
                    let label = self.children[index].frame.get();
                    if i32::from(label.x) <= col && col < i32::from(label.x + label.width) + 2 {
                        if entry.enabled.get() {
                            selection.set(entry.id);
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
        self.focus
    }

    /// Synchronizes `Metadata<Focused>` bindings with the focused id.
    /// Returns `true` when this subtree contains the focused node.
    pub fn sync_focused(&self, focused: Option<u32>) -> bool {
        let mut contains = self.focus.is_some() && self.focus == focused;
        for child in &self.children {
            contains |= child.sync_focused(focused);
        }
        match &self.kind {
            Kind::Lazy(state) => {
                for child in state.children.borrow().iter() {
                    contains |= child.sync_focused(focused);
                }
            }
            Kind::Dynamic(slot) => {
                contains |= slot.borrow().sync_focused(focused);
            }
            _ => {}
        }
        if let Some(binding) = &self.focus_signal {
            binding.set(contains);
        }
        contains
    }
}

impl SubView for Node {
    fn measure(&self, proposal: ProposalSize) -> ViewDimensions {
        let size = match &self.kind {
            Kind::Empty => Size::zero(),
            Kind::Text {
                content,
                line_limit,
                ..
            } => measure_text(&content.get(), *line_limit),
            Kind::Button { style, .. } => {
                let label = self
                    .children
                    .first()
                    .map(|child| child.measure(proposal).size)
                    .unwrap_or_default();
                if bordered(*style) {
                    Size::new(label.width + 4.0, label.height.max(LINE_HEIGHT))
                } else {
                    label
                }
            }
            Kind::Toggle { .. } => {
                let label = self
                    .children
                    .first()
                    .map(|child| child.measure(proposal).size)
                    .unwrap_or_default();
                Size::new(label.width + 4.0, label.height.max(LINE_HEIGHT))
            }
            Kind::Field(field) => {
                let label_width = field.label.get().to_plain().width() as f32;
                let content_width = (field
                    .value
                    .get()
                    .to_plain()
                    .width()
                    .max(field.prompt.get().to_plain().width())
                    as f32)
                    .max(8.0);
                Size::new(label_width + 2.0 + content_width, LINE_HEIGHT)
            }
            Kind::Secure(state) => {
                let label_width = state.label.get().to_plain().width() as f32;
                let content_width = (state.value.get().expose().chars().count() as f32).max(8.0);
                Size::new(label_width + 2.0 + content_width, LINE_HEIGHT)
            }
            Kind::Slider { .. } => {
                let labels: f32 = self
                    .children
                    .iter()
                    .map(|child| {
                        child
                            .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                            .size
                            .width
                    })
                    .sum();
                Size::new(labels + 3.0 + 8.0, LINE_HEIGHT)
            }
            Kind::Stepper { range, .. } => {
                let label = self
                    .children
                    .first()
                    .map(|child| {
                        child
                            .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                            .size
                            .width
                    })
                    .unwrap_or(0.0);
                Size::new(
                    label + 2.0 + f32::from(stepper_digits(range)) + 8.0,
                    LINE_HEIGHT,
                )
            }
            Kind::Progress { .. } => {
                let labels: f32 = self
                    .children
                    .iter()
                    .map(|child| {
                        child
                            .measure(ProposalSize::new(None, Some(LINE_HEIGHT)))
                            .size
                            .width
                    })
                    .sum();
                Size::new(labels + 2.0 + 10.0, LINE_HEIGHT)
            }
            Kind::Scroll(scroll) => {
                let proposal = match scroll.axis {
                    ScrollAxis::Vertical => ProposalSize::new(proposal.width, None),
                    ScrollAxis::Horizontal => ProposalSize::new(None, proposal.height),
                    _ => ProposalSize::new(None, None),
                };
                let natural = self
                    .children
                    .first()
                    .map(|child| child.measure(proposal).size)
                    .unwrap_or_default();
                Size::new(
                    proposal.width_or(natural.width),
                    proposal.height_or(natural.height),
                )
            }
            Kind::Tabs { tabs, .. } => {
                let n = tabs.len();
                let content = self.children[n..]
                    .iter()
                    .map(|child| child.measure(proposal).size)
                    .fold(Size::zero(), |a, b| {
                        Size::new(a.width.max(b.width), a.height.max(b.height))
                    });
                Size::new(
                    proposal.width_or(content.width),
                    proposal.height_or(content.height + LINE_HEIGHT),
                )
            }
            Kind::NavBar { hidden } => {
                let content = self
                    .children
                    .get(1)
                    .map(|child| child.measure(proposal).size)
                    .unwrap_or_default();
                let bar = if hidden.get() { 0.0 } else { 2.0 * LINE_HEIGHT };
                Size::new(
                    proposal.width_or(content.width),
                    proposal.height_or(content.height + bar),
                )
            }
            Kind::Divider { vertical } => {
                if *vertical {
                    Size::new(1.0, LINE_HEIGHT)
                } else {
                    Size::new(0.0, LINE_HEIGHT)
                }
            }
            Kind::Fill(_) | Kind::Gradient(_) | Kind::Gpu(_) => {
                Size::new(proposal.width_or(0.0), proposal.height_or(0.0))
            }
            Kind::Container(layout) => {
                return measure_layout(
                    layout.as_ref(),
                    proposal,
                    &Self::child_refs(&self.children),
                );
            }
            Kind::Lazy(state) => {
                let children = state.children.borrow();
                return measure_layout(
                    state.layout.as_ref(),
                    proposal,
                    &Self::child_refs(&children),
                );
            }
            Kind::Dynamic(slot) => return slot.borrow().measure(proposal),
            Kind::Background(_) => {
                return self
                    .children
                    .first()
                    .map(|child| child.measure(proposal))
                    .unwrap_or_default();
            }
        };
        ViewDimensions::new(size)
    }

    fn stretch_axis(&self) -> StretchAxis {
        self.stretch
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

/// Whether a button style draws the `[ ... ]` chrome.
const fn bordered(style: ButtonStyle) -> bool {
    !matches!(
        style,
        ButtonStyle::Plain | ButtonStyle::Borderless | ButtonStyle::Link
    )
}

fn measure_text(content: &StyledStr, line_limit: Option<usize>) -> Size {
    let plain = content.to_plain();
    let mut width = 0usize;
    let mut lines = 0usize;
    for line in plain.split('\n') {
        width = width.max(line.width());
        lines += 1;
    }
    if let Some(limit) = line_limit {
        lines = lines.min(limit);
    }
    Size::new(width as f32, lines as f32 * LINE_HEIGHT)
}

fn draw_text(
    content: &StyledStr,
    alignment: HorizontalAlignment,
    line_limit: Option<usize>,
    area: CellRect,
    base: Style,
    buf: &mut Buffer,
    ctx: &DrawCtx,
) {
    // Split chunks into lines, keeping each piece's style.
    let mut lines: Vec<Vec<(String, Style)>> = vec![Vec::new()];
    for (chunk, style) in content.chunks() {
        let style = chunk_style(style, ctx.env, base);
        let mut first = true;
        for piece in chunk.split('\n') {
            if !first {
                lines.push(Vec::new());
            }
            first = false;
            if !piece.is_empty()
                && let Some(line) = lines.last_mut()
            {
                line.push((piece.to_owned(), style));
            }
        }
    }
    for (row, line) in lines
        .iter()
        .take(line_limit.unwrap_or(usize::MAX))
        .enumerate()
    {
        let y = area.y + row as u16;
        if y >= area.y + area.height {
            break;
        }
        let width: usize = line.iter().map(|(text, _)| text.width()).sum();
        let mut x = if alignment == HorizontalAlignment::Center {
            area.x + (area.width as usize).saturating_sub(width) as u16 / 2
        } else if alignment == HorizontalAlignment::Trailing {
            area.x + area.width.saturating_sub(width as u16)
        } else {
            area.x
        };
        for (text, style) in line {
            let remaining = (area.x + area.width).saturating_sub(x);
            if remaining == 0 {
                break;
            }
            x = buf.set_stringn(x, y, text, remaining as usize, *style).0;
        }
    }
}

fn draw_button_chrome(
    style: ButtonStyle,
    focused: bool,
    area: CellRect,
    buf: &mut Buffer,
    ctx: &DrawCtx,
) {
    match style {
        ButtonStyle::BorderedProminent => {
            let fill = Style::default()
                .fg(ctx.theme.accent_foreground)
                .bg(ctx.theme.accent);
            buf.set_style(area, fill);
            let accent = if focused {
                Style::default()
                    .fg(ctx.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ctx.theme.border)
            };
            buf.set_stringn(area.x, area.y, "▐", 1, accent);
            buf.set_stringn(area.x + area.width - 1, area.y, "▌", 1, accent);
        }
        _ if bordered(style) => {
            let accent = if focused {
                Style::default()
                    .fg(ctx.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ctx.theme.border)
            };
            buf.set_stringn(area.x, area.y, "[", 1, accent);
            buf.set_stringn(area.x + area.width - 1, area.y, "]", 1, accent);
        }
        ButtonStyle::Link => {
            buf.set_style(
                area,
                Style::default()
                    .fg(ctx.theme.accent)
                    .add_modifier(Modifier::UNDERLINED),
            );
        }
        _ => {
            if focused {
                buf.set_style(
                    area,
                    Style::default()
                        .fg(ctx.theme.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
        }
    }
}

/// Intersects a signed-origin rect with `clip`, returning the visible cell
/// rect. Scroll shifts push content origins negative; anything left or above
/// `clip` is simply not drawn.
fn visible(x: i32, y: i32, width: u16, height: u16, clip: CellRect) -> CellRect {
    let x0 = x.max(i32::from(clip.x));
    let y0 = y.max(i32::from(clip.y));
    let x1 = (x + i32::from(width)).min(i32::from(clip.x + clip.width));
    let y1 = (y + i32::from(height)).min(i32::from(clip.y + clip.height));
    if x1 <= x0 || y1 <= y0 {
        CellRect::ZERO
    } else {
        CellRect::new(x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16)
    }
}

/// Display width for a stepper's value: enough digits for both range ends.
fn stepper_digits(range: &RangeInclusive<i32>) -> u16 {
    let digits = |v: i32| v.unsigned_abs().checked_ilog10().unwrap_or(0) as u16 + 1;
    let sign = u16::from(*range.start() < 0 || *range.end() < 0);
    digits(*range.start()).max(digits(*range.end())) + sign
}

fn draw_field(field: &FieldState, focused: bool, area: CellRect, buf: &mut Buffer, ctx: &DrawCtx) {
    let label = field.label.get();
    let label_text = label.to_plain();
    let label_width = label_text.width();
    let muted = Style::default().fg(ctx.theme.muted);
    let mut x = area.x;
    if label_width > 0 {
        x = buf
            .set_stringn(x, area.y, &label_text, label_width, muted)
            .0;
        x = buf.set_stringn(x, area.y, ": ", 2, muted).0;
    }

    let value = field.value.get();
    let plain = value.to_plain();
    let text: &str = &plain;
    let empty = text.is_empty();
    let shown: String = if empty {
        field.prompt.get().to_plain().into_string()
    } else {
        text.to_owned()
    };
    let style = if empty {
        muted.add_modifier(Modifier::ITALIC)
    } else if focused {
        ctx.theme.text().add_modifier(Modifier::UNDERLINED)
    } else {
        ctx.theme.text()
    };
    let max = (area.x + area.width).saturating_sub(x) as usize;
    buf.set_stringn(x, area.y, &shown, max, style);

    if focused {
        let cursor = field.cursor.get().min(shown.chars().count());
        let offset: usize = shown.chars().take(cursor).collect::<String>().width();
        ctx.cursor.set(Some((x + offset as u16, area.y)));
    }
}

fn draw_secure(
    state: &SecureState,
    focused: bool,
    area: CellRect,
    buf: &mut Buffer,
    ctx: &DrawCtx,
) {
    let label = state.label.get();
    let label_text = label.to_plain();
    let label_width = label_text.width();
    let muted = Style::default().fg(ctx.theme.muted);
    let mut x = area.x;
    if label_width > 0 {
        x = buf
            .set_stringn(x, area.y, &label_text, label_width, muted)
            .0;
        x = buf.set_stringn(x, area.y, ": ", 2, muted).0;
    }

    let chars = state.value.get().expose().chars().count();
    let shown = "•".repeat(chars);
    let style = if focused {
        ctx.theme.text().add_modifier(Modifier::UNDERLINED)
    } else {
        ctx.theme.text()
    };
    let max = (area.x + area.width).saturating_sub(x) as usize;
    buf.set_stringn(x, area.y, &shown, max, style);

    if focused {
        let cursor = state.cursor.get().min(chars);
        ctx.cursor.set(Some((x + cursor as u16, area.y)));
    }
}

/// Applies an edit/move key to `chars`/`cursor`; returns `true` when handled.
fn edit_chars(chars: &mut Vec<char>, cursor: &mut usize, key: &KeyEvent) -> bool {
    *cursor = (*cursor).min(chars.len());
    let mut edited = false;
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            chars.insert(*cursor, c);
            *cursor += 1;
            edited = true;
        }
        KeyCode::Backspace if *cursor > 0 => {
            chars.remove(*cursor - 1);
            *cursor -= 1;
            edited = true;
        }
        KeyCode::Delete if *cursor < chars.len() => {
            chars.remove(*cursor);
            edited = true;
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(chars.len()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = chars.len(),
        _ => return false,
    }
    edited
}

fn field_key(field: &FieldState, key: &KeyEvent) -> bool {
    let mut chars: Vec<char> = field.value.get().to_plain().chars().collect();
    let mut cursor = field.cursor.get();
    if edit_chars(&mut chars, &mut cursor, key) {
        field
            .value
            .set(StyledStr::plain(chars.iter().collect::<String>()));
        field.cursor.set(cursor);
        true
    } else {
        false
    }
}

fn secure_key(state: &SecureState, key: &KeyEvent) -> bool {
    let mut chars: Vec<char> = state.value.get().expose().chars().collect();
    let mut cursor = state.cursor.get();
    if edit_chars(&mut chars, &mut cursor, key) {
        state
            .value
            .set(Secure::new(chars.iter().collect::<String>()));
        state.cursor.set(cursor);
        true
    } else {
        false
    }
}

/// Spinner frames for indeterminate progress; indexed by the draw tick.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Quarter-circle fill glyphs for determinate circular progress.
const CIRCLE: &[char] = &['○', '◔', '◑', '◕', '●'];

fn draw_progress(
    style: ProgressStyle,
    value: f64,
    bar: (u16, u16),
    area: CellRect,
    ctx: &DrawCtx,
    buf: &mut Buffer,
) {
    let frac = value.clamp(0.0, 1.0);
    match style {
        ProgressStyle::Linear => {
            let (x0, x1) = bar;
            let width = x1.saturating_sub(x0);
            if width == 0 {
                return;
            }
            let filled = (f32::from(width) * frac as f32).round() as u16;
            if filled > 0 {
                buf.set_stringn(
                    x0,
                    area.y,
                    "█".repeat(filled as usize),
                    filled as usize,
                    Style::default().fg(ctx.theme.accent),
                );
            }
            if width > filled {
                buf.set_stringn(
                    x0 + filled,
                    area.y,
                    "░".repeat((width - filled) as usize),
                    (width - filled) as usize,
                    Style::default().fg(ctx.theme.muted),
                );
            }
        }
        ProgressStyle::Circular => {
            let glyph = CIRCLE[(frac * 4.0).round() as usize];
            buf.set_stringn(area.x, area.y, glyph.to_string(), 1, ctx.theme.text());
        }
        ProgressStyle::Loading => {
            let glyph = SPINNER[(ctx.tick % SPINNER.len() as u64) as usize];
            buf.set_stringn(
                area.x,
                area.y,
                glyph.to_string(),
                1,
                Style::default().fg(ctx.theme.accent),
            );
        }
        _ => {}
    }
}

/// Draws a scrollbar on the viewport's right edge when content overflows.
fn draw_scrollbar(
    scroll: &ScrollState,
    frame: CellRect,
    area: CellRect,
    ctx: &DrawCtx,
    buf: &mut Buffer,
) {
    let (_, oy) = scroll.offset.get();
    let (_, ch) = scroll.extent.get();
    if i32::from(ch) <= i32::from(frame.height) {
        return;
    }
    let track = i32::from(area.height);
    let thumb = (track * track / i32::from(ch)).max(1);
    let max_offset = i32::from(ch) - i32::from(frame.height);
    let top = area.y + (oy * (track - thumb) / max_offset.max(1)) as u16;
    let x = area.x + area.width - 1;
    for y in area.top()..area.bottom() {
        let (glyph, color) = if (top..top + thumb as u16).contains(&y) {
            ("┃", ctx.theme.muted)
        } else {
            ("│", ctx.theme.border)
        };
        buf.set_stringn(x, y, glyph, 1, Style::default().fg(color));
    }
}

/// Converts a cell-space area into the point rect a root node is placed in.
#[must_use]
pub fn screen_points(cols: u16, rows: u16) -> PtRect {
    PtRect::new(
        Point::zero(),
        Size::new(
            f32::from(cols) * crate::units::PT_PER_COL,
            f32::from(rows) * PT_PER_ROW,
        ),
    )
}
