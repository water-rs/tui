//! The retained terminal node tree.
//!
//! [`crate::TuiRenderer`] dispatches WaterUI views into `Node`s. A node is both
//! a layout leaf — it implements [`SubView`] so shared layout algorithms can
//! measure and place it — and a drawable cell region. Layout happens in logical
//! points; the cell rect is materialized by [`Node::set_frame`].

use std::cell::{Cell, RefCell};
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
use waterui_core::layout::{
    HorizontalAlignment, Layout, Point, ProposalSize, Rect as PtRect, Size, StretchAxis, SubView,
    ViewDimensions, measure_layout,
};
use waterui_core::views::SharedAnyViews;
use waterui_core::{AnyView, Environment};
use waterui_graphics::color::ResolvedColor;
use waterui_text::styled::StyledStr;

use crate::style::{Theme, chunk_style, tui_color};
use crate::units::{LINE_HEIGHT, PT_PER_ROW, to_cells};

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

/// A lazily populated container (`LazyContainer`) node.
pub struct LazyState {
    /// The layout algorithm placing the children.
    pub layout: Box<dyn Layout>,
    /// The view collection; retained so updates can rebuild children.
    pub contents: SharedAnyViews<AnyView>,
    /// The materialized children, rebuilt in place on collection updates.
    pub children: Rc<RefCell<Vec<Node>>>,
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
    pub fn set_frame(&mut self, frame: PtRect) {
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
            _ => {}
        }
    }

    /// Draws the node into the buffer. Children draw after their chrome.
    pub fn render(&self, buf: &mut Buffer, ctx: &DrawCtx) {
        let area = self.frame.get();
        if area.is_empty() {
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
            Kind::Empty | Kind::Container(_) | Kind::Lazy(_) | Kind::Dynamic(_) => {}
            Kind::Background(color) => {
                buf.set_style(area, Style::default().bg(tui_color(color.get())));
            }
        }

        match &self.kind {
            Kind::Lazy(state) => {
                for child in state.children.borrow().iter() {
                    child.render(buf, ctx);
                }
            }
            Kind::Dynamic(slot) => slot.borrow().render(buf, ctx),
            _ => {
                for child in &self.children {
                    child.render(buf, ctx);
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
        let area = self.frame.get();
        let inside = area.x <= col
            && col < area.x.saturating_add(area.width)
            && area.y <= row
            && row < area.y.saturating_add(area.height);
        if !inside {
            return None;
        }
        // Children draw above their parent; later children draw above earlier
        // ones, so hit-test in reverse order.
        for child in self.children.iter().rev() {
            if let hit @ Some(_) = child.hit(col, row) {
                return hit;
            }
        }
        match &self.kind {
            Kind::Lazy(state) => {
                for child in state.children.borrow().iter().rev() {
                    if let hit @ Some(_) = child.hit(col, row) {
                        return hit;
                    }
                }
            }
            Kind::Dynamic(slot) => {
                if let hit @ Some(_) = slot.borrow().hit(col, row) {
                    return hit;
                }
            }
            _ => {}
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
                Kind::Field(field) => field_key(field, key),
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

    /// Activates the node owning `id` (mouse click).
    pub fn activate(&mut self, id: u32) -> bool {
        if self.focus == Some(id) {
            return match &mut self.kind {
                Kind::Button { action, .. } => {
                    (action.borrow_mut())(&self.env);
                    true
                }
                Kind::Toggle { value, .. } => {
                    value.set(!value.get());
                    true
                }
                _ => true,
            };
        }
        for child in &mut self.children {
            if child.activate(id) {
                return true;
            }
        }
        match &mut self.kind {
            Kind::Lazy(state) => state
                .children
                .borrow_mut()
                .iter_mut()
                .any(|child| child.activate(id)),
            Kind::Dynamic(slot) => slot.borrow_mut().activate(id),
            _ => false,
        }
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
            Kind::Divider { vertical } => {
                if *vertical {
                    Size::new(1.0, LINE_HEIGHT)
                } else {
                    Size::new(0.0, LINE_HEIGHT)
                }
            }
            Kind::Fill(_) => Size::new(proposal.width_or(0.0), proposal.height_or(0.0)),
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

fn field_key(field: &FieldState, key: &KeyEvent) -> bool {
    let mut chars: Vec<char> = field.value.get().to_plain().chars().collect();
    let mut cursor = field.cursor.get().min(chars.len());
    let mut edited = false;
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            chars.insert(cursor, c);
            cursor += 1;
            edited = true;
        }
        KeyCode::Backspace if cursor > 0 => {
            chars.remove(cursor - 1);
            cursor -= 1;
            edited = true;
        }
        KeyCode::Delete if cursor < chars.len() => {
            chars.remove(cursor);
            edited = true;
        }
        KeyCode::Left => cursor = cursor.saturating_sub(1),
        KeyCode::Right => cursor = (cursor + 1).min(chars.len()),
        KeyCode::Home => cursor = 0,
        KeyCode::End => cursor = chars.len(),
        _ => return false,
    }
    if edited {
        field
            .value
            .set(StyledStr::plain(chars.iter().collect::<String>()));
    }
    field.cursor.set(cursor);
    true
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
