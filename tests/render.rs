//! Headless render tests: dispatch a view, place it on a fixed cell grid, and
//! assert on the resulting buffer contents and interaction behavior.

use std::cell::{Cell, RefCell};

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
use waterui_tui::{Node, ScrollOp, TuiRenderer, install_terminal_theme};

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
    scroll_ops: RefCell<Vec<ScrollOp>>,
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
            scroll_ops: RefCell::new(Vec::new()),
        };
        fixture.layout(cols, rows);
        fixture
    }

    fn layout(&mut self, cols: u16, rows: u16) {
        self.root.set_frame(screen_points(cols, rows));
    }

    fn render_buf(&mut self, cols: u16, rows: u16) -> Buffer {
        // The app lays out every frame; mirror that so reactive frame changes
        // (tab switches, scroll extents) are reflected in tests.
        self.layout(cols, rows);
        let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
        self.scroll_ops.borrow_mut().clear();
        self.root.render(
            &mut buf,
            &DrawCtx {
                env: &self.env,
                theme: &self.theme,
                focused: None,
                cursor: &self.cursor,
                picker: None,
                tick: 0,
                scroll_ops: &self.scroll_ops,
            },
        );
        buf
    }

    fn draw(&mut self, cols: u16, rows: u16) -> String {
        buffer_string(&self.render_buf(cols, rows))
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
            picker: None,
            tick: 0,
            scroll_ops: &fixture.scroll_ops,
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
    // `mouse` performs hit-testing + activation in one step.
    assert_eq!(
        fixture
            .root
            .mouse(button_frame.x + 1, button_frame.y, false),
        Some(chain[0])
    );
    assert_eq!(counter.get(), 1);
}

#[test]
fn slider_arrows_and_track_click() {
    use waterui_controls::slider::slider;
    let value = binding(0.5f64);
    let view = slider("Vol", &value);
    let mut fixture = Fixture::new(view, 40, 3);

    let out = fixture.draw(40, 3);
    assert!(out.contains('─') && out.contains('●'), "got:\n{out}");

    let chain = fixture.focus_chain();
    assert!(fixture.press(chain[0], KeyCode::Right));
    assert!((value.get() - 0.55).abs() < 1e-9, "got {}", value.get());
    assert!(fixture.press(chain[0], KeyCode::Home));
    assert_eq!(value.get(), 0.0);
    assert!(fixture.press(chain[0], KeyCode::End));
    assert_eq!(value.get(), 1.0);

    // Click at the far right of the track sets ~1.0.
    let frame = fixture.root.frame.get();
    fixture
        .root
        .mouse(frame.x + frame.width - 1, frame.y, false);
    assert!(value.get() > 0.9, "got {}", value.get());
}

#[test]
fn stepper_increments_with_keys_and_clicks() {
    use waterui_controls::stepper::stepper;
    let value = binding(0i32);
    let view = stepper("Qty", &value).range(0..=9);
    let mut fixture = Fixture::new(view, 30, 3);

    let out = fixture.draw(30, 3);
    assert!(out.contains("[-]") && out.contains("[+]"), "got:\n{out}");

    let chain = fixture.focus_chain();
    assert!(fixture.press(chain[0], KeyCode::Right));
    assert_eq!(value.get(), 1);
    // Click on the `[+]` region (rightmost 3 cells of the row).
    let frame = fixture.root.frame.get();
    fixture
        .root
        .mouse(frame.x + frame.width - 1, frame.y, false);
    assert_eq!(value.get(), 2);
    fixture
        .root
        .mouse(frame.x + frame.width - 9, frame.y, false);
    assert_eq!(value.get(), 1, "click on [-] should decrement");
}

