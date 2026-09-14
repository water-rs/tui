//! Chat with an ACP agent in the terminal.
//!
//! `cargo run --example chat` spawns `devin acp` (override with
//! `ACP_AGENT="program args..."`, pick a model with `ACP_MODEL=<id>`), streams
//! `session/update`s into a scrollable transcript, and sends your input as
//! `session/prompt`s. Permission requests are auto-approved and logged into
//! the transcript — this is a personal playground, not a sandbox.
//!
//! `Enter` sends, `Tab`/`Shift-Tab` moves focus, `Esc`/`Ctrl-C` quits.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aither_acp::{
    AcpClient, ClientCapabilities, ClientHandler, ContentBlock, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionParams, RequestPermissionResult,
    SessionNotification, SessionUpdate, TerminalCreateParams, TerminalCreateResult,
    TerminalExitStatus, TerminalKillParams, TerminalKillResult, TerminalOutputParams,
    TerminalOutputResult, TerminalReleaseParams, TerminalReleaseResult, TerminalWaitForExitParams,
    TextContent, ToolCallContent,
};
use aither_mcp::protocol::JsonRpcError;
use event_listener::Event;
use futures_lite::AsyncReadExt;
use nami::{Binding, SignalExt, binding};
use waterui_controls::{button, field};
use waterui_core::env::with;
use waterui_core::layout::Point;
use waterui_core::{Str, View};
use waterui_graphics::color::{Color, MutedForegroundColor, ResolvedColor, Srgb};
use waterui_internal::view::ViewExt;
use waterui_layout::divider::Divider;
use waterui_layout::scroll::{ScrollController, scroll};
use waterui_layout::stack::{hstack, vstack};
use waterui_text::styled::{Style, StyledStr};
use waterui_text::text::text;
use waterui_tui::{OnScroll, OnSubmit};

/// A transcript row: a role prefix plus its text.
struct Line {
    prefix: &'static str,
    text: String,
    /// Render `text` through the markdown parser (agent output).
    markdown: bool,
}

impl Clone for Line {
    fn clone(&self) -> Self {
        Self {
            prefix: self.prefix,
            text: self.text.clone(),
            markdown: self.markdown,
        }
    }
}

/// Agent traffic forwarded to the UI: session updates plus notes the client
/// handler itself wants to surface (permission auto-approvals, terminal
/// lifecycle and output).
enum AgentEvent {
    Update(Box<SessionNotification>),
    Note(String),
    /// A `terminal/create` happened: `command` labels the transcript block.
    TermStart {
        /// The id returned to the agent.
        terminal_id: String,
        /// Displayed command line (`$ cmd args…`).
        command: String,
    },
    /// A terminal's retained output, ANSI-stripped and complete (not a delta).
    Term {
        /// Which terminal block this output belongs to.
        terminal_id: String,
        /// Full retained output so far.
        text: String,
    },
}

/// Live `terminal/*` sessions keyed by the ids we hand the agent.
#[derive(Default)]
struct Terminals {
    map: Mutex<HashMap<String, Arc<Terminal>>>,
    next: AtomicU64,
}

impl Terminals {
    fn get(&self, terminal_id: &str) -> Option<Arc<Terminal>> {
        self.map.lock().unwrap().get(terminal_id).cloned()
    }

    fn lock_remove(&self, terminal_id: &str) -> Option<Arc<Terminal>> {
        self.map.lock().unwrap().remove(terminal_id)
    }
}

/// One running terminal: a piped child whose output is retained (capped at
/// the requested byte limit) and mirrored into the transcript as it arrives.
struct Terminal {
    retained: Mutex<Retained>,
    status: Mutex<Option<TerminalExitStatus>>,
    /// Broadcast when `status` is written — `wait_for_exit` waits on it.
    exited: Event,
    /// Child pid, for `terminal/kill`/`release`.
    pid: u32,
}

impl Terminal {
    /// SIGKILLs the child if it has not exited; the exit task records the
    /// status, so this is a no-op for finished terminals.
    fn kill(&self) {
        if self.status.lock().unwrap().is_some() {
            return;
        }
        if let Some(pid) = rustix::process::Pid::from_raw(self.pid as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

/// Drains one child pipe into the retained buffer, mirroring the full
/// ANSI-stripped output into the transcript on every chunk.
async fn drain_pipe(
    mut pipe: Pin<Box<dyn futures_lite::AsyncRead + Send>>,
    terminal: Arc<Terminal>,
    terminal_id: String,
    tx: async_channel::Sender<AgentEvent>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = {
                    let mut retained = terminal.retained.lock().unwrap();
                    retained.push(&buf[..n]);
                    String::from_utf8_lossy(&strip_ansi_escapes::strip(&retained.bytes))
                        .into_owned()
                };
                let _ = tx
                    .send(AgentEvent::Term {
                        terminal_id: terminal_id.clone(),
                        text,
                    })
                    .await;
            }
        }
    }
}

