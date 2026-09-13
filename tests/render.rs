//! Headless render tests: dispatch a view, place it on a fixed cell grid, and
//! assert on the resulting buffer contents and interaction behavior.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nami::{Binding, SignalExt, binding};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use waterui_controls::{button, toggle};
use waterui_core::{Environment, Str, View};
use waterui_layout::stack::vstack;
use waterui_text::styled::StyledStr;
use waterui_text::text::text;
use waterui_tui::node::{DrawCtx, screen_points};
use waterui_tui::style::Theme;
use waterui_tui::{Node, TuiRenderer, install_terminal_theme};

fn buffer_string(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        out.push('\n');
    }
    out
}

struct Fixture {
    env: Environment,
    renderer: TuiRenderer,
    root: Node,
    theme: Theme,
    cursor: Cell<Option<(u16, u16)>>,
}

impl Fixture {
    fn new(view: impl View, cols: u16, rows: u16) -> Self {
        let mut env = Environment::new();
        install_terminal_theme(&mut env);
        let mut renderer = TuiRenderer::new();
        let root = renderer.dispatch(view, &env);
        let theme = Theme::resolve(&env);
        let mut fixture = Self {
            env,
            renderer,
            root,
            theme,
            cursor: Cell::new(None),
        };
        fixture.layout(cols, rows);
        fixture
    }

    fn layout(&mut self, cols: u16, rows: u16) {
        self.root.set_frame(screen_points(cols, rows));
    }

    fn draw(&mut self, cols: u16, rows: u16) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
        self.root.render(
            &mut buf,
            &DrawCtx {
                env: &self.env,
                theme: &self.theme,
                focused: None,
                cursor: &self.cursor,
            },
        );
        buffer_string(&buf)
    }

    fn focus_chain(&self) -> Vec<u32> {
        let mut chain = Vec::new();
        self.root.collect_focus(&mut chain);
        chain
    }

    fn press(&mut self, id: u32, code: KeyCode) -> bool {
        self.root.handle_key(
            id,
            &KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press),
        )
    }

    fn take_dirty(&self) -> bool {
        self.renderer.dirty().replace(false)
    }
}

#[test]
fn renders_text() {
    let mut fixture = Fixture::new(text("hello waterui"), 40, 5);
    let out = fixture.draw(40, 5);
    assert!(out.starts_with("hello waterui"), "got:\n{out}");
}

#[test]
fn vstack_places_children_vertically() {
    let view = vstack((text("first"), text("second"), text("third"))).spacing(0.0);
    let mut fixture = Fixture::new(view, 40, 5);
    let out = fixture.draw(40, 5);
    let lines: Vec<&str> = out.lines().collect();
    // vstack centers children horizontally and stacks them on successive rows.
    assert!(lines[0].contains("first"), "got:\n{out}");
    assert!(lines[1].contains("second"), "got:\n{out}");
    assert!(lines[2].contains("third"), "got:\n{out}");
}

#[test]
fn button_renders_chrome_and_activates() {
    let counter: Binding<i32> = binding(0);
    let counter2 = counter.clone();
    let view = vstack((
        text(counter.clone().map(|v| format!("count={v}")).computed()),
        button("Bump").action(move || counter2.set(counter2.get() + 1)),
    ))
    .spacing(0.0);
    let mut fixture = Fixture::new(view, 40, 5);

    let out = fixture.draw(40, 5);
    assert!(out.contains("count=0"), "got:\n{out}");
    assert!(out.contains("[ Bump ]"), "got:\n{out}");

    let chain = fixture.focus_chain();
    assert_eq!(chain.len(), 1);
    assert!(fixture.press(chain[0], KeyCode::Enter));
    assert_eq!(counter.get(), 1);
    assert!(fixture.take_dirty(), "action should mark the tree dirty");

    let out = fixture.draw(40, 5);
    assert!(out.contains("count=1"), "got:\n{out}");
}

