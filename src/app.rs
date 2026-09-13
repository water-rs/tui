//! The terminal event loop.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io::{self, Stdout, stdout};
use std::panic;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use executor_core::LocalExecutor;
use executor_core::async_task::{self, AsyncTask, Runnable};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use waterui_core::{Environment, View};
use waterui_internal::app::App;

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
/// The view is a closure because the executors are installed first: view
/// composition may already spawn reactive work, and `spawn_local` panics
/// without a local executor on the thread.
///
/// # Errors
///
/// Returns terminal I/O errors from `crossterm`/`ratatui`.
pub fn run<V: View>(view: impl FnOnce() -> V) -> io::Result<()> {
    init_executors();
    let mut env = Environment::new();
    install_terminal_theme(&mut env);
    run_inner(view(), env)
}

/// Runs a WaterUI [`App`] as a full-screen terminal application.
///
/// This is the entry point generated launcher crates use: the application's own
/// environment is the composition root, the terminal theme is installed into
/// it, and the main window's content becomes the screen. A terminal has a
/// single surface, so declaring windows beyond the main window is a programmer
/// error and panics.
///
/// The app is a closure for the same reason as [`run`]: composing the app —
/// including `configure_environment!` — may spawn work, so the executors must
/// already be installed when it runs.
///
/// # Errors
///
/// Returns terminal I/O errors from `crossterm`/`ratatui`.
pub fn run_app(app: impl FnOnce() -> App) -> io::Result<()> {
    init_executors();
    let (mut windows, _menu_bar, mut env) = app().into_parts();
    install_terminal_theme(&mut env);
    assert!(
        windows.len() == 1,
        "the TUI backend renders a single window; this app declares {} — \
         use a desktop backend for multi-window applications",
        windows.len()
    );
    let window = windows.pop().expect("the main window exists");
    run_inner(window.build_content(), env)
}

/// Installs the global and thread-local executors before any app code runs.
///
/// `spawn` work goes to the platform's native executor; `spawn_local` work is
/// parked and drained between frames by the event loop so it never re-enters
/// the code that spawned it.
fn init_executors() {
    let _ = executor_core::try_init_global_executor(native_executor::NativeExecutor::new());
    let _ = executor_core::try_init_local_executor(
        waterui_internal::task::monitored_local_executor(TuiLocalExecutor),
    );
}

fn run_inner(view: impl View, env: Environment) -> io::Result<()> {

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
        if drain_parked_local_work() {
            dirty.set(true);
        }
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
            if drain_parked_local_work() {
                dirty.set(true);
                continue;
            }
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

thread_local! {
    /// `spawn_local` work parked between frames. Runnables are deliberately not
    /// run inline: reactive work re-enters the code under render, which
    /// deadlocks when polled in the middle of the call that spawned it.
    static PARKED_RUNNABLES: RefCell<Vec<Runnable>> = const { RefCell::new(Vec::new()) };
}

/// Queues `spawn_local` work for the terminal event loop to drain.
#[derive(Clone, Copy, Debug, Default)]
struct TuiLocalExecutor;

impl LocalExecutor for TuiLocalExecutor {
    type Task<T: 'static> = AsyncTask<T>;

    fn spawn_local<Fut>(&self, fut: Fut) -> Self::Task<Fut::Output>
    where
        Fut: Future + 'static,
    {
        let (runnable, task) = async_task::spawn_local(fut, |runnable: Runnable| {
            PARKED_RUNNABLES.with(|parked| parked.borrow_mut().push(runnable));
        });
        runnable.schedule();
        task
    }
}

/// Runs the work `spawn_local` parked since the last drain, returning whether
/// any ran. Each drained runnable may park more work; this drains only what was
/// already queued, so a task that reschedules itself is polled next frame.
fn drain_parked_local_work() -> bool {
    let ready = PARKED_RUNNABLES.with(|parked| core::mem::take(&mut *parked.borrow_mut()));
    let ran = !ready.is_empty();
    for runnable in ready {
        runnable.run();
    }
    ran
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