/// Waits for the child to exit, records the status, and wakes
/// `wait_for_exit` listeners. Owns the `Child` so `kill_on_drop` keeps
/// working for the child's lifetime.
async fn wait_exit(mut child: async_process::Child, terminal: Arc<Terminal>) {
    let status = child.status().await;
    let exit = match status {
        Ok(status) => TerminalExitStatus {
            exit_code: status.code().map(i64::from),
            signal: signal_name(&status),
            meta: None,
        },
        Err(_) => TerminalExitStatus {
            exit_code: None,
            signal: Some("lost".to_string()),
            meta: None,
        },
    };
    *terminal.status.lock().unwrap() = Some(exit);
    terminal.exited.notify(usize::MAX);
}

/// The terminating signal name on unix; `None` elsewhere.
fn signal_name(status: &ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| signal.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// Retained terminal output.
struct Retained {
    bytes: Vec<u8>,
    limit: u64,
    truncated: bool,
}

impl Retained {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        let limit = self.limit as usize;
        if self.bytes.len() > limit {
            self.bytes.drain(..self.bytes.len() - limit);
            self.truncated = true;
        }
    }
}

/// `ClientHandler` that forwards every session update to the UI thread and
/// runs the `terminal/*` backend. State mutation happens on the receiver
/// side; the handler only carries a channel endpoint plus the terminal table,
/// so it stays `Send` on the connection task.
struct Forwarder {
    tx: async_channel::Sender<AgentEvent>,
    terminals: Arc<Terminals>,
}

/// Default cap on retained terminal output when the agent does not set one.
const TERMINAL_OUTPUT_LIMIT: u64 = 256 * 1024;

/// Characters of a terminal block's tail rendered in the transcript.
const TERM_DISPLAY_CAP: usize = 8 * 1024;

impl ClientHandler for Forwarder {
    fn capabilities(&self) -> ClientCapabilities {
        ClientCapabilities {
            terminal: true,
            ..ClientCapabilities::default()
        }
    }

    async fn session_update(&self, notification: SessionNotification) {
        let _ = self
            .tx
            .send(AgentEvent::Update(Box::new(notification)))
            .await;
    }

    async fn terminal_create(
        &self,
        params: TerminalCreateParams,
    ) -> Result<TerminalCreateResult, JsonRpcError> {
        tracing::info!(
            command = %params.command,
            args = ?params.args,
            cwd = ?params.cwd,
            env = params.env.len(),
            "terminal/create"
        );
        // `command` is a command line, not a program path — devin sends raw
        // shell strings (e.g. `for i in …; done`), while spec-style agents
        // send `command` + `args`. Running `sh -c` with the args appended
        // shell-quoted covers both; `/bin/sh` keeps it POSIX even when the
        // user's $SHELL is fish or another non-POSIX shell.
        let mut line = params.command.clone();
        for arg in &params.args {
            line.push(' ');
            line.push_str(&shell_escape::escape(arg.clone().into()));
        }
        let mut command = async_process::Command::new("sh");
        command
            .arg("-c")
            .arg(&line)
            .envs(
                params
                    .env
                    .iter()
                    .map(|var| (var.name.clone(), var.value.clone())),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &params.cwd {
            command.current_dir(cwd);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(%error, "terminal/create: spawn failed");
                return Err(JsonRpcError::internal_error(error.to_string()));
            }
        };
        tracing::info!(pid = child.id(), "terminal/create: spawned");

        let id = self.terminals.next.fetch_add(1, Ordering::Relaxed);
        let terminal_id = format!("terminal-{id}");
        let terminal = Arc::new(Terminal {
            retained: Mutex::new(Retained {
                bytes: Vec::new(),
                limit: params.output_byte_limit.unwrap_or(TERMINAL_OUTPUT_LIMIT),
                truncated: false,
            }),
            status: Mutex::new(None),
            exited: Event::new(),
            pid: child.id(),
        });

        let mut title = params.command.clone();
        for arg in &params.args {
            title.push(' ');
            title.push_str(arg);
        }
        let _ = self
            .tx
            .send(AgentEvent::TermStart {
                terminal_id: terminal_id.clone(),
                command: format!("$ {title}"),
            })
            .await;

        for pipe in [
            child.stdout.take().map(|pipe| Box::pin(pipe) as _),
            child.stderr.take().map(|pipe| Box::pin(pipe) as _),
        ]
        .into_iter()
        .flatten()
        {
            executor_core::spawn(drain_pipe(
                pipe,
                terminal.clone(),
                terminal_id.clone(),
                self.tx.clone(),
            ))
            .detach();
        }
        executor_core::spawn(wait_exit(child, terminal.clone())).detach();
        self.terminals
            .map
            .lock()
            .unwrap()
            .insert(terminal_id.clone(), terminal);
        Ok(TerminalCreateResult {
            terminal_id,
            meta: None,
        })
    }