#[test]
fn toggle_flips_on_space() {
    let value = binding(false);
    let view = toggle("Enable", &value);
    let mut fixture = Fixture::new(view, 40, 5);

    let out = fixture.draw(40, 5);
    assert!(out.contains("[ ] Enable"), "got:\n{out}");

    let chain = fixture.focus_chain();
    assert_eq!(chain.len(), 1);
    assert!(fixture.press(chain[0], KeyCode::Char(' ')));
    assert!(value.get());

    let out = fixture.draw(40, 5);
    assert!(out.contains("[x] Enable"), "got:\n{out}");
}

#[test]
fn spacer_pushes_apart() {
    use waterui_layout::spacer::spacer;
    use waterui_layout::stack::hstack;
    let view = hstack((text("left"), spacer(), text("right"))).spacing(0.0);
    let mut fixture = Fixture::new(view, 40, 3);
    let out = fixture.draw(40, 3);
    // hstack centers children vertically; the texts land on the middle row,
    // pushed to opposite edges by the spacer.
    let middle = out.lines().nth(1).unwrap();
    assert!(middle.starts_with("left"), "got:\n{out}");
    assert!(middle.trim_end().ends_with("right"), "got:\n{out}");
}

#[test]
fn divider_draws_rule() {
    use waterui_layout::divider::Divider;
    let view = vstack((text("a"), Divider, text("b"))).spacing(0.0);
    let mut fixture = Fixture::new(view, 40, 5);
    let out = fixture.draw(40, 5);
    assert!(out.lines().nth(1).unwrap().contains('─'), "got:\n{out}");
}

#[test]
fn styled_str_maps_bold_and_color() {
    use ratatui::style::Modifier;
    let styled = StyledStr::plain("bold").bold();
    let mut fixture = Fixture::new(text(styled), 20, 3);
    fixture.draw(20, 3);

    // Render once more with a buffer we can inspect.
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 3));
    fixture.root.render(
        &mut buf,
        &DrawCtx {
            env: &fixture.env,
            theme: &fixture.theme,
            focused: None,
            cursor: &fixture.cursor,
        },
    );
    let cell = buf.cell((0, 0)).unwrap();
    assert_eq!(cell.symbol(), "b");
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn str_view_dispatches() {
    // A bare &str is a View producing a `Str` raw view.
    let mut fixture = Fixture::new("plain string", 20, 3);
    let out = fixture.draw(20, 3);
    assert!(out.starts_with("plain string"), "got:\n{out}");
}

#[test]
fn metadata_environment_passthrough() {
    use waterui_core::Metadata;
    // `Metadata<Environment>` redispatches its content under the overlay env.
    let view = Metadata::new(text("nested"), Environment::new());
    let mut fixture = Fixture::new(view, 20, 3);
    let out = fixture.draw(20, 3);
    assert!(out.starts_with("nested"), "got:\n{out}");
}

#[test]
fn text_field_edits_binding() {
    use waterui_controls::field;
    let value = binding(Str::from_static(""));
    let view = field("Name", &value);
    let mut fixture = Fixture::new(view, 40, 5);

    let chain = fixture.focus_chain();
    assert_eq!(chain.len(), 1);
    for c in ['h', 'i'] {
        assert!(fixture.press(chain[0], KeyCode::Char(c)));
    }
    assert_eq!(&*value.get(), "hi");

    let out = fixture.draw(40, 5);
    assert!(out.contains("Name: hi"), "got:\n{out}");

    assert!(fixture.press(chain[0], KeyCode::Backspace));
    assert_eq!(&*value.get(), "h");
}

#[test]
fn hit_testing_finds_focusable() {
    let counter: Binding<i32> = binding(0);
    let counter2 = counter.clone();
    let view = vstack((
        text("count"),
        button("Bump").action(move || counter2.set(counter2.get() + 1)),
    ))
    .spacing(0.0);
    let mut fixture = Fixture::new(view, 40, 5);
    let chain = fixture.focus_chain();
    // A click inside the button's frame hits it; one above it does not.
    let button_frame = fixture.root.children[1].frame.get();
    assert_eq!(
        fixture.root.hit(button_frame.x + 1, button_frame.y),
        Some(chain[0])
    );
    assert!(
        fixture
            .root
            .hit(0, button_frame.y + button_frame.height)
            .is_none()
    );
    assert!(fixture.root.activate(chain[0]));
    assert_eq!(counter.get(), 1);
}