#[test]
fn progress_renders_linear_circular_and_loading() {
    use waterui_internal::component::progress::{loading, progress};

    let mut fixture = Fixture::new(progress(0.5), 30, 2);
    let out = fixture.draw(30, 2);
    assert!(out.contains('█') && out.contains('░'), "got:\n{out}");

    let mut fixture = Fixture::new(progress(1.0).circular(), 10, 2);
    let out = fixture.draw(10, 2);
    assert!(out.contains('●'), "got:\n{out}");

    let mut fixture = Fixture::new(loading(), 10, 2);
    let buf = fixture.render_buf(10, 2);
    let first = buf.cell((0, 0)).unwrap().symbol().chars().next().unwrap();
    assert!(
        ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'].contains(&first),
        "expected spinner glyph, got {first:?}"
    );
    assert!(fixture.renderer.animated(), "loading should flag animation");
}

#[test]
fn secure_field_masks_and_edits() {
    use waterui_form::secure::{Secure, secure};
    let value = binding(Secure::new(String::new()));
    let view = secure("Pass", &value);
    let mut fixture = Fixture::new(view, 40, 3);

    let chain = fixture.focus_chain();
    for c in ['h', 'i'] {
        assert!(fixture.press(chain[0], KeyCode::Char(c)));
    }
    assert_eq!(value.get().expose(), "hi");

    let out = fixture.draw(40, 3);
    assert!(out.contains("Pass: ••"), "got:\n{out}");
    assert!(!out.contains("hi"), "secret must not render:\n{out}");
}

#[test]
fn scroll_view_clips_and_wheel_scrolls() {
    use waterui_layout::scroll::scroll;
    let rows: Vec<_> = (0..10)
        .map(|i| text(Str::from(format!("row{i}"))))
        .collect();
    let view = scroll(vstack(rows).spacing(0.0));
    let mut fixture = Fixture::new(view, 20, 4);

    let out = fixture.draw(20, 4);
    assert!(out.contains("row0") && out.contains("row3"), "got:\n{out}");
    assert!(!out.contains("row9"), "row9 should be clipped:\n{out}");
    // Scrollbar on the right edge.
    let line: String = (0..4)
        .map(|y| {
            fixture
                .render_buf(20, 4)
                .cell((19, y))
                .unwrap()
                .symbol()
                .to_string()
        })
        .collect();
    assert!(
        line.contains('┃') || line.contains('│'),
        "scrollbar: {line}"
    );

    // Wheel down shifts content up.
    assert!(fixture.root.scroll_at(5, 1, 0, 2));
    let out = fixture.draw(20, 4);
    assert!(out.contains("row2"), "after scroll:\n{out}");
    assert!(!out.contains("row0"), "row0 should be scrolled out:\n{out}");

    // The render reports the offset delta as a ScrollOp so the presentation
    // step can replay it as a hardware scroll-region command.
    let ops = fixture.scroll_ops.borrow();
    assert_eq!(ops.len(), 1, "scroll ops: {ops:?}");
    assert_eq!(ops[0].region, Rect::new(0, 0, 20, 4));
    assert_eq!(ops[0].delta, (0, 2));
}

#[test]
fn tabs_render_labels_and_switch() {
    use waterui_navigation::{NavigationView, Tab, Tabs};
    let selection = binding(0i32);
    let view = Tabs::new(
        &selection,
        vec![
            Tab::new(0, "One", || NavigationView::new("", text("first-page"))),
            Tab::new(1, "Two", || NavigationView::new("", text("second-page"))),
        ],
    );
    let mut fixture = Fixture::new(view, 40, 6);

    let out = fixture.draw(40, 6);
    assert!(out.contains("One") && out.contains("Two"), "got:\n{out}");
    assert!(out.contains("first-page"), "got:\n{out}");
    assert!(!out.contains("second-page"), "got:\n{out}");

    let chain = fixture.focus_chain();
    assert!(fixture.press(chain[0], KeyCode::Right));
    assert_eq!(selection.get(), 1);
    let out = fixture.draw(40, 6);
    assert!(out.contains("second-page"), "got:\n{out}");
    assert!(!out.contains("first-page"), "got:\n{out}");
}