    async fn terminal_output(
        &self,
        params: TerminalOutputParams,
    ) -> Result<TerminalOutputResult, JsonRpcError> {
        let Some(terminal) = self.terminals.get(&params.terminal_id) else {
            return Err(JsonRpcError::invalid_params("unknown terminal id"));
        };
        let retained = terminal.retained.lock().unwrap();
        Ok(TerminalOutputResult {
            output: String::from_utf8_lossy(&retained.bytes).into_owned(),
            truncated: retained.truncated,
            exit_status: terminal.status.lock().unwrap().clone(),
            meta: None,
        })
    }

    async fn terminal_wait_for_exit(
        &self,
        params: TerminalWaitForExitParams,
    ) -> Result<TerminalExitStatus, JsonRpcError> {
        let Some(terminal) = self.terminals.get(&params.terminal_id) else {
            return Err(JsonRpcError::invalid_params("unknown terminal id"));
        };
        // Listen before checking so a status written between the two is seen.
        let listener = terminal.exited.listen();
        if let Some(status) = terminal.status.lock().unwrap().clone() {
            return Ok(status);
        }
        listener.await;
        Ok(terminal
            .status
            .lock()
            .unwrap()
            .clone()
            .expect("exit notification implies a recorded status"))
    }

    async fn terminal_kill(
        &self,
        params: TerminalKillParams,
    ) -> Result<TerminalKillResult, JsonRpcError> {
        let Some(terminal) = self.terminals.get(&params.terminal_id) else {
            return Err(JsonRpcError::invalid_params("unknown terminal id"));
        };
        terminal.kill();
        Ok(TerminalKillResult { meta: None })
    }

    async fn terminal_release(
        &self,
        params: TerminalReleaseParams,
    ) -> Result<TerminalReleaseResult, JsonRpcError> {
        if let Some(terminal) = self.terminals.lock_remove(&params.terminal_id) {
            terminal.kill();
        }
        Ok(TerminalReleaseResult { meta: None })
    }

    async fn request_permission(
        &self,
        params: RequestPermissionParams,
    ) -> Result<RequestPermissionResult, JsonRpcError> {
        // Personal playground: approve autonomously but prefer the
        // single-use grant so repeated risky calls get re-asked; the
        // transcript note keeps every approval visible.
        let option = params
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            .or_else(|| {
                params
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            })
            .or(params.options.first());
        let option_id = option.map_or_else(|| "allow".to_string(), |o| o.option_id.clone());
        let option_name = option.map_or_else(|| "allow".to_string(), |o| o.name.clone());
        let tool = if params.tool_call.title.is_empty() {
            &params.tool_call.tool_call_id
        } else {
            &params.tool_call.title
        };
        let _ = self
            .tx
            .send(AgentEvent::Note(format!(
                "auto-approved `{tool}` → {option_name}"
            )))
            .await;
        Ok(RequestPermissionResult {
            outcome: RequestPermissionOutcome::Selected { option_id },
            meta: None,
        })
    }
}

/// Pushes a new transcript row.
fn push_line(lines: &Binding<Vec<Line>>, prefix: &'static str, text: impl Into<String>) {
    let mut all = lines.get();
    all.push(Line {
        prefix,
        text: text.into(),
        markdown: false,
    });
    lines.set(all);
}

/// Appends streaming text to the last row when the role matches, else starts
/// a new row.
fn append_chunk(lines: &Binding<Vec<Line>>, prefix: &'static str, markdown: bool, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut all = lines.get();
    match all.last_mut() {
        Some(last) if last.prefix == prefix => last.text.push_str(text),
        _ => all.push(Line {
            prefix,
            text: text.to_string(),
            markdown,
        }),
    }
    lines.set(all);
}

