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

    let mut focus_chain = Vec::new();
    root.collect_focus(&mut focus_chain);
    let mut focused = focus_chain.first().copied();
    let cursor = Cell::new(None);

    'app: loop {
        let size = guard.terminal.size()?;
        root.set_frame(screen_points(size.width, size.height));
        let theme = Theme::resolve(&env);
        guard.terminal.draw(|frame| {
            cursor.set(None);
            root.render(
                frame.buffer_mut(),
                &DrawCtx {
                    env: &env,
                    theme: &theme,
                    focused,
                    cursor: &cursor,
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
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => break 'app,
                    KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        break 'app;
                    }
                    KeyCode::Tab => {
                        focus_chain.clear();
                        root.collect_focus(&mut focus_chain);
                        if !focus_chain.is_empty() {
                            let next = focused
                                .and_then(|id| focus_chain.iter().position(|&f| f == id))
                                .map_or(0, |index| (index + 1) % focus_chain.len());
                            focused = Some(focus_chain[next]);
                        }
                        dirty.set(true);
                    }
                    KeyCode::BackTab => {
                        focus_chain.clear();
                        root.collect_focus(&mut focus_chain);
                        if !focus_chain.is_empty() {
                            let prev = focused
                                .and_then(|id| focus_chain.iter().position(|&f| f == id))
                                .map_or(0, |index| {
                                    (index + focus_chain.len() - 1) % focus_chain.len()
                                });
                            focused = Some(focus_chain[prev]);
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
                Event::Mouse(mouse) => {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
                        && let Some(id) = root.hit(mouse.column, mouse.row)
                    {
                        focused = Some(id);
                        root.activate(id);
                        dirty.set(true);
                    }
                }
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
