//! The terminal event loop.

use std::cell::Cell;
use std::io::{self, Stdout, stdout};
use std::panic;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use waterui_core::{Environment, View};

use crate::node::{DrawCtx, screen_points};
use crate::renderer::TuiRenderer;
use crate::style::Theme;
use crate::theme::install_terminal_theme;

/// Runs a WaterUI view as a full-screen terminal application.
///
/// Installs the terminal theme into a fresh [`Environment`], enters raw mode
/// on the alternate screen, and loops until `Esc` or `Ctrl-C`. The terminal
/// state is restored on exit and on panic.
///
/// # Errors
///
/// Returns terminal I/O errors from `crossterm`/`ratatui`.
pub fn run(view: impl View) -> io::Result<()> {
    let mut env = Environment::new();
    install_terminal_theme(&mut env);

    let mut renderer = TuiRenderer::new();
    let dirty = renderer.dirty();
    let mut root = renderer.dispatch(view, &env);

    let mut guard = TerminalGuard::enter()?;

    // Probe terminal graphics support once, before the event loop reads
    // stdin. Terminals without a graphics protocol fall back to half-blocks.
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .map_err(|error| tracing::warn!("terminal graphics probe failed: {error}"))
        .ok();

    let mut focus_chain = Vec::new();
    root.collect_focus(&mut focus_chain);
    let mut focused = focus_chain.first().copied();
    root.sync_focused(focused);
    let cursor = Cell::new(None);

    'app: loop {
        for id in renderer.take_focus_requests() {
            focused = Some(id);
        }
        root.sync_focused(focused);

        let size = guard.terminal.size()?;
        root.set_frame(screen_points(size.width, size.height));
        let theme = Theme::resolve(&env);
        let tick = renderer.tick();
        guard.terminal.draw(|frame| {
            cursor.set(None);
            root.render(
                frame.buffer_mut(),
                &DrawCtx {
                    env: &env,
                    theme: &theme,
                    focused,
                    cursor: &cursor,
                    picker: picker.as_ref(),
                    tick,
                },
            );
            if let Some(position) = cursor.get() {
                frame.set_cursor_position(position);
            }
        })?;

        for (hook, hook_env) in renderer.take_appear_hooks() {
            hook.handle(&hook_env);
        }

        dirty.set(false);
        while !dirty.get() {
            // Spinners advance on a timer; without them the loop only wakes
            // for input or a signal-driven dirty flag.
            let frame_ms = if renderer.animated() { 80 } else { 250 };
            if !event::poll(Duration::from_millis(frame_ms))? {
                if renderer.animated() {
                    renderer.bump_tick();
                    dirty.set(true);
                }
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => break 'app,
                    KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        break 'app;
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        focus_chain.clear();
                        root.collect_focus(&mut focus_chain);
                        if !focus_chain.is_empty() {
                            let next = focused
                                .and_then(|id| focus_chain.iter().position(|&f| f == id))
                                .map_or(0, |index| {
                                    if key.code == KeyCode::Tab {
                                        (index + 1) % focus_chain.len()
                                    } else {
                                        (index + focus_chain.len() - 1) % focus_chain.len()
                                    }
                                });
                            focused = Some(focus_chain[next]);
                            root.sync_focused(focused);
                        }
                        dirty.set(true);
                    }
                    _ => {
                        if let Some(id) = focused
                            && root.handle_key(id, &key)
                        {
                            dirty.set(true);
                        }
                    }
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(id) = root.mouse(mouse.column, mouse.row, false) {
                            focused = Some(id);
                            root.sync_focused(focused);
                        }
                        dirty.set(true);
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(id) = root.mouse(mouse.column, mouse.row, true) {
                            focused = Some(id);
                            root.sync_focused(focused);
                        }
                        dirty.set(true);
                    }
                    MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollRight
                    | MouseEventKind::ScrollLeft => {
                        let (dx, dy) = match mouse.kind {
                            MouseEventKind::ScrollDown => (0, 1),
                            MouseEventKind::ScrollUp => (0, -1),
                            MouseEventKind::ScrollRight => (1, 0),
                            _ => (-1, 0),
                        };
                        if root.scroll_at(mouse.column, mouse.row, dx, dy) {
                            dirty.set(true);
                        }
                    }
                    _ => {}
                },
                Event::Resize(..) => dirty.set(true),
                _ => {}
            }
        }
    }

    Ok(())
}

/// Restores the terminal when dropped, including through unwinding.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
        let terminal = Terminal::new(CrosstermBackend::new(out))?;

        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            previous(info);
        }));

        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}