fn chunk_text(chunk: &aither_acp::ContentChunk) -> &str {
    match &chunk.content {
        ContentBlock::Text(text) => &text.text,
        _ => "",
    }
}

/// Applies one forwarded event to the transcript binding. `terms` maps
/// terminal ids to their transcript row so streamed output lands in place.
fn apply(lines: &Binding<Vec<Line>>, terms: &mut HashMap<String, usize>, event: AgentEvent) {
    match event {
        AgentEvent::Note(note) => push_line(lines, "perm", note),
        AgentEvent::TermStart {
            terminal_id,
            command,
        } => {
            // Reserve the output row next to its `$` title so a command that
            // starts slowly still lands there instead of at the bottom.
            let mut all = lines.get();
            all.push(Line {
                prefix: "tool",
                text: command,
                markdown: false,
            });
            all.push(Line {
                prefix: "term",
                text: String::new(),
                markdown: false,
            });
            terms.insert(terminal_id, all.len() - 1);
            lines.set(all);
        }
        AgentEvent::Term { terminal_id, text } => {
            let mut all = lines.get();
            let index = *terms.entry(terminal_id).or_insert_with(|| {
                all.push(Line {
                    prefix: "term",
                    text: String::new(),
                    markdown: false,
                });
                all.len() - 1
            });
            all[index].text = text;
            lines.set(all);
        }
        AgentEvent::Update(notification) => match notification.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                append_chunk(lines, "devin", true, chunk_text(&chunk));
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                append_chunk(lines, "think", false, chunk_text(&chunk));
            }
            SessionUpdate::UserMessageChunk(chunk) => {
                append_chunk(lines, "you", false, chunk_text(&chunk));
            }
            SessionUpdate::ToolCall(call) => {
                if call.title.is_empty() {
                    push_line(lines, "tool", call.tool_call_id);
                } else {
                    push_line(lines, "tool", call.title);
                }
                render_tool_content(lines, &call.content);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(title) = update.title {
                    push_line(lines, "tool", title);
                }
                if let Some(status) = update.status {
                    push_line(
                        lines,
                        "tool",
                        format!("{} → {status:?}", update.tool_call_id),
                    );
                }
                if let Some(content) = &update.content {
                    render_tool_content(lines, content);
                }
            }
            SessionUpdate::Plan(plan) => {
                for entry in plan.entries {
                    push_line(lines, "plan", entry.content);
                }
            }
            _ => {}
        },
    }
}

/// Renders the content a tool call reports: text blocks and edited paths.
/// Terminal content streams separately through `AgentEvent::Term`.
fn render_tool_content(lines: &Binding<Vec<Line>>, content: &[ToolCallContent]) {
    for item in content {
        match item {
            ToolCallContent::Content {
                content: ContentBlock::Text(text),
            } => {
                let text = text.text.trim_end();
                if !text.is_empty() {
                    push_line(lines, "tool", text);
                }
            }
            ToolCallContent::Diff(diff) => {
                push_line(lines, "tool", format!("edit {}", diff.path.display()));
            }
            _ => {}
        }
    }
}

/// Role colors for transcript prefixes.
fn prefix_style(prefix: &str) -> Style {
    let color = |r, g, b| Color::from(ResolvedColor::from_srgb(Srgb::new(r, g, b)));
    match prefix {
        "you" => Style::new().foreground(color(0.42, 0.68, 1.0)),
        "devin" => Style::new().foreground(color(0.55, 0.85, 0.55)),
        "think" | "tool" | "plan" | "perm" | "term" => {
            Style::new().foreground(Color::new(MutedForegroundColor))
        }
        _ => Style::new().foreground(Color::new(MutedForegroundColor)),
    }
}

