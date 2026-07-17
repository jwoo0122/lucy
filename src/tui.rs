use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::prelude::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::app::Harness;
use crate::cancellation::CancellationToken;
use crate::model::ChatMessage;
use crate::protocol::{EventSink, ProtocolEvent};
use crate::redaction::redact_secret;
use crate::session::SessionHistoryRecord;

const EVENT_POLL: Duration = Duration::from_millis(50);
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
const MAX_DISPLAY_INPUT_CHARS: usize = 16 * 1024;
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Maximum number of wrapped input rows the input box grows to before it
/// stops expanding and scrolls its contents internally.
const MAX_INPUT_ROWS: u16 = 12;

pub(crate) fn run<W: Write>(mut harness: Harness, resumed: bool, stdout: W) -> Result<(), String> {
    let secret = harness.provider.api_key().to_owned();
    let mut state = UiState::from_history(
        &harness.session.history,
        &secret,
        &harness.session.id,
        resumed,
    );
    let (request_tx, request_rx) = mpsc::channel::<WorkerRequest>();
    let (message_tx, message_rx) = mpsc::channel::<WorkerMessage>();

    let stdout = stdout;
    enable_raw_mode().map_err(|error| format!("unable to enable terminal input: {error}"))?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            return Err(format!("unable to initialize terminal UI: {error}"));
        }
    };
    let mut terminal_guard = TerminalGuard::new(terminal);
    let backend = terminal_guard.terminal_mut().backend_mut();
    if let Err(error) = execute!(
        backend,
        EnterAlternateScreen,
        EnableMouseCapture,
        Hide
    ) {
        return Err(format!("unable to enter terminal UI: {error}"));
    }
    // Kitty keyboard protocol makes Shift+Enter (and other modified keys)
    // distinguishable from plain Enter. Only push it on terminals known to
    // support it; otherwise the enhancement sequence would leak as literal
    // text on screen.
    if supports_keyboard_enhancement() {
        let _ = execute!(
            backend,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
            )
        );
        terminal_guard.keyboard_enhancement = true;
    }
    let worker = thread::spawn(move || worker_loop(&mut harness, request_rx, message_tx, resumed));

    let result = event_loop(
        terminal_guard.terminal_mut(),
        &mut state,
        &request_tx,
        &message_rx,
    );

    if let Some(token) = state.active_cancel.take() {
        let _ = token.cancel();
    }
    let _ = request_tx.send(WorkerRequest::Shutdown);
    wait_for_worker(worker, WORKER_SHUTDOWN_GRACE);
    drop(terminal_guard);
    result
}

fn worker_loop(
    harness: &mut Harness,
    requests: Receiver<WorkerRequest>,
    messages: Sender<WorkerMessage>,
    resumed: bool,
) {
    let mut sink = ChannelSink {
        sender: messages.clone(),
    };
    if sink
        .emit_event(&ProtocolEvent::Session {
            session_id: harness.session.id.clone(),
            resumed,
        })
        .is_err()
    {
        return;
    }

    while let Ok(request) = requests.recv() {
        match request {
            WorkerRequest::Turn { text, cancel } => {
                if let Err(error) = harness.handle_message(&text, &mut sink, Some(&cancel)) {
                    let message = redact_secret(&error, Some(harness.provider.api_key()));
                    let _ = sink.emit_event(&ProtocolEvent::Error { message });
                }
                let _ = messages.send(WorkerMessage::Finished);
            }
            WorkerRequest::Shutdown => break,
        }
    }
}