#[test]
fn offset_metadata_shifts_rendering() {
    use waterui_core::Metadata;
    use waterui_internal::style::Offset;
    let view = Metadata::new(text("moved"), Offset::new(3.0, 8.0));
    let mut fixture = Fixture::new(view, 20, 4);
    let out = fixture.draw(20, 4);
    let line = out.lines().nth(1).unwrap();
    assert!(line.starts_with("   moved"), "got:\n{out}");
}

#[test]
fn focused_binding_requests_focus() {
    use waterui_controls::field;
    use waterui_core::Metadata;
    use waterui_internal::component::focus::Focused;
    let text_value = binding(Str::from_static(""));
    let focused = binding(false);
    let view = Metadata::new(field("Name", &text_value), Focused(focused.clone()));
    let mut fixture = Fixture::new(view, 40, 3);

    focused.set(true);
    let requests = fixture.renderer.take_focus_requests();
    let chain = fixture.focus_chain();
    assert_eq!(requests, vec![chain[0]], "should request the field's id");
}

fn srgb(r: u8, g: u8, b: u8) -> waterui_graphics::color::ResolvedColor {
    waterui_graphics::color::ResolvedColor::from_srgb(waterui_graphics::color::Srgb::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    ))
}

#[test]
fn linear_gradient_interpolates_per_cell() {
    use waterui_graphics::gradient_renderer::Gradient;
    let view = Gradient::linear(
        vec![(0.0, srgb(255, 0, 0)), (1.0, srgb(0, 0, 255))],
        [0.0, 0.5],
        [1.0, 0.5],
    );
    let mut fixture = Fixture::new(view, 4, 2);
    let buf = fixture.render_buf(4, 2);

    // Half-block rendering: fg is the top-half sample, bg the bottom half;
    // a horizontal gradient therefore lands mostly on `fg`.
    let ratatui::style::Color::Rgb(left_r, _, left_b) = buf.cell((0, 0)).unwrap().fg else {
        panic!("expected truecolor left edge");
    };
    let ratatui::style::Color::Rgb(right_r, _, right_b) = buf.cell((3, 0)).unwrap().fg else {
        panic!("expected truecolor right edge");
    };
    assert!(
        left_r > left_b,
        "left edge should be red-dominant: {left_r}/{left_b}"
    );
    assert!(
        right_b > right_r,
        "right edge should be blue-dominant: {right_r}/{right_b}"
    );
}

#[test]
fn gradient_sample_respects_geometry() {
    use waterui_graphics::gradient_renderer::{Gradient, ResolvedGradient};
    use waterui_tui::gradient::sample;

    // Vertical linear: top is red, bottom is blue.
    let linear = ResolvedGradient::linear(
        vec![
            waterui_graphics::gradient_renderer::ResolvedGradientStop::new(0.0, srgb(255, 0, 0)),
            waterui_graphics::gradient_renderer::ResolvedGradientStop::new(1.0, srgb(0, 0, 255)),
        ],
        [0.5, 0.0],
        [0.5, 1.0],
    );
    let top = sample(&linear, 0.5, 0.01);
    let bottom = sample(&linear, 0.5, 0.99);
    assert!(top.red > top.blue, "top should be red: {top:?}");
    assert!(
        bottom.blue > bottom.red,
        "bottom should be blue: {bottom:?}"
    );

    // Radial: center is red, outside is blue.
    let radial = ResolvedGradient::radial(
        vec![
            waterui_graphics::gradient_renderer::ResolvedGradientStop::new(0.0, srgb(255, 0, 0)),
            waterui_graphics::gradient_renderer::ResolvedGradientStop::new(1.0, srgb(0, 0, 255)),
        ],
        [0.5, 0.5],
        0.0,
        0.5,
    );
    let center = sample(&radial, 0.5, 0.5);
    let corner = sample(&radial, 0.0, 0.0);
    assert!(center.red > center.blue, "center should be red: {center:?}");
    assert!(
        corner.blue > corner.red,
        "corner should be blue: {corner:?}"
    );

    // A mesh gradient never reaches `ResolvedGradient`; constructing one is
    // fine but it dispatches through the GPU path instead.
    let _mesh = Gradient::mesh(
        2,
        2,
        vec![
            ([0.0, 0.0], srgb(255, 0, 0)),
            ([1.0, 0.0], srgb(0, 255, 0)),
            ([0.0, 1.0], srgb(0, 0, 255)),
            ([1.0, 1.0], srgb(255, 255, 0)),
        ],
        true,
    );
}