/// Flattens the transcript into one styled string: a colored prefix line per
/// row, agent rows rendered through the markdown parser.
///
/// This runs on every repaint, so markdown results are memoized per row:
/// during a stream only the row actively being appended re-parses.
fn render_transcript(lines: &[Line], cache: &mut Vec<(u64, StyledStr)>) -> StyledStr {
    use std::hash::{Hash, Hasher};
    let mut out = StyledStr::empty();
    cache.resize_with(lines.len(), || (0, StyledStr::empty()));
    for (line, cached) in lines.iter().zip(cache.iter_mut()) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        line.prefix.hash(&mut hasher);
        line.markdown.hash(&mut hasher);
        line.text.hash(&mut hasher);
        let fingerprint = hasher.finish();
        if cached.0 != fingerprint {
            cached.0 = fingerprint;
            cached.1 = if line.markdown {
                StyledStr::from_markdown(&line.text)
            } else if line.prefix == "term" && line.text.len() > TERM_DISPLAY_CAP {
                // Terminal output is unbounded; show the tail, which is where
                // a running command's live output accumulates.
                let mut start = line.text.len() - TERM_DISPLAY_CAP;
                while !line.text.is_char_boundary(start) {
                    start += 1;
                }
                let mut shown = StyledStr::empty();
                shown.push_str("…\n");
                shown.push_str(line.text[start..].to_string());
                shown
            } else {
                StyledStr::plain(line.text.clone())
            };
        }
        out.push(format!("{}\n", line.prefix), prefix_style(line.prefix));
        for (text, style) in cached.1.chunks() {
            out.push(text.clone(), style.clone());
        }
        out.push_str("\n\n");
    }
    out
}

/// Focusable targets in the chat view.
#[derive(Clone, PartialEq, Eq)]
enum Focus {
    /// The message input field.
    Input,
}

/// Splits `ACP_AGENT` (default `devin acp`) into program + args.
fn agent_command() -> (String, Vec<String>) {
    let spec = env::var("ACP_AGENT").unwrap_or_else(|_| "devin acp".to_string());
    let mut parts = spec.split_whitespace();
    let program = parts.next().unwrap_or("devin").to_string();
    (program, parts.map(str::to_string).collect())
}

fn app() -> impl View {
    let lines: Binding<Vec<Line>> = binding(Vec::new());
    let busy = binding(false);
    let input: Binding<Str> = binding(Str::from(""));
    let session: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let scroller = ScrollController::<Point>::default();
    let focus: Binding<Option<Focus>> = binding(None);
    // Follow mode: new transcript content keeps the view pinned to the bottom
    // only while the user hasn't scrolled up to read back.
    let following = Rc::new(Cell::new(true));
    let follow = {
        let following = following.clone();
        let scroller = scroller.clone();
        move || {
            if following.get() {
                scroller.scroll_to(Point::new(0.0, f32::MAX));
            }
        }
    };

    // Focus the input once the tree is dispatched: the watcher that turns a
    // `Focused` metadata value into a focus request only exists after
    // dispatch, so the request is deferred to the first local-executor drain.
    executor_core::spawn_local({
        let focus = focus.clone();
        async move { focus.set(Some(Focus::Input)) }
    })
    .detach();

    let (tx, rx) = async_channel::unbounded::<AgentEvent>();

    // Spawn the agent and drive its connection on the global executor; the
    // returned future owns the child process and the stdio transport. The
    // child's stderr goes to a log file — inheriting it would paint agent
    // logs straight over the alternate screen.
    let (program, args) = agent_command();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let terminals = Arc::new(Terminals::default());
    let client = match spawn_agent(&program, &args, &cwd, tx, terminals) {
        Ok((client, connection)) => {
            executor_core::spawn(connection).detach();
            push_line(
                &lines,
                "status",
                format!("spawned `{program}`; connecting…"),
            );
            Some(client)
        }
        Err(error) => {
            push_line(
                &lines,
                "status",
                format!("failed to spawn `{program}`: {error}"),
            );
            None
        }
    };

    // Handshake + open a session; runs on the main thread's local executor.
    if let Some(client) = client.clone() {
        executor_core::spawn_local({
            let session = session.clone();
            let lines = lines.clone();
            let follow = follow.clone();
            async move {
                match client.initialize().await {
                    Ok(init) => {
                        let agent = init.agent_info.map_or("?".to_string(), |info| info.name);
                        push_line(&lines, "status", format!("connected to {agent}"));
                    }
                    Err(error) => {
                        push_line(&lines, "status", format!("initialize failed: {error}"));
                        return;
                    }
                }
                match client.new_session(cwd, Vec::new()).await {
                    Ok(result) => {
                        if let Ok(model) = env::var("ACP_MODEL") {
                            let _ = client
                                .set_config_option(&result.session_id, "model", model)
                                .await;
                        }
                        *session.borrow_mut() = Some(result.session_id);
                        push_line(&lines, "status", "session ready — say something");
                    }
                    Err(error) => {
                        push_line(&lines, "status", format!("session/new failed: {error}"));
                    }
                }
                follow();
            }
        })
        .detach();
    }

    // Drain forwarded agent traffic on the main thread.
    executor_core::spawn_local({
        let lines = lines.clone();
        let follow = follow.clone();
        async move {
            let mut terms = HashMap::new();
            while let Ok(event) = rx.recv().await {
                apply(&lines, &mut terms, event);
                follow();
            }
        }
    })
    .detach();

    // The send action shared by Enter-on-field and the Send button.
    let send = {
        let input = input.clone();
        let lines = lines.clone();
        let busy = busy.clone();
        let session = session.clone();
        let scroller = scroller.clone();
        let follow = follow.clone();
        move || {
            if busy.get() {
                return;
            }
            let text = input.get().to_string();
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let Some(client) = client.clone() else {
                push_line(&lines, "status", "agent is not running");
                return;
            };
            let Some(session_id) = session.borrow().clone() else {
                push_line(&lines, "status", "agent is still starting…");
                return;
            };
            input.set(Str::from(""));
            push_line(&lines, "you", text);
            busy.set(true);
            // Sending your own message always snaps to the bottom and
            // re-engages follow mode.
            scroller.scroll_to(Point::new(0.0, f32::MAX));
            let lines = lines.clone();
            let busy = busy.clone();
            let follow = follow.clone();
            let text = text.to_string();
            executor_core::spawn_local(async move {
                let result = client
                    .prompt(
                        &session_id,
                        vec![ContentBlock::Text(TextContent {
                            text,
                            annotations: None,
                        })],
                    )
                    .await;
                if let Err(error) = result {
                    push_line(&lines, "status", format!("prompt failed: {error}"));
                }
                busy.set(false);
                follow();
            })
            .detach();
        }
    };

    let transcript = {
        let cache = Rc::new(RefCell::new(Vec::new()));
        lines
            .map(move |lines| render_transcript(&lines, &mut cache.borrow_mut()))
            .computed()
    };
    let status = busy
        .map(|busy| {
            if busy {
                StyledStr::plain("thinking…")
            } else {
                StyledStr::empty()
            }
        })
        .computed();

    let mut header_text = StyledStr::empty();
    header_text.push(
        "acp chat — Enter sends · Esc quits",
        Style::new().foreground(Color::new(MutedForegroundColor)),
    );
    let header = text(header_text);
    let prompt_field = {
        let send = send.clone();
        with(
            field("you", &input)
                .prompt("message…")
                .focused(&focus, Focus::Input),
            OnSubmit::new(move |_: &waterui_core::Environment| send()),
        )
    };
    let send_button = button("Send").action(send);

    vstack((
        header,
        with(
            scroll(text(transcript)).scroll_controller(&scroller),
            OnScroll::new(move |metrics| following.set(metrics.at_end())),
        ),
        text(status),
        Divider,
        hstack((prompt_field, send_button)),
    ))
}

