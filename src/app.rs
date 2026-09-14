//! The terminal event loop.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io::{self, Stdout, stdout};
use std::mem;
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use executor_core::LocalExecutor;
use executor_core::async_task::{self, AsyncTask, Runnable};
use ratatui::backend::{Backend, ClearType, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use waterui_core::{Environment, View};
use waterui_internal::app::App;

use crate::node::{DrawCtx, Node, screen_points};
use crate::present::present;
use crate::renderer::TuiRenderer;
use crate::scroll::ScrollOp;
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
    let wake = init_executors();
    let mut env = Environment::new();
    install_terminal_theme(&mut env);
    run_inner(view(), env, wake)
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
    let wake = init_executors();
    let (mut windows, _menu_bar, mut env) = app().into_parts();
    install_terminal_theme(&mut env);
    assert!(
        windows.len() == 1,
        "the TUI backend renders a single window; this app declares {} — \
         use a desktop backend for multi-window applications",
        windows.len()
    );
    let window = windows.pop().expect("the main window exists");
    run_inner(window.build_content(), env, wake)
}

/// A wake-up for the event loop. Input arrives from a dedicated reader thread
/// and `spawn_local` runnables arrive from whatever thread scheduled them, so
/// the loop can block on a single channel instead of polling: streamed state
/// updates repaint immediately, and bursts of queued events coalesce into one
/// frame rather than one frame per event.
enum Wake {
    /// A runnable parked by `spawn_local`.
    Task(Runnable),
    /// A terminal event read off stdin by the reader thread.
    Input(Event),
    /// The reader thread's `event::read` failed — the terminal is gone.
    InputEof,
}

/// The sender stays with the reader thread; the receiver drives the loop.
struct WakeBus {
    tx: mpsc::Sender<Wake>,
    rx: mpsc::Receiver<Wake>,
}

/// Installs the global and thread-local executors before any app code runs,
/// returning the channel the event loop waits on.
///
/// `spawn` work goes to the platform's native executor; `spawn_local` work is
/// posted to the wake channel and run between frames by the event loop so it
/// never re-enters the code that spawned it.
fn init_executors() -> WakeBus {
    let (tx, rx) = mpsc::channel();
    let _ = executor_core::try_init_global_executor(native_executor::NativeExecutor::new());
    let _ = executor_core::try_init_local_executor(
        waterui_internal::task::monitored_local_executor(TuiLocalExecutor {
            wake_tx: tx.clone(),
        }),
    );
    WakeBus { tx, rx }
}

/// Reads crossterm events off stdin and posts them to the wake channel until
/// `alive` drops or the channel closes. Only this thread may touch
/// `event::read`/`poll` — crossterm keeps a single global reader.
fn spawn_event_reader(tx: mpsc::Sender<Wake>, alive: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if tx.send(Wake::Input(event)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Wake::InputEof);
                        return;
                    }
                },
                Ok(false) => {}
                Err(_) => {
                    let _ = tx.send(Wake::InputEof);
                    return;
                }
            }
        }
    })
}

fn run_inner(view: impl View, env: Environment, wake: WakeBus) -> io::Result<()> {
    let mut renderer = TuiRenderer::new();
    let dirty = renderer.dirty();
    let mut root = renderer.dispatch(view, &env);

    let mut guard = TerminalGuard::enter()?;

    // Probe terminal graphics support once, before the event loop reads
    // stdin. Terminals without a graphics protocol fall back to half-blocks.
    let picker = crate::probe::picker()
        .map_err(|error| tracing::warn!("terminal graphics probe failed: {error}"))
        .ok();

    let alive = Arc::new(AtomicBool::new(true));
    let reader = spawn_event_reader(wake.tx, alive.clone());
    let wake_rx = wake.rx;

    let mut focus_chain = Vec::new();
    root.collect_focus(&mut focus_chain);
    let mut focused = focus_chain.first().copied();
    root.sync_focused(focused);
    let cursor = Cell::new(None);
    // Scroll deltas discovered during render; `TerminalGuard::draw_frame`
    // clears and refills it each presented frame.
    let scroll_ops = RefCell::new(Vec::new());

    'app: loop {
        if dirty.get() {
            for id in renderer.take_focus_requests() {
                focused = Some(id);
            }
            root.sync_focused(focused);

            let size = guard.size()?;
            root.set_frame(screen_points(size.width, size.height));
            let theme = Theme::resolve(&env);
            let tick = renderer.tick();
            guard.draw_frame(&scroll_ops, |buf| {
                cursor.set(None);
                root.render(
                    buf,
                    &DrawCtx {
                        env: &env,
                        theme: &theme,
                        focused,
                        cursor: &cursor,
                        picker: picker.as_ref(),
                        tick,
                        scroll_ops: &scroll_ops,
                    },
                );
                cursor.get()
            })?;

            for (hook, hook_env) in renderer.take_appear_hooks() {
                hook.handle(&hook_env);
            }
            dirty.set(false);
        }

        // Wait for the first wake; while animating the same wait doubles as
        // the frame timer.
        let first = if renderer.animated() {
            match wake_rx.recv_timeout(Duration::from_millis(80)) {
                Ok(wake) => Some(wake),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    renderer.bump_tick();
                    dirty.set(true);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break 'app,
            }
        } else {
            match wake_rx.recv() {
                Ok(wake) => Some(wake),
                Err(_) => break 'app,
            }
        };
        if let Some(wake) = first
            && process(wake, &mut root, &mut focus_chain, &mut focused, &dirty)
        {
            break 'app;
        }
        // Coalesce everything already queued into this same frame — a wheel
        // burst or a chunk flood becomes one redraw, not one per event.
        while let Ok(wake) = wake_rx.try_recv() {
            if process(wake, &mut root, &mut focus_chain, &mut focused, &dirty) {
                break 'app;
            }
        }
    }

    alive.store(false, Ordering::Relaxed);
    let _ = reader.join();
    Ok(())
}