/// A `GpuView` that clears the surface to solid red — exercises the same
/// `Native<GpuSurface>` path `waterui_image::Image` and mesh gradients take.
struct ClearView;

impl waterui_graphics::GpuView for ClearView {
    async fn setup(&mut self, _ctx: &waterui_graphics::GpuContext<'_>, _env: &mut Environment) {}

    fn render(&mut self, frame: &mut waterui_graphics::GpuFrame) {
        use waterui_graphics::wgpu;
        let mut encoder = frame
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::RED),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
        }
        frame.queue.submit([encoder.finish()]);
    }
}

#[test]
fn gpu_surface_rasterizes_into_half_blocks() {
    let view = waterui_graphics::GpuSurface::new(ClearView);
    let mut fixture = Fixture::new(view, 4, 2);
    let buf = fixture.render_buf(4, 2);

    if !fixture.renderer.gpu_available() {
        assert!(
            buffer_string(&buf).contains("[gpu]"),
            "expected placeholder without a GPU"
        );
        return;
    }
    let cell = buf.cell((0, 0)).unwrap();
    assert_eq!(cell.symbol(), "▀", "expected half-block image cell");
    let ratatui::style::Color::Rgb(r, g, b) = cell.fg else {
        panic!("expected truecolor pixel");
    };
    assert!(
        r > 200 && g < 60 && b < 60,
        "expected red pixel: {r}/{g}/{b}"
    );
}

#[test]
fn scroll_shifts_multiline_text_content() {
    use waterui_layout::scroll::scroll;
    // A single Text node taller than the viewport: its lines must slide past
    // the clip, not re-anchor at the visible top.
    let content: String = (0..30).map(|i| format!("line{i}\n")).collect();
    let view = scroll(text(Str::from(content)));
    let mut fixture = Fixture::new(view, 20, 5);

    let out = fixture.draw(20, 5);
    assert!(out.contains("line0"), "got:\n{out}");
    assert!(!out.contains("line29"), "got:\n{out}");

    assert!(fixture.root.scroll_at(5, 2, 0, 20));
    let out = fixture.draw(20, 5);
    assert!(out.contains("line20"), "after scroll:\n{out}");
    assert!(
        !out.contains("line0\n") && !out.lines().next().unwrap().starts_with("line0"),
        "line0 should be scrolled out:\n{out}"
    );
}

#[test]
fn scroll_controller_pins_to_bottom() {
    use waterui_layout::scroll::{ScrollController, scroll};
    let scroller = ScrollController::<waterui_core::layout::Point>::default();
    let rows: Vec<_> = (0..30)
        .map(|i| text(Str::from(format!("row{i}"))))
        .collect();
    let view = scroll(vstack(rows).spacing(0.0)).scroll_controller(&scroller);
    let mut fixture = Fixture::new(view, 20, 5);

    let out = fixture.draw(20, 5);
    assert!(out.contains("row0"), "got:\n{out}");

    scroller.scroll_to(waterui_core::layout::Point::new(0.0, f32::MAX));
    let out = fixture.draw(20, 5);
    assert!(
        out.contains("row29"),
        "pinned bottom should show row29:\n{out}"
    );
}