/// Spawns the ACP agent child with its stderr parked in a temp log file,
/// returning the client plus the connection future to drive.
fn spawn_agent(
    program: &str,
    args: &[String],
    cwd: &std::path::Path,
    tx: async_channel::Sender<AgentEvent>,
    terminals: Arc<Terminals>,
) -> Result<
    (
        AcpClient<Forwarder>,
        impl std::future::Future<Output = ()> + Send + 'static,
    ),
    String,
> {
    let mut command = async_process::Command::new(program);
    command
        .args(args)
        .envs(env::vars())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let stderr_log = env::temp_dir().join("waterui-tui-acp-stderr.log");
    match File::create(&stderr_log) {
        Ok(file) => {
            command.stderr(Stdio::from(file));
        }
        Err(_) => {
            command.stderr(Stdio::null());
        }
    }
    // `ChildProcessTransport::from_command` forces `stderr(inherit)`, which
    // would paint agent logs over the alternate screen — spawn manually and
    // hand the configured child to `from_child` instead. `kill_on_drop` keeps
    // the agent from outliving its transport when the connection task dies.
    command.kill_on_drop(true);
    let child = command.spawn().map_err(|error| error.to_string())?;
    let transport = aither_mcp::transport::ChildProcessTransport::from_child(child)
        .map_err(|error| error.to_string())?;
    Ok(AcpClient::connect(transport, Forwarder { tx, terminals }))
}

fn main() -> io::Result<()> {
    // The alternate screen owns the terminal, so diagnostics go to a file.
    if let Ok(file) = File::create(env::temp_dir().join("waterui-tui-chat-trace.log")) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info,aither=debug".parse().unwrap()),
            )
            .with_writer(move || file.try_clone().expect("trace file"))
            .with_ansi(false)
            .init();
    }
    waterui_tui::run(app)
}
