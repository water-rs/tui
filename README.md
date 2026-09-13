# waterui-tui

Experimental terminal (TUI) backend for [WaterUI](https://github.com/water-rs/waterui).

Maps WaterUI's declarative view tree onto a terminal cell grid: views dispatch
into a retained node tree, measure and place with the shared `waterui-layout`
algorithms in logical points, then quantize to whole cells (8pt = 1 row, 1pt =
1 column) and draw with [ratatui](https://ratatui.rs). Input comes from
[crossterm](https://github.com/crossterm-rs/crossterm).

## Status

Experiment — not integrated into the WaterUI CLI, not published.

Supported: `Text`/`StyledStr` (styled spans, alignment, `line_limit`), all
`Layout`-driven containers (`vstack`, `hstack`, `padding`, `frame`, `spacer`,
scroll-less `LazyContainer`), `Button`, `Toggle`, `TextField`, `Divider`,
`Color` fills, `Metadata<Environment>`/`LayoutPriority`/`Retain`/
`LifeCycleHook(Appear)`, `Dynamic` subtree rebuilds, keyboard focus
(`Tab`/`Shift-Tab`, `Enter`/`Space`), mouse click, terminal resize, theme
color tokens.

Not supported (by design): GPU surfaces, video, maps, web views, images,
canvas — a terminal has no such primitives. `SystemIcon` renders as a
`[name]` placeholder since no OS symbol catalog exists on a terminal.

## Try it

```sh
cargo run --example demo
```

`Tab`/`Shift-Tab` move focus, `Enter`/`Space` activate, mouse clicks focus and
activate, `Esc`/`Ctrl-C` quit.

## Using it

```rust
use waterui_controls::button;
use waterui_layout::stack::vstack;
use waterui_text::text::text;

fn main() -> std::io::Result<()> {
    waterui_tui::run(vstack((
        text("Hello"),
        button("Quit").action(|| std::process::exit(0)),
    )))
}
```

For embedding, drive `TuiRenderer` + `Node` directly: `dispatch` a view into a
root `Node`, call `Node::set_frame` per resize, `Node::render` into a ratatui
`Buffer`, forward key/mouse events to `Node::handle_key`/`Node::hit`, and
watch `TuiRenderer::dirty()` for redraw requests.

## Layout contract

| points | cells |
| ------ | ----- |
| 1pt    | 1 column |
| 8pt    | 1 row |

A line of text measures exactly one row; the default 10pt stack spacing snaps
to one empty row. Frames quantize per-node at placement: column edges round to
the nearest cell, row tops floor, heights round (minimum one row).