/// Applies one wake-up to the tree; `true` means quit. Running a parked task
/// conservatively dirties the frame — the task may have touched any binding.
fn process(
    wake: Wake,
    root: &mut Node,
    focus_chain: &mut Vec<u32>,
    focused: &mut Option<u32>,
    dirty: &Cell<bool>,
) -> bool {
    match wake {
        Wake::Task(runnable) => {
            runnable.run();
            dirty.set(true);
        }
        Wake::Input(event) => {
            return handle_input(event, root, focus_chain, focused, dirty);
        }
        Wake::InputEof => return true,
    }
    false
}

/// Dispatches a terminal event; `true` means quit.
fn handle_input(
    event: Event,
    root: &mut Node,
    focus_chain: &mut Vec<u32>,
    focused: &mut Option<u32>,
    dirty: &Cell<bool>,
) -> bool {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Esc => return true,
            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                return true;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                focus_chain.clear();
                root.collect_focus(focus_chain);
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
                    *focused = Some(focus_chain[next]);
                    root.sync_focused(*focused);
                }
                dirty.set(true);
            }
            _ => {
                if let Some(id) = *focused
                    && root.handle_key(id, &key)
                {
                    dirty.set(true);
                }
            }
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(id) = root.mouse(
                    mouse.column,
                    mouse.row,
                    matches!(mouse.kind, MouseEventKind::Drag(_)),
                ) {
                    *focused = Some(id);
                    root.sync_focused(*focused);
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
    false
}

/// Posts `spawn_local` work to the event loop's wake channel.
///
/// A waker fires on whatever thread triggered it — the global executor's
/// workers included — so the schedule hook sends runnables back to the loop's
/// thread through the channel instead of touching a thread-local queue.
/// Runnables are deliberately not run inline either: reactive work re-enters
/// the code under render, which deadlocks when polled in the middle of the
/// call that spawned it.
struct TuiLocalExecutor {
    wake_tx: mpsc::Sender<Wake>,
}

impl LocalExecutor for TuiLocalExecutor {
    type Task<T: 'static> = AsyncTask<T>;

    fn spawn_local<Fut>(&self, fut: Fut) -> Self::Task<Fut::Output>
    where
        Fut: Future + 'static,
    {
        let wake_tx = self.wake_tx.clone();
        let (runnable, task) = async_task::spawn_local(fut, move |runnable: Runnable| {
            if let Err(unsent) = wake_tx.send(Wake::Task(runnable)) {
                // Teardown race: a waker held by another thread fired after
                // the loop dropped the receiver. Dropping a `spawn_local`
                // runnable off its spawning thread panics by design
                // (async-task's thread check), so leak it instead — bounded
                // to shutdown, reclaimed at process exit.
                let Wake::Task(unsent) = unsent.0 else {
                    return;
                };
                std::mem::forget(unsent);
            }
        });
        runnable.schedule();
        task
    }
}

/// Owns the terminal for the app's lifetime: raw mode, alternate screen,
/// mouse capture, and the double buffer every frame is diffed against.
///
/// The buffers live here rather than inside ratatui's `Terminal` because the
/// scroll-region replay must rotate the *previous* buffer before diffing —
/// `Terminal` never exposes it. Restores the terminal on drop, including
/// through unwinding.
struct TerminalGuard {
    backend: CrosstermBackend<Stdout>,
    /// What is currently on screen; `present` keeps this true.
    prev: Buffer,
    /// The frame being rendered.
    cur: Buffer,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(out);
        let blank = Buffer::empty(Rect::ZERO);

        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            previous(info);
        }));

        Ok(Self {
            backend,
            prev: blank.clone(),
            cur: blank,
        })
    }

    /// The live terminal size; queried per frame so a resize between the
    /// `Resize` event and the draw is still seen.
    fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    /// Renders one frame into `cur`, replays the scroll ops the render
    /// produced, writes the remaining diff, and swaps the buffers.
    ///
    /// The render callback receives the current buffer and returns the field
    /// cursor position, if a focused field is on screen.
    fn draw_frame(
        &mut self,
        scroll_ops: &RefCell<Vec<ScrollOp>>,
        render: impl FnOnce(&mut Buffer) -> Option<(u16, u16)>,
    ) -> io::Result<()> {
        let size = self.backend.size()?;
        let resized = size.width != self.cur.area.width || size.height != self.cur.area.height;
        if resized {
            let area = Rect::new(0, 0, size.width, size.height);
            self.prev = Buffer::empty(area);
            self.cur = Buffer::empty(area);
            self.backend.clear_region(ClearType::All)?;
        }
        self.cur.reset();
        scroll_ops.borrow_mut().clear();
        let cursor = render(&mut self.cur);
        present(
            &mut self.backend,
            &mut self.prev,
            &self.cur,
            &scroll_ops.borrow(),
            cursor,
            resized,
        )?;
        mem::swap(&mut self.prev, &mut self.cur);
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}
