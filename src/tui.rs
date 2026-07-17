use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::prelude::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
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
const MAX_DISPLAY_RESULT_CHARS: usize = 8 * 1024;
const MAX_DISPLAY_INPUT_CHARS: usize = 16 * 1024;
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
    if let Err(error) = execute!(
        terminal_guard.terminal_mut().backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture,
        Hide
    ) {
        return Err(format!("unable to enter terminal UI: {error}"));
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
                let max_scroll = max_scroll_for_area(
                    state,
                    terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?,
                );
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
                    let max_scroll = max_scroll_for_area(
                        state,
                        terminal
                            .size()
                            .map_err(|error| format!("unable to read terminal size: {error}"))?,
                    );
                    scroll_up(state, max_scroll);
                }
                KeyCode::Down | KeyCode::PageDown => {
                    let max_scroll = max_scroll_for_area(
                        state,
                        terminal
                            .size()
                            .map_err(|error| format!("unable to read terminal size: {error}"))?,
                    );
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
}

impl<W: Write> TerminalGuard<W> {
    fn new(terminal: Terminal<CrosstermBackend<W>>) -> Self {
        Self {
            terminal: Some(terminal),
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);
    let lines = transcript_lines(state);
    let visual_lines = visual_line_count(&lines, chunks[0].width);
    visual_lines
        .saturating_sub(chunks[0].height as usize)
        .min(u16::MAX as usize) as u16
}

fn visual_line_count(lines: &[Line<'static>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    let width = width as usize;
    lines.iter().fold(0, |total, line| {
        let line_width = line.width().max(1);
        let rows = line_width.saturating_add(width - 1) / width;
        total.saturating_add(rows)
    })
}

fn draw(frame: &mut Frame<'_>, state: &UiState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);

    let lines = transcript_lines(state);
    let available = chunks[0].height as usize;
    let visual_lines = visual_line_count(&lines, chunks[0].width);
    let max_scroll = visual_lines
        .saturating_sub(available)
        .min(u16::MAX as usize) as u16;
    let scroll = if state.auto_scroll {
        max_scroll
    } else {
        state.scroll.min(max_scroll)
    };
    let transcript = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(transcript, chunks[0]);

    let mode = if state.resumed { "resumed" } else { "new" };
    let status_text = format!(
        " session={} · {} · {} · Enter send · Esc cancel · Ctrl-C exit",
        state.session_id, mode, state.status
    );
    let status = Paragraph::new(redact_secret(&status_text, Some(&state.secret)));
    frame.render_widget(status, chunks[1]);

    let input_text = format!("> {}", state.input);
    let safe_input = redact_secret(&input_text, Some(&state.secret));
    let input_block = Block::default().borders(Borders::TOP | Borders::BOTTOM);
    let input_area = input_block.inner(chunks[2]);
    let input = Paragraph::new(safe_input.clone()).block(input_block);
    frame.render_widget(input, chunks[2]);
    // Ratatui shows the cursor when a frame requests a position and hides it
    // when this branch is skipped, which provides the blink phase.
    if state.cursor_visible() && !input_area.is_empty() {
        let cursor_offset = UnicodeWidthStr::width(safe_input.as_str()) as u16;
        let cursor_x = input_area.x + cursor_offset.min(input_area.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, input_area.y));
    }
}

fn transcript_lines(state: &UiState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in &state.transcript {
        match item {
            TranscriptItem::User(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_message_lines(&mut lines, &text, user_message_style());
            }
            TranscriptItem::Assistant(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_message_lines(&mut lines, &text, Style::default());
            }
            TranscriptItem::ToolCall {
                id,
                name,
                arguments,
            } => {
                let text =
                    redact_secret(&format!("{name} [{id}] {arguments}"), Some(&state.secret));
                push_labeled_lines(&mut lines, "tool", &text);
            }
            TranscriptItem::ToolResult { id, name, result } => {
                let text = serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_owned());
                let text = redact_secret(
                    &format!("{name} [{id}]\n{}", bounded_display(&text)),
                    Some(&state.secret),
                );
                push_labeled_lines(&mut lines, "result", &text);
            }
            TranscriptItem::Error(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_labeled_lines(&mut lines, "error", &text);
            }
            TranscriptItem::Info(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_labeled_lines(&mut lines, "status", &text);
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::raw("type a message"));
    }
    lines
}

fn user_message_style() -> Style {
    Style::default().bg(Color::Rgb(96, 86, 42))
}

fn push_message_lines(lines: &mut Vec<Line<'static>>, text: &str, style: Style) {
    let mut added = false;
    for line in text.lines() {
        lines.push(Line::styled(line.to_owned(), style));
        added = true;
    }
    if !added {
        lines.push(Line::styled(String::new(), style));
    }
}

fn push_labeled_lines(lines: &mut Vec<Line<'static>>, label: &str, text: &str) {
    let mut first = true;
    for line in text.lines() {
        let content = if first {
            format!("{label}> {line}")
        } else {
            format!("       {line}")
        };
        lines.push(Line::raw(content));
        first = false;
    }
    if first {
        lines.push(Line::raw(format!("{label}>")));
    }
}

fn bounded_display(text: &str) -> String {
    let mut result = text
        .chars()
        .take(MAX_DISPLAY_RESULT_CHARS)
        .collect::<String>();
    if text.chars().count() > MAX_DISPLAY_RESULT_CHARS {
        result.push('…');
    }
    result
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
        let text = transcript_lines(&state)
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
        let text = transcript_lines(&state)
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
    fn user_lines_use_a_muted_yellow_background_without_role_prefixes() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::user("hello".to_owned()),
        }];
        let state = UiState::from_history(&history, "provider-secret", "id", false);
        let lines = transcript_lines(&state);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style.bg, Some(Color::Rgb(96, 86, 42)));
        assert_eq!(lines[0].to_string(), "hello");
    }

    #[test]
    fn transcript_rendering_redacts_history_content() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::assistant("provider-secret".to_owned(), Vec::new()),
        }];
        let state = UiState::from_history(&history, "provider-secret", "id", false);
        let text = transcript_lines(&state)
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
    fn visual_line_count_accounts_for_wrapped_lines_and_empty_lines() {
        let lines = vec![Line::raw("12345"), Line::raw("")];
        assert_eq!(visual_line_count(&lines, 3), 3);
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
}