fn event_loop<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    state: &mut UiState,
    requests: &Sender<WorkerRequest>,
    messages: &Receiver<WorkerMessage>,
) -> Result<(), String> {
    let mut quitting = false;
    loop {
        loop {
            match messages.try_recv() {
                Ok(WorkerMessage::Event(event)) => state.apply_event(event),
                Ok(WorkerMessage::Finished) => {
                    state.busy = false;
                    state.active_cancel = None;
                    discard_pending_input()?;
                    match state.status.as_str() {
                        "cancelling" => state.status = "사용자 중단".to_owned(),
                        "finalizing" => state.status = "ready".to_owned(),
                        _ => {}
                    }
                    if quitting {
                        return Ok(());
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if state.busy {
                        return Err("TUI worker stopped unexpectedly".to_owned());
                    }
                    return Ok(());
                }
            }
        }

        terminal
            .draw(|frame| draw(frame, state))
            .map_err(|error| format!("unable to render TUI: {error}"))?;

        if quitting {
            thread::sleep(EVENT_POLL);
            continue;
        }
        if event::poll(EVENT_POLL)
            .map_err(|error| format!("unable to read terminal input: {error}"))?
        {
            let event =
                event::read().map_err(|error| format!("unable to read terminal input: {error}"))?;
            if let Event::Mouse(mouse) = event {
                let size = terminal
                    .size()
                    .map_err(|error| format!("unable to read terminal size: {error}"))?;
                let max_scroll = max_scroll_for_area(state, size);
                handle_mouse_event(state, mouse.kind, max_scroll);
                continue;
            }
            let Event::Key(key) = event else {
                continue;
            };
            if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                continue;
            }
            if is_ctrl_c(&key) {
                if let Some(token) = state.active_cancel.as_ref() {
                    let _ = token.cancel();
                    quitting = true;
                } else {
                    return Ok(());
                }
                continue;
            }
            if key.code == KeyCode::Esc {
                if let Some(token) = state.active_cancel.as_ref() {
                    if token.cancel() {
                        state.status = "cancelling".to_owned();
                    }
                }
                continue;
            }
            if state.busy {
                // A turn owns the input line. ESC and Ctrl-C are the only
                // controls accepted while provider/tool work is active.
                continue;
            }
            match key.code {
                KeyCode::Enter => {
                    // Shift+Enter (and Alt+Enter fallback) insert a literal
                    // newline so the user can write multi-line prompts. Plain
                    // Enter sends the turn. Many terminals cannot distinguish
                    // Shift+Enter from Enter, so Alt+Enter is also accepted.
                    if key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.modifiers.contains(KeyModifiers::ALT)
                    {
                        if state.input.chars().count() < MAX_DISPLAY_INPUT_CHARS {
                            state.input.push('\n');
                        }
                        continue;
                    }
                    let text = std::mem::take(&mut state.input);
                    if text.trim().is_empty() {
                        continue;
                    }
                    let secret = state.secret.clone();
                    state.auto_scroll = true;
                    state.scroll = 0;
                    state.add_user(&text, &secret);
                    let cancel = CancellationToken::new();
                    state.active_cancel = Some(cancel.clone());
                    state.busy = true;
                    state.status = "thinking".to_owned();
                    requests
                        .send(WorkerRequest::Turn { text, cancel })
                        .map_err(|_| "TUI worker is unavailable".to_owned())?;
                }
                KeyCode::Char(character) => {
                    if state.input.chars().count() < MAX_DISPLAY_INPUT_CHARS {
                        state.input.push(character);
                    }
                }
                KeyCode::Backspace => {
                    state.input.pop();
                }
                KeyCode::Up | KeyCode::PageUp => {
                    let size = terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?;
                    let max_scroll = max_scroll_for_area(state, size);
                    scroll_up(state, max_scroll);
                }
                KeyCode::Down | KeyCode::PageDown => {
                    let size = terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?;
                    let max_scroll = max_scroll_for_area(state, size);
                    scroll_down(state, max_scroll);
                }
                KeyCode::Home => {
                    state.scroll = 0;
                    state.auto_scroll = false;
                }
                KeyCode::End => {
                    state.auto_scroll = true;
                    state.scroll = 0;
                }
                _ => {}
            }
        }
    }
}

