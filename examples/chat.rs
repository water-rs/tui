//! Chat with an ACP agent in the terminal.
//!
//! `cargo run --example chat` spawns `devin acp` (override with
//! `ACP_AGENT="program args..."`, pick a model with `ACP_MODEL=<id>`), streams
//! `session/update`s into a scrollable transcript, and sends your input as
//! `session/prompt`s. Permission requests are auto-approved and logged into
//! the transcript — this is a personal playground, not a sandbox.
//!
//! `Enter` sends, `Tab`/`Shift-Tab` moves focus, `Esc`/`Ctrl-C` quits.

use std::cell::RefCell;
use std::env;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;

use aither_acp::{
    AcpClient, ClientHandler, ContentBlock, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionParams, RequestPermissionResult, SessionNotification, SessionUpdate,
    TextContent,
};
use aither_mcp::protocol::JsonRpcError;
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
use waterui_tui::OnSubmit;

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
/// handler itself wants to surface (permission auto-approvals).
enum AgentEvent {
    Update(Box<SessionNotification>),
    Note(String),
}

/// `ClientHandler` that forwards every session update to the UI thread. All
/// state mutation happens on the receiver side; the handler only carries a
/// channel endpoint, so it stays `Send` on the connection task.
struct Forwarder {
    tx: async_channel::Sender<AgentEvent>,
}

impl ClientHandler for Forwarder {
    async fn session_update(&self, notification: SessionNotification) {
        let _ = self
            .tx
            .send(AgentEvent::Update(Box::new(notification)))
            .await;
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
        let _ = self
            .tx
            .send(AgentEvent::Note(format!(
                "auto-approved `{}` → {option_name}",
                params.tool_call.title
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

/// Applies one forwarded event to the transcript binding.
fn apply(lines: &Binding<Vec<Line>>, event: AgentEvent) {
    match event {
        AgentEvent::Note(note) => push_line(lines, "perm", note),
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
                push_line(lines, "tool", call.title);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(status) = update.status {
                    push_line(
                        lines,
                        "tool",
                        format!("{} → {status:?}", update.tool_call_id),
                    );
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

/// Role colors for transcript prefixes.
fn prefix_style(prefix: &str) -> Style {
    let color = |r, g, b| Color::from(ResolvedColor::from_srgb(Srgb::new(r, g, b)));
    match prefix {
        "you" => Style::new().foreground(color(0.42, 0.68, 1.0)),
        "devin" => Style::new().foreground(color(0.55, 0.85, 0.55)),
        "think" | "tool" | "plan" | "perm" => {
            Style::new().foreground(Color::new(MutedForegroundColor))
        }
        _ => Style::new().foreground(Color::new(MutedForegroundColor)),
    }
}

/// Flattens the transcript into one styled string: a colored prefix line per
/// row, agent rows rendered through the markdown parser.
fn render_transcript(lines: &[Line]) -> StyledStr {
    let mut out = StyledStr::empty();
    for line in lines {
        out.push(format!("{}\n", line.prefix), prefix_style(line.prefix));
        if line.markdown {
            for (text, style) in StyledStr::from_markdown(&line.text).chunks() {
                out.push(text.clone(), style.clone());
            }
        } else {
            out.push_str(line.text.clone());
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
    let client = match spawn_agent(&program, &args, &cwd, tx) {
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
            let scroller = scroller.clone();
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
                scroller.scroll_to(Point::new(0.0, f32::MAX));
            }
        })
        .detach();
    }

    // Drain forwarded agent traffic on the main thread.
    executor_core::spawn_local({
        let lines = lines.clone();
        let scroller = scroller.clone();
        async move {
            while let Ok(event) = rx.recv().await {
                apply(&lines, event);
                scroller.scroll_to(Point::new(0.0, f32::MAX));
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
            scroller.scroll_to(Point::new(0.0, f32::MAX));
            let lines = lines.clone();
            let busy = busy.clone();
            let scroller = scroller.clone();
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
                scroller.scroll_to(Point::new(0.0, f32::MAX));
            })
            .detach();
        }
    };

    let transcript = lines.map(|lines| render_transcript(&lines)).computed();
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
        scroll(text(transcript)).scroll_controller(&scroller),
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
    Ok(AcpClient::connect(transport, Forwarder { tx }))
}

fn main() -> io::Result<()> {
    waterui_tui::run(app)
}