fn discard_pending_input() -> Result<(), String> {
    while event::poll(Duration::ZERO)
        .map_err(|error| format!("unable to read terminal input: {error}"))?
    {
        let _ = event::read().map_err(|error| format!("unable to read terminal input: {error}"))?;
    }
    Ok(())
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn handle_mouse_event(state: &mut UiState, kind: MouseEventKind, max_scroll: u16) {
    match kind {
        MouseEventKind::ScrollUp => scroll_up(state, max_scroll),
        MouseEventKind::ScrollDown => scroll_down(state, max_scroll),
        _ => {}
    }
}

fn scroll_up(state: &mut UiState, max_scroll: u16) {
    if state.auto_scroll {
        state.scroll = max_scroll;
        state.auto_scroll = false;
    } else {
        state.scroll = state.scroll.min(max_scroll);
    }
    state.scroll = state.scroll.saturating_sub(3);
}

fn scroll_down(state: &mut UiState, max_scroll: u16) {
    if state.auto_scroll {
        return;
    }
    state.scroll = state.scroll.saturating_add(3).min(max_scroll);
}

fn wait_for_worker(worker: JoinHandle<()>, grace: Duration) {
    let deadline = std::time::Instant::now() + grace;
    while !worker.is_finished() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if worker.is_finished() {
        let _ = worker.join();
    }
}

struct TerminalGuard<W: Write> {
    terminal: Option<Terminal<CrosstermBackend<W>>>,
    keyboard_enhancement: bool,
}

impl<W: Write> TerminalGuard<W> {
    fn new(terminal: Terminal<CrosstermBackend<W>>) -> Self {
        Self {
            terminal: Some(terminal),
            keyboard_enhancement: false,
        }
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<W>> {
        self.terminal
            .as_mut()
            .expect("terminal guard is initialized")
    }
}

impl<W: Write> Drop for TerminalGuard<W> {
    fn drop(&mut self) {
        let Some(mut terminal) = self.terminal.take() else {
            return;
        };
        if self.keyboard_enhancement {
            let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        let _ = terminal.show_cursor();
        let _ = disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
        let _ = terminal.backend_mut().flush();
    }
}

/// Heuristic for terminals that implement the kitty keyboard protocol.
/// `PushKeyboardEnhancementFlags` is a no-op on supported terminals, but on
/// unsupported ones the CSI sequence can render as literal text, so it is only
/// enabled when the terminal advertises support via `TERM`/`TERM_PROGRAM`.
fn supports_keyboard_enhancement() -> bool {
    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().map(|value| value.to_lowercase())
    }
    let term = env("TERM").unwrap_or_default();
    let program = env("TERM_PROGRAM").unwrap_or_default();
    if term.starts_with("xterm-kitty")
        || term.starts_with("ghostty")
        || term.starts_with("xterm-ghostty")
    {
        return true;
    }
    matches!(
        program.as_str(),
        "ghostty" | "kitty" | "wezterm" | "alacritty" | "foot" | "footclient" | "iterm.app"
    )
}

enum WorkerRequest {
    Turn {
        text: String,
        cancel: CancellationToken,
    },
    Shutdown,
}

enum WorkerMessage {
    Event(ProtocolEvent),
    Finished,
}

struct ChannelSink {
    sender: Sender<WorkerMessage>,
}

impl EventSink for ChannelSink {
    fn emit_event(&mut self, event: &ProtocolEvent) -> io::Result<()> {
        self.sender
            .send(WorkerMessage::Event(event.clone()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI closed"))
    }
}

struct UiState {
    session_id: String,
    resumed: bool,
    secret: String,
    transcript: Vec<TranscriptItem>,
    input: String,
    status: String,
    busy: bool,
    active_cancel: Option<CancellationToken>,
    scroll: u16,
    auto_scroll: bool,
    cursor_epoch: Instant,
}

impl UiState {
    fn from_history(
        history: &[SessionHistoryRecord],
        secret: &str,
        session_id: &str,
        resumed: bool,
    ) -> Self {
        let mut state = Self {
            session_id: session_id.to_owned(),
            resumed,
            secret: secret.to_owned(),
            transcript: Vec::new(),
            input: String::new(),
            status: "ready".to_owned(),
            busy: false,
            active_cancel: None,
            scroll: 0,
            auto_scroll: true,
            cursor_epoch: Instant::now(),
        };
        for record in history {
            state.add_history_record(record);
        }
        state
    }

    fn add_history_record(&mut self, record: &SessionHistoryRecord) {
        match record {
            SessionHistoryRecord::Message { message, .. } => self.add_message(message),
            SessionHistoryRecord::Interruption {
                assistant_text,
                tool_calls,
                tool_results,
                reason,
                phase,
                ..
            } => {
                if !assistant_text.is_empty() {
                    self.add_assistant_message(assistant_text);
                }
                for call in tool_calls {
                    self.add_tool_call(call);
                }
                for observation in tool_results {
                    self.add_tool_result(
                        &observation.id,
                        &observation.name,
                        observation.result.clone(),
                    );
                }
                self.transcript
                    .push(TranscriptItem::Info(format!("! {reason} ({phase})")));
            }
        }
    }

    fn add_message(&mut self, message: &ChatMessage) {
        match message.role.as_str() {
            "user" => {
                let secret = self.secret.clone();
                self.add_user(message.content.as_deref().unwrap_or(""), &secret);
            }
            "assistant" => {
                if let Some(content) = message.content.as_deref() {
                    self.add_assistant_message(content);
                }
                for call in &message.tool_calls {
                    self.add_tool_call(call);
                }
            }
            "tool" => {
                let result = message
                    .content
                    .as_deref()
                    .and_then(|content| serde_json::from_str::<Value>(content).ok())
                    .unwrap_or_else(|| Value::String(message.content.clone().unwrap_or_default()));
                self.add_tool_result(
                    message.tool_call_id.as_deref().unwrap_or(""),
                    message.name.as_deref().unwrap_or("cmd"),
                    result,
                );
            }
            _ => {}
        }
    }

    fn add_user(&mut self, text: &str, secret: &str) {
        self.transcript
            .push(TranscriptItem::User(redact_secret(text, Some(secret))));
    }

    fn add_assistant(&mut self, text: &str) {
        if let Some(TranscriptItem::Assistant(current)) = self.transcript.last_mut() {
            current.push_str(text);
        } else {
            self.add_assistant_message(text);
        }
    }

    fn add_assistant_message(&mut self, text: &str) {
        self.transcript
            .push(TranscriptItem::Assistant(text.to_owned()));
    }

    fn add_tool_call(&mut self, call: &crate::model::ChatToolCall) {
        self.transcript.push(TranscriptItem::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
    }

    fn add_tool_result(&mut self, id: &str, name: &str, result: Value) {
        self.transcript.push(TranscriptItem::ToolResult {
            id: id.to_owned(),
            name: name.to_owned(),
            result,
        });
    }

    fn cursor_visible(&self) -> bool {
        !self.busy
            && (self.cursor_epoch.elapsed().as_millis() / CURSOR_BLINK_INTERVAL.as_millis())
                .is_multiple_of(2)
    }

    fn apply_event(&mut self, event: ProtocolEvent) {
        match event {
            ProtocolEvent::Session { .. } => {}
            ProtocolEvent::AssistantDelta { text } => self.add_assistant(&text),
            ProtocolEvent::ToolCall {
                id,
                name,
                arguments,
            } => self.add_tool_call(&crate::model::ChatToolCall {
                id,
                name,
                arguments,
            }),
            ProtocolEvent::ToolResult { id, name, result } => {
                self.add_tool_result(&id, &name, result)
            }
            ProtocolEvent::TurnEnd => {
                self.status = "finalizing".to_owned();
                self.transcript
                    .push(TranscriptItem::Info("✓ turn complete".to_owned()));
            }
            ProtocolEvent::TurnInterrupted { reason, phase } => {
                self.status = "cancelling".to_owned();
                self.transcript
                    .push(TranscriptItem::Info(format!("! {reason} ({phase})")));
            }
            ProtocolEvent::Error { message } => {
                self.status = "error".to_owned();
                self.transcript.push(TranscriptItem::Error(message));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TranscriptItem {
    User(String),
    Assistant(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        id: String,
        name: String,
        result: Value,
    },
    Error(String),
    Info(String),
}

fn max_scroll_for_area(state: &UiState, size: Size) -> u16 {
    let area = Rect::new(0, 0, size.width, size.height);
    let input_rows = input_visible_rows(state, area.width);
    let input_height = input_rows.clamp(1, MAX_INPUT_ROWS) + 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(input_height),
        ])
        .split(area);
    let lines = transcript_lines(state, chunks[0].width);
    lines
        .len()
        .saturating_sub(chunks[0].height as usize)
        .min(u16::MAX as usize) as u16
}

/// Number of wrapped rows the current input prompt occupies at `width`,
/// including the leading `> ` marker on the first row.
fn input_visible_rows(state: &UiState, width: u16) -> u16 {
    let width = width as usize;
    if width == 0 {
        return 1;
    }
    let prompt = input_prompt(&state.input);
    let wrapped = wrap_text(&prompt, width);
    wrapped.len().max(1) as u16
}

fn input_prompt(input: &str) -> String {
    format!("> {}", input)
}

fn draw(frame: &mut Frame<'_>, state: &UiState) {
    let area = frame.area();
    let input_rows = input_visible_rows(state, area.width).clamp(1, MAX_INPUT_ROWS);
    let input_height = input_rows + 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(input_height),
        ])
        .split(area);

    let width = chunks[0].width;
    let lines = transcript_lines(state, width);
    let available = chunks[0].height as usize;
    let max_scroll = lines.len().saturating_sub(available).min(u16::MAX as usize) as u16;
    let scroll = if state.auto_scroll {
        max_scroll
    } else {
        state.scroll.min(max_scroll)
    };
    let transcript = Paragraph::new(lines).scroll((scroll, 0));
    frame.render_widget(transcript, chunks[0]);

    let mode = if state.resumed { "resumed" } else { "new" };
    let status_text = format!(
        " session={} · {} · {} · Enter send · Shift/Alt+Enter newline · Esc cancel · Ctrl-C exit",
        state.session_id, mode, state.status
    );
    let status = Paragraph::new(redact_secret(&status_text, Some(&state.secret)));
    frame.render_widget(status, chunks[1]);

    let input_block = Block::default().borders(Borders::TOP | Borders::BOTTOM);
    let input_area = input_block.inner(chunks[2]);
    let prompt = redact_secret(&input_prompt(&state.input), Some(&state.secret));
    let wrapped = wrap_text(&prompt, input_area.width.max(1) as usize);
    let visible = (wrapped.len() as u16).clamp(1, input_rows);
    let input_scroll = (wrapped.len() as u16).saturating_sub(visible);
    let input_lines: Vec<Line<'static>> = wrapped
        .into_iter()
        .map(Line::raw)
        .collect();
    let input = Paragraph::new(input_lines.clone())
        .scroll((input_scroll, 0))
        .block(input_block);
    frame.render_widget(input, chunks[2]);
    // Ratatui shows the cursor when a frame requests a position and hides it
    // when this branch is skipped, which provides the blink phase.
    if state.cursor_visible() && !input_area.is_empty() && visible > 0 {
        // The cursor sits at the end of the input. The last visible row is the
        // row that holds the cursor when the input is scrolled to the bottom.
        let last_visible_idx = (input_scroll as usize) + (visible as usize) - 1;
        let last_row = input_lines
            .get(last_visible_idx)
            .map(|line| line.to_string())
            .unwrap_or_default();
        let cursor_offset = UnicodeWidthStr::width(last_row.as_str()) as u16;
        let cursor_x = input_area.x + cursor_offset.min(input_area.width.saturating_sub(1));
        let cursor_y = input_area.y + (visible - 1);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn transcript_lines(state: &UiState, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    // Track the previous rendered item so a ToolResult can be folded onto the
    // same logical line as its preceding ToolCall instead of producing a
    // separate transcript block.
    let mut prev: Option<&TranscriptItem> = None;
    for item in &state.transcript {
        let is_result_after_call = matches!(item, TranscriptItem::ToolResult { .. })
            && matches!(prev, Some(TranscriptItem::ToolCall { .. }));
        if !is_result_after_call && !lines.is_empty() {
            // One blank line between every pair of transcript items so user
            // messages, assistant messages, and tool calls/results stay
            // visually separated. A ToolResult that directly follows a
            // ToolCall is rendered on the same line and skips the separator.
            lines.push(Line::raw(String::new()));
        }
        match item {
            TranscriptItem::User(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_wrapped(&mut lines, &text, width, user_message_style());
            }
            TranscriptItem::Assistant(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_wrapped(&mut lines, &text, width, Style::default());
            }
            TranscriptItem::ToolCall {
                id: _,
                name,
                arguments,
            } => {
                let call_text = format!("[tool:{name} {}]", call_arguments(arguments));
                let call_text = redact_secret(&call_text, Some(&state.secret));
                push_spans_wrapped(
                    &mut lines,
                    &[(call_text, tool_call_style())],
                    width,
                );
            }
            TranscriptItem::ToolResult { id: _, name: _, result } => {
                let result_text = format_tool_result(result);
                let result_text = redact_secret(&result_text, Some(&state.secret));
                if is_result_after_call {
                    if let Some(last) = lines.last_mut() {
                        last.spans.push(Span::raw(" > "));
                        last.spans.push(Span::styled(result_text, tool_result_style()));
                    } else {
                        push_spans_wrapped(
                            &mut lines,
                            &[(result_text, tool_result_style())],
                            width,
                        );
                    }
                } else {
                    push_spans_wrapped(
                        &mut lines,
                        &[(result_text, tool_result_style())],
                        width,
                    );
                }
            }
            TranscriptItem::Error(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_wrapped(&mut lines, &text, width, error_style());
            }
            TranscriptItem::Info(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_wrapped(&mut lines, &text, width, info_style());
            }
        }
        prev = Some(item);
    }
    if lines.is_empty() {
        lines.push(Line::raw("type a message"));
    }
    lines
}

/// Render tool call arguments as the command string inside double quotes, for
/// example `"cat README.md"`. Malformed arguments fall back to the raw text.
fn call_arguments(arguments: &str) -> String {
    let parsed: Value = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => return arguments.to_owned(),
    };
    if let Some(command) = parsed.get("command").and_then(Value::as_str) {
        return format!("\"{command}\"");
    }
    serde_json::to_string(&parsed).unwrap_or_else(|_| arguments.to_owned())
}

/// Render a tool result as a single-line JSON-string-array literal containing
/// stdout (or stderr when stdout is empty). Newlines are escaped so the whole
/// result stays on one line. Output is truncated to `RESULT_PREVIEW_CHARS`.
fn format_tool_result(result: &Value) -> String {
    let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
    let output = if !stdout.is_empty() { stdout } else { stderr };
    let truncated = truncate_output(output);
    // Build a JSON string literal so newlines and quotes are escaped and the
    // result renders on a single line as `["..."]`.
    let json_string = serde_json::to_string(&truncated).unwrap_or_else(|_| "\"\"".to_owned());
    format!("[{json_string}]")
}

const RESULT_PREVIEW_CHARS: usize = 50;

fn truncate_output(output: &str) -> String {
    let mut result: String = output.chars().take(RESULT_PREVIEW_CHARS).collect();
    if output.chars().count() > RESULT_PREVIEW_CHARS {
        result.push('…');
    }
    result
}

fn user_message_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn tool_call_style() -> Style {
    Style::default().fg(Color::Magenta)
}

fn tool_result_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn error_style() -> Style {
    Style::default().fg(Color::Red)
}

fn info_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    let mut added = false;
    for piece in wrap_text(text, width) {
        lines.push(Line::styled(piece, style));
        added = true;
    }
    if !added {
        lines.push(Line::styled(String::new(), style));
    }
}

/// Push a logical line built from styled segments. When the rendered width
/// exceeds `width`, the whole line is character-wrapped; wrapped continuations
/// keep the style of the segment they fall on.
fn push_spans_wrapped(
    lines: &mut Vec<Line<'static>>,
    segments: &[(String, Style)],
    width: usize,
) {
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    for (text, style) in segments {
        for character in text.chars() {
            let char_width =
                unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            if current_width + char_width > width && !current_spans.is_empty() {
                lines.push(Line::from(std::mem::take(&mut current_spans)));
                current_width = 0;
            }
            let mut buffer = [0u8; 4];
            let s = character.encode_utf8(&mut buffer);
            current_spans.push(Span::styled(s.to_owned(), *style));
            current_width += char_width;
        }
    }
    if current_spans.is_empty() {
        current_spans.push(Span::raw(String::new()));
    }
    lines.push(Line::from(current_spans));
}

/// Wrap `text` into rows no wider than `width` display columns. Wrapping is
/// character-based so the row count matches exactly what a non-wrapping
/// `Paragraph` renderer draws, which keeps auto-scroll pinned to the true
/// bottom of the transcript regardless of terminal width. Empty lines are
/// preserved as empty rows.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(str::to_owned).collect();
    }
    let mut rows = Vec::new();
    for line in text.lines() {
        rows.extend(wrap_line(line, width));
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for character in line.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if current_width + char_width > width && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(character);
        current_width += char_width;
    }
    rows.push(current);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_replay_keeps_interruption_after_messages() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::user("hello".to_owned()),
            },
            SessionHistoryRecord::Interruption {
                timestamp: 2,
                reason: "user_cancelled".to_owned(),
                phase: "provider_stream".to_owned(),
                assistant_text: "partial".to_owned(),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
            },
        ];
        let state = UiState::from_history(&history, "provider-secret", "id", true);
        assert!(matches!(state.transcript[0], TranscriptItem::User(_)));
        assert!(matches!(state.transcript[1], TranscriptItem::Assistant(_)));
        assert!(matches!(state.transcript[2], TranscriptItem::Info(_)));
        let text = transcript_lines(&state, 80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("choices"));
    }

    #[test]
    fn history_replay_does_not_render_assistant_reasoning_details() {
        let mut message = ChatMessage::assistant("visible answer".to_owned(), Vec::new());
        message.reasoning_details = Some(vec![serde_json::json!({
            "type": "reasoning.text",
            "text": "private reasoning"
        })]);
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message,
        }];
        let state = UiState::from_history(&history, "provider-secret", "id", true);
        let text = transcript_lines(&state, 80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("visible answer"));
        assert!(!text.contains("private reasoning"));
        assert!(!text.contains("reasoning_details"));
    }

    #[test]
    fn history_replay_preserves_repeated_records() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant("same".to_owned(), Vec::new()),
            },
            SessionHistoryRecord::Interruption {
                timestamp: 2,
                reason: "user_cancelled".to_owned(),
                phase: "provider_stream".to_owned(),
                assistant_text: "same".to_owned(),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
            },
        ];
        let state = UiState::from_history(&history, "provider-secret", "id", true);
        assert_eq!(
            state
                .transcript
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Assistant(text) if text == "same"))
                .count(),
            2
        );
    }

    #[test]
    fn user_lines_use_a_bright_yellow_foreground_without_role_prefixes() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::user("hello".to_owned()),
        }];
        let state = UiState::from_history(&history, "provider-secret", "id", false);
        let lines = transcript_lines(&state, 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style.fg, Some(Color::Yellow));
        assert_eq!(lines[0].style.bg, None);
        assert_eq!(lines[0].to_string(), "hello");
    }

    #[test]
    fn transcript_rendering_redacts_history_content() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::assistant("provider-secret".to_owned(), Vec::new()),
        }];
        let state = UiState::from_history(&history, "provider-secret", "id", false);
        let text = transcript_lines(&state, 80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("provider-secret"));
    }

    #[test]
    fn mouse_wheel_disables_following_and_changes_scroll_offset() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::user("hello".to_owned()),
        }];
        let mut state = UiState::from_history(&history, "provider-secret", "id", false);
        handle_mouse_event(&mut state, MouseEventKind::ScrollUp, 10);
        assert!(!state.auto_scroll);
        assert_eq!(state.scroll, 7);
        handle_mouse_event(&mut state, MouseEventKind::ScrollDown, 10);
        assert_eq!(state.scroll, 10);
        assert!(!state.auto_scroll);
        state.scroll = 20;
        scroll_up(&mut state, 10);
        assert_eq!(state.scroll, 7);
    }

    #[test]
    fn wrap_text_breaks_long_lines_and_preserves_empty_lines() {
        let rows = wrap_text("12345\n\nabc", 3);
        assert_eq!(rows, vec!["123", "45", "", "abc"]);
    }

    #[test]
    fn wrap_line_never_returns_an_empty_vec() {
        assert_eq!(wrap_line("", 5), vec![""]);
        assert_eq!(wrap_line("abc", 5), vec!["abc"]);
    }

    #[test]
    fn completion_event_does_not_release_input_before_worker_finishes() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::user("hello".to_owned()),
        }];
        let mut state = UiState::from_history(&history, "provider-secret", "id", false);
        state.busy = true;
        state.active_cancel = Some(CancellationToken::new());
        state.apply_event(ProtocolEvent::TurnEnd);
        assert!(state.busy);
        assert!(state.active_cancel.is_some());
        assert_eq!(state.status, "finalizing");
    }

    #[test]
    fn transcript_inserts_a_blank_line_between_items() {
        let history = [
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::user("hi".to_owned()),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::assistant("hello".to_owned(), Vec::new()),
            },
        ];
        let state = UiState::from_history(&history, "secret", "id", false);
        let lines = transcript_lines(&state, 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].to_string(), "hi");
        assert_eq!(lines[1].to_string(), "");
        assert_eq!(lines[2].to_string(), "hello");
    }

    #[test]
    fn tool_call_renders_as_compact_single_line_with_command() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::assistant(
                String::new(),
                vec![crate::model::ChatToolCall {
                    id: "call-1".to_owned(),
                    name: "cmd".to_owned(),
                    arguments: r#"{"command":"pwd"}"#.to_owned(),
                }],
            ),
        }];
        let state = UiState::from_history(&history, "secret", "id", false);
        let text = transcript_lines(&state, 80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[tool:cmd \"pwd\"]"));
        // The raw JSON arguments must not appear verbatim.
        assert!(!text.contains("{\"command\":\"pwd\"}"));
    }

    #[test]
    fn tool_call_and_result_render_on_one_line_with_truncated_stdout() {
        let long_stdout = "a".repeat(80);
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-1".to_owned(),
                        name: "cmd".to_owned(),
                        arguments: r#"{"command":"cat README.md"}"#.to_owned(),
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-1".to_owned(),
                    "cmd".to_owned(),
                    serde_json::json!({
                        "command": "cat README.md",
                        "exit_code": 0,
                        "stdout": long_stdout,
                        "stderr": "",
                    })
                    .to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "id", false);
        let lines = transcript_lines(&state, 200);
        // Call and result share one logical line (no blank line between them).
        assert_eq!(lines.len(), 1);
        let text = lines[0].to_string();
        assert!(text.starts_with("[tool:cmd \"cat README.md\"] > ["));
        // stdout is truncated to 50 chars plus the ellipsis.
        assert!(text.contains(&"a".repeat(50)));
        assert!(text.contains('…'));
        assert!(!text.contains(&"a".repeat(51)));
    }

    #[test]
    fn tool_result_falls_back_to_stderr_when_stdout_is_empty() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-1".to_owned(),
                        name: "cmd".to_owned(),
                        arguments: r#"{"command":"bad"}"#.to_owned(),
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-1".to_owned(),
                    "cmd".to_owned(),
                    serde_json::json!({
                        "command": "bad",
                        "exit_code": 127,
                        "stdout": "",
                        "stderr": "not found",
                    })
                    .to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "id", false);
        let text = transcript_lines(&state, 200)[0].to_string();
        assert!(text.contains("not found"));
        assert!(text.contains(" > "));
    }

    #[test]
    fn tool_call_and_result_styles_use_foreground_colors() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-1".to_owned(),
                        name: "cmd".to_owned(),
                        arguments: r#"{"command":"pwd"}"#.to_owned(),
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-1".to_owned(),
                    "cmd".to_owned(),
                    serde_json::json!({"stdout":"x","stderr":""}).to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "id", false);
        let lines = transcript_lines(&state, 200);
        let spans = &lines[0].spans;
        // Call text is split per-character into Magenta spans; the separator
        // " > " is a single default-color span; the result is one DarkGray span.
        assert_eq!(spans[0].style.fg, Some(Color::Magenta));
        let separator = spans.iter().find(|span| span.content == " > ").expect("separator");
        assert_eq!(separator.style.fg, None);
        let result_span = spans.last().expect("result span");
        assert_eq!(result_span.style.fg, Some(Color::DarkGray));
        assert_eq!(result_span.content, "[\"x\"]");
    }

    #[test]
    fn input_prompt_wraps_to_multiple_rows_when_long() {
        let mut state = UiState::from_history(&[], "secret", "id", false);
        state.input = "abcdefghij".to_owned();
        // width 5: "> abcdefghij" wraps to "> abc", "defg", "hij".
        let rows = input_visible_rows(&state, 5);
        assert!(rows >= 3);
    }
}
