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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect, Size};
use ratatui::prelude::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Terminal;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::app::Harness;
use crate::cancellation::CancellationToken;
use crate::model::{estimate_context_tokens, ChatMessage};
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
const WELCOME_MESSAGE: &str = concat!("Coding Agent Harness LUCY - v", env!("CARGO_PKG_VERSION"),);
const WELCOME_TAGLINE: &str = "An ultra-thin harness for tomorrow's most powerful models";
const WELCOME_START_COLOR: (u8, u8, u8) = (0, 180, 180);
const WELCOME_END_COLOR: (u8, u8, u8) = (255, 215, 0);
const USER_BORDER_COLOR: Color = Color::Rgb(192, 154, 0);
const PENDING_TOOL_COLOR: Color = Color::Rgb(255, 165, 0);
const WAITING_INPUT_MESSAGE: &str = "The agent is working. Please wait until it finishes.";
const BUSY_INPUT_COLOR: Color = Color::DarkGray;
const SKILL_PICKER_MAX_ROWS: usize = 5;

pub(crate) fn run<W: Write>(mut harness: Harness, resumed: bool, stdout: W) -> Result<(), String> {
    let secret = harness.provider.api_key().to_owned();
    let context_window = harness
        .context_window
        .or_else(|| harness.provider.context_window());
    harness.context_window = context_window;
    let context_tokens = estimate_context_tokens(&harness.session.provider_messages());
    let skill_names = harness
        .session
        .skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    let mut state = UiState::from_history(
        &harness.session.history,
        &secret,
        &harness.session.llm.model,
        harness.session.llm.effort.as_deref(),
        resumed,
    )
    .with_attached_agents(harness.attached_agents.clone())
    .with_skill_names(skill_names)
    .with_context(context_window, context_tokens);
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
    if let Err(error) = execute!(backend, EnterAlternateScreen, EnableMouseCapture, Hide) {
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
                Ok(WorkerMessage::Thinking) => state.show_thinking(),
                Ok(WorkerMessage::SkillInstructionAttached) => {
                    state.mark_latest_user_skill_attached()
                }
                Ok(WorkerMessage::ContextUsage(tokens)) => state.context_tokens = tokens,
                Ok(WorkerMessage::CompactionStarted) => state.status = "compacting".to_owned(),
                Ok(WorkerMessage::CompactionFinished {
                    tokens_before,
                    tokens_after,
                }) => {
                    state.context_tokens = tokens_after;
                    state.status = "working".to_owned();
                    state.transcript.push(TranscriptItem::Info(format!(
                        "↻ context compacted ({} → {})",
                        format_context_tokens(tokens_before),
                        format_context_tokens(tokens_after)
                    )));
                }
                Ok(WorkerMessage::Finished) => {
                    release_finished_turn(terminal.backend_mut(), state);
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
                            insert_at_cursor(&mut state.input, &mut state.cursor, '\n');
                            state.input_changed();
                        }
                        continue;
                    }
                    if state.select_focused_skill() {
                        continue;
                    }
                    let text = std::mem::take(&mut state.input);
                    state.cursor = 0;
                    state.reset_skill_picker();
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
                    state.status = "working".to_owned();
                    requests
                        .send(WorkerRequest::Turn { text, cancel })
                        .map_err(|_| "TUI worker is unavailable".to_owned())?;
                }
                KeyCode::Char(character) => {
                    if state.input.chars().count() < MAX_DISPLAY_INPUT_CHARS {
                        insert_at_cursor(&mut state.input, &mut state.cursor, character);
                        state.input_changed();
                    }
                }
                KeyCode::Backspace => {
                    if remove_before_cursor(&mut state.input, &mut state.cursor) {
                        state.input_changed();
                    }
                }
                KeyCode::Left => {
                    state.cursor = state.cursor.saturating_sub(1);
                    state.cursor_epoch = Instant::now();
                }
                KeyCode::Right => {
                    state.cursor = (state.cursor + 1).min(state.input.chars().count());
                    state.cursor_epoch = Instant::now();
                }
                KeyCode::Home => {
                    state.cursor = 0;
                    state.cursor_epoch = Instant::now();
                }
                KeyCode::End => {
                    state.cursor = state.input.chars().count();
                    state.cursor_epoch = Instant::now();
                }
                KeyCode::Up => {
                    if !state.move_skill_picker(false) {
                        let size = terminal
                            .size()
                            .map_err(|error| format!("unable to read terminal size: {error}"))?;
                        let max_scroll = max_scroll_for_area(state, size);
                        scroll_up(state, max_scroll);
                    }
                }
                KeyCode::Down => {
                    if !state.move_skill_picker(true) {
                        let size = terminal
                            .size()
                            .map_err(|error| format!("unable to read terminal size: {error}"))?;
                        let max_scroll = max_scroll_for_area(state, size);
                        scroll_down(state, max_scroll);
                    }
                }
                KeyCode::PageUp => {
                    let size = terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?;
                    let max_scroll = max_scroll_for_area(state, size);
                    scroll_up(state, max_scroll);
                }
                KeyCode::PageDown => {
                    let size = terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?;
                    let max_scroll = max_scroll_for_area(state, size);
                    scroll_down(state, max_scroll);
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
    if state.scroll == max_scroll {
        // Reaching the real bottom is an explicit request to resume following
        // the transcript, so subsequent streamed output stays visible.
        state.auto_scroll = true;
        state.scroll = 0;
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnNotification {
    Completed,
    Interrupted,
    Failed,
}

impl TurnNotification {
    fn body(self) -> &'static str {
        match self {
            Self::Completed => "Turn complete",
            Self::Interrupted => "Turn interrupted",
            Self::Failed => "Turn failed",
        }
    }
}

fn turn_notification_for_status(status: &str) -> TurnNotification {
    match status {
        "cancelling" | "사용자 중단" => TurnNotification::Interrupted,
        "error" => TurnNotification::Failed,
        _ => TurnNotification::Completed,
    }
}

/// Ask terminal emulators that support OSC 777 to show a desktop notification.
///
/// The title and body are fixed Lucy-owned strings rather than model/provider
/// text, so completion notifications cannot inject terminal control data or
/// expose a secret. Terminals without OSC 777 support safely ignore the OSC.
fn send_turn_notification<W: Write>(
    writer: &mut W,
    notification: TurnNotification,
) -> io::Result<()> {
    writer.write_all(b"\x1b]777;notify;Lucy;")?;
    writer.write_all(notification.body().as_bytes())?;
    writer.write_all(b"\x07")?;
    writer.flush()
}

fn release_finished_turn<W: Write>(writer: &mut W, state: &mut UiState) {
    let was_busy = state.busy;
    let notification = turn_notification_for_status(&state.status);
    state.busy = false;
    state.active_cancel = None;
    if was_busy {
        // Notification failure must never change the completed turn result or
        // make the TUI unusable.
        let _ = send_turn_notification(writer, notification);
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
    Thinking,
    SkillInstructionAttached,
    ContextUsage(usize),
    CompactionStarted,
    CompactionFinished {
        tokens_before: usize,
        tokens_after: usize,
    },
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

    fn reasoning_started(&mut self) -> io::Result<()> {
        self.sender
            .send(WorkerMessage::Thinking)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI closed"))
    }

    fn skill_instruction_attached(&mut self, _name: &str) -> io::Result<()> {
        self.sender
            .send(WorkerMessage::SkillInstructionAttached)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI closed"))
    }

    fn context_usage(&mut self, tokens: usize) -> io::Result<()> {
        self.sender
            .send(WorkerMessage::ContextUsage(tokens))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI closed"))
    }

    fn compaction_started(&mut self) -> io::Result<()> {
        self.sender
            .send(WorkerMessage::CompactionStarted)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI closed"))
    }

    fn compaction_finished(&mut self, tokens_before: usize, tokens_after: usize) -> io::Result<()> {
        self.sender
            .send(WorkerMessage::CompactionFinished {
                tokens_before,
                tokens_after,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI closed"))
    }
}

struct UiState {
    model: String,
    effort: Option<String>,
    context_window: Option<usize>,
    context_tokens: usize,
    secret: String,
    transcript: Vec<TranscriptItem>,
    input: String,
    cursor: usize,
    status: String,
    busy: bool,
    active_cancel: Option<CancellationToken>,
    scroll: u16,
    auto_scroll: bool,
    cursor_epoch: Instant,
    welcome_visible: bool,
    attached_agents: Vec<String>,
    skill_names: Vec<String>,
    skill_picker_focus: usize,
    skill_picker_suppressed: bool,
}

impl UiState {
    fn from_history(
        history: &[SessionHistoryRecord],
        secret: &str,
        model: &str,
        effort: Option<&str>,
        resumed: bool,
    ) -> Self {
        let mut state = Self {
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
            context_window: None,
            context_tokens: 1,
            secret: secret.to_owned(),
            transcript: Vec::new(),
            input: String::new(),
            cursor: 0,
            status: "ready".to_owned(),
            busy: false,
            active_cancel: None,
            scroll: 0,
            auto_scroll: true,
            cursor_epoch: Instant::now(),
            welcome_visible: !resumed && history.is_empty(),
            attached_agents: Vec::new(),
            skill_names: Vec::new(),
            skill_picker_focus: 0,
            skill_picker_suppressed: false,
        };
        for record in history {
            state.add_history_record(record);
        }
        state
    }

    fn with_attached_agents(mut self, attached_agents: Vec<String>) -> Self {
        self.attached_agents = attached_agents;
        self
    }

    fn with_skill_names(mut self, skill_names: Vec<String>) -> Self {
        self.skill_names = skill_names;
        self
    }

    fn with_context(mut self, context_window: Option<usize>, context_tokens: usize) -> Self {
        self.context_window = context_window;
        self.context_tokens = context_tokens.max(1);
        self
    }

    /// Return matching skills only while the first input character is `/` and
    /// the user is still writing the command name (rather than its arguments).
    fn matching_skill_names(&self) -> Vec<&str> {
        matching_skill_names(&self.input, &self.skill_names)
    }

    fn reset_skill_picker(&mut self) {
        self.skill_picker_focus = 0;
        self.skill_picker_suppressed = false;
    }

    fn skill_picker_visible(&self) -> bool {
        !self.skill_picker_suppressed && !self.matching_skill_names().is_empty()
    }

    fn input_changed(&mut self) {
        self.reset_skill_picker();
        self.cursor_epoch = Instant::now();
    }

    /// Move through the current filter result without wrapping at its ends.
    /// Returning false lets the caller retain normal transcript scrolling when
    /// no slash picker is active.
    fn move_skill_picker(&mut self, down: bool) -> bool {
        let match_count = self.matching_skill_names().len();
        if self.skill_picker_suppressed || match_count == 0 {
            return false;
        }
        if down {
            self.skill_picker_focus = (self.skill_picker_focus + 1).min(match_count - 1);
        } else {
            self.skill_picker_focus = self.skill_picker_focus.saturating_sub(1);
        }
        true
    }

    /// Replace the slash query with the focused explicit skill command. The
    /// normal Enter path then sends that command and the existing turn engine
    /// attaches the immutable session skill snapshot.
    fn select_focused_skill(&mut self) -> bool {
        if self.skill_picker_suppressed {
            return false;
        }
        let Some(name) = self
            .matching_skill_names()
            .get(self.skill_picker_focus)
            .map(|name| (*name).to_owned())
        else {
            return false;
        };
        self.input = format!("/{name}");
        self.cursor = self.input.chars().count();
        // The first Enter chooses a skill; a second Enter sends the completed
        // command to the normal attachment path.
        self.skill_picker_suppressed = true;
        self.cursor_epoch = Instant::now();
        true
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
            SessionHistoryRecord::Compaction(compaction) => {
                self.transcript.push(TranscriptItem::Info(format!(
                    "↻ context compacted ({} before)",
                    format_context_tokens(compaction.tokens_before)
                )));
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
        self.welcome_visible = false;
        self.transcript.push(TranscriptItem::User {
            text: redact_secret(text, Some(secret)),
            skill_instruction_attached: false,
        });
    }

    fn mark_latest_user_skill_attached(&mut self) {
        if let Some(TranscriptItem::User {
            skill_instruction_attached,
            ..
        }) = self.transcript.last_mut()
        {
            *skill_instruction_attached = true;
        }
    }

    fn clear_thinking(&mut self) {
        if matches!(self.transcript.last(), Some(TranscriptItem::Thinking)) {
            self.transcript.pop();
        }
    }

    fn show_thinking(&mut self) {
        self.status = "working".to_owned();
        if !matches!(self.transcript.last(), Some(TranscriptItem::Thinking)) {
            self.transcript.push(TranscriptItem::Thinking);
        }
    }

    fn add_assistant(&mut self, text: &str) {
        self.clear_thinking();
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
        self.clear_thinking();
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
                self.clear_thinking();
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
    User {
        text: String,
        skill_instruction_attached: bool,
    },
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
    Thinking,
}

fn ui_layout(state: &UiState, area: Rect) -> (Rect, Option<Rect>, Rect, Rect) {
    let input_rows = input_visible_rows(state, area.width.saturating_sub(2));
    let input_height = input_rows.clamp(1, MAX_INPUT_ROWS) + 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);
    let picker_height = skill_picker_height(state);
    let picker_area = (picker_height > 0).then(|| {
        Rect::new(
            chunks[1].x,
            chunks[1].y.saturating_sub(picker_height),
            chunks[1].width,
            picker_height,
        )
    });
    (chunks[0], picker_area, chunks[1], chunks[2])
}

fn max_scroll_for_area(state: &UiState, size: Size) -> u16 {
    let area = Rect::new(0, 0, size.width, size.height);
    let (chat_chunk, picker_area, _, _) = ui_layout(state, area);
    let chat_height = chat_chunk.height.saturating_sub(1);
    let chat_area = Rect::new(chat_chunk.x, chat_chunk.y, chat_chunk.width, chat_height);
    let visible_chat_height = visible_chat_height(chat_area, picker_area);
    let lines = transcript_lines(state, chat_chunk.width);
    lines
        .len()
        .saturating_sub(visible_chat_height as usize)
        .min(u16::MAX as usize) as u16
}

/// Return the transcript rows that remain visible after the picker overlays the
/// bottom of the chat viewport. The activity row is excluded by the caller;
/// the picker intentionally covers that row as well.
fn visible_chat_height(chat_area: Rect, picker_area: Option<Rect>) -> u16 {
    let Some(picker_area) = picker_area else {
        return chat_area.height;
    };
    let overlap_start = chat_area.y.max(picker_area.y);
    let overlap_end = chat_area
        .y
        .saturating_add(chat_area.height)
        .min(picker_area.y.saturating_add(picker_area.height));
    chat_area
        .height
        .saturating_sub(overlap_end.saturating_sub(overlap_start))
}

/// Number of wrapped rows the current input occupies at `width`.
fn input_visible_rows(state: &UiState, width: u16) -> u16 {
    let width = width as usize;
    if width == 0 {
        return 1;
    }
    let prompt = input_display_text(state);
    let wrapped = wrap_text(&prompt, width);
    wrapped.len().max(1) as u16
}

fn input_prompt(input: &str) -> String {
    input.to_owned()
}

fn input_display_text(state: &UiState) -> String {
    if state.busy {
        WAITING_INPUT_MESSAGE.to_owned()
    } else {
        redact_secret(&input_prompt(&state.input), Some(&state.secret))
    }
}

/// The slash picker only accepts a command at the beginning of the message.
/// Once whitespace starts arguments, normal message entry resumes.
fn matching_skill_names<'a>(input: &str, skill_names: &'a [String]) -> Vec<&'a str> {
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    skill_names
        .iter()
        .map(String::as_str)
        .filter(|name| name.starts_with(query))
        .collect()
}

fn skill_picker_height(state: &UiState) -> u16 {
    if state.skill_picker_visible() {
        (state
            .matching_skill_names()
            .len()
            .min(SKILL_PICKER_MAX_ROWS)
            + 1) as u16
    } else {
        0
    }
}

fn skill_picker_range(total: usize, focus: usize) -> std::ops::Range<usize> {
    let start = focus
        .saturating_add(1)
        .saturating_sub(SKILL_PICKER_MAX_ROWS);
    start.min(total)..(start + SKILL_PICKER_MAX_ROWS).min(total)
}

/// Return the command portion of a currently valid explicit skill invocation.
/// This mirrors the command grammar used by the turn engine, while keeping the
/// styling concern local to the TUI.
fn active_skill_trigger<'a>(input: &'a str, skill_names: &[String]) -> Option<&'a str> {
    let invocation = input.strip_prefix('/')?;
    let name = invocation
        .split_once(char::is_whitespace)
        .map_or(invocation, |(name, _)| name);
    if name.is_empty() || !skill_names.iter().any(|skill_name| skill_name == name) {
        return None;
    }
    Some(&input[..1 + name.len()])
}

/// Preserve input wrapping while styling a recognized `/<name>` prefix
/// independently from any arguments the user is still entering.
fn styled_text_lines(
    input: &str,
    active_skill_trigger: Option<&str>,
    width: usize,
    text_style: Style,
) -> Vec<Line<'static>> {
    let trigger_len = active_skill_trigger.map_or(0, |trigger| trigger.chars().count());
    let mut char_offset = 0usize;
    let mut lines = Vec::new();

    for source_line in input.split('\n') {
        for row in wrap_line(source_line, width) {
            let mut spans = Vec::new();
            let mut text = String::new();
            let mut highlighted = None;
            for character in row.chars() {
                let should_highlight = char_offset < trigger_len;
                if highlighted != Some(should_highlight) && !text.is_empty() {
                    spans.push(styled_text_span(
                        std::mem::take(&mut text),
                        highlighted.unwrap_or(false),
                        text_style,
                    ));
                }
                highlighted = Some(should_highlight);
                text.push(character);
                char_offset += 1;
            }
            if !text.is_empty() {
                spans.push(styled_text_span(
                    text,
                    highlighted.unwrap_or(false),
                    text_style,
                ));
            }
            if spans.is_empty() {
                spans.push(Span::styled(String::new(), text_style));
            }
            lines.push(Line::from(spans));
        }
        // `split` retains empty trailing lines; account for the newline that
        // separated this source line from the next one in the character index.
        char_offset += 1;
    }

    lines
}

fn styled_text_span(text: String, highlighted: bool, text_style: Style) -> Span<'static> {
    if highlighted {
        Span::styled(text, Style::default().fg(Color::Cyan))
    } else {
        Span::styled(text, text_style)
    }
}

fn cursor_row(input: &str, cursor: usize, width: usize) -> u16 {
    let prefix: String = input.chars().take(cursor).collect();
    wrap_text(&prefix, width)
        .len()
        .saturating_sub(1)
        .min(u16::MAX as usize) as u16
}

fn insert_at_cursor(input: &mut String, cursor: &mut usize, character: char) {
    let byte_index = input
        .char_indices()
        .nth(*cursor)
        .map_or(input.len(), |(index, _)| index);
    input.insert(byte_index, character);
    *cursor += 1;
}

fn remove_before_cursor(input: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 {
        return false;
    }
    let end = input
        .char_indices()
        .nth(*cursor)
        .map_or(input.len(), |(index, _)| index);
    let start = input
        .char_indices()
        .nth(*cursor - 1)
        .map(|(index, _)| index)
        .unwrap_or(0);
    input.replace_range(start..end, "");
    *cursor -= 1;
    true
}

fn draw(frame: &mut Frame<'_>, state: &UiState) {
    let (chat_chunk, picker_area, input_chunk, status_area) = ui_layout(state, frame.area());

    // Activity is conversation state, not terminal help text. Keep it in the
    // chat viewport so the picker can intentionally overlay its ready state.
    let chat_height = chat_chunk.height.saturating_sub(1);
    let chat_area = Rect::new(chat_chunk.x, chat_chunk.y, chat_chunk.width, chat_height);
    let visible_chat_height = visible_chat_height(chat_area, picker_area);
    let visible_chat_area = Rect::new(
        chat_area.x,
        chat_area.y,
        chat_area.width,
        visible_chat_height,
    );
    let activity_area = Rect::new(
        chat_chunk.x,
        chat_chunk.y + chat_height,
        chat_chunk.width,
        chat_chunk.height - chat_height,
    );

    let width = chat_chunk.width;
    if state.welcome_visible {
        let welcome_lines = welcome_lines(&state.attached_agents);
        let welcome_height = (welcome_lines.len() as u16).min(visible_chat_area.height);
        let welcome_area = Rect::new(
            visible_chat_area.x,
            visible_chat_area.y + visible_chat_area.height.saturating_sub(welcome_height) / 2,
            visible_chat_area.width,
            welcome_height,
        );
        let welcome = Paragraph::new(welcome_lines).alignment(Alignment::Center);
        frame.render_widget(welcome, welcome_area);
    } else {
        let lines = transcript_lines(state, width);
        let available = visible_chat_area.height as usize;
        let max_scroll = lines.len().saturating_sub(available).min(u16::MAX as usize) as u16;
        let scroll = if state.auto_scroll {
            max_scroll
        } else {
            state.scroll.min(max_scroll)
        };
        let transcript = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(transcript, visible_chat_area);
    }

    let activity_text = if state.status == "working" {
        format!("{} working", spinner_frame(state))
    } else if state.status == "compacting" {
        format!("{} compacting", spinner_frame(state))
    } else {
        format!("● {}", state.status)
    };
    let activity = Line::styled(activity_text, activity_style_for(state));
    frame.render_widget(Paragraph::new(activity), activity_area);

    if let Some(picker_area) = picker_area {
        draw_skill_picker(frame, state, picker_area);
    }

    let input_style = if state.busy {
        waiting_input_style()
    } else {
        user_message_style()
    };
    let input_text_style = if state.busy {
        waiting_input_style()
    } else {
        Style::default().fg(Color::White)
    };
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(input_style);
    let input_area = input_block.inner(input_chunk);
    let prompt = input_display_text(state);
    let input_rows =
        input_visible_rows(state, frame.area().width.saturating_sub(2)).clamp(1, MAX_INPUT_ROWS);
    let wrapped = wrap_text(&prompt, input_area.width.max(1) as usize);
    let visible = (wrapped.len() as u16).clamp(1, input_rows);
    let cursor_row = cursor_row(&prompt, state.cursor, input_area.width.max(1) as usize);
    let bottom_scroll = (wrapped.len() as u16).saturating_sub(visible);
    let cursor_scroll = (cursor_row + 1).saturating_sub(visible);
    let input_scroll = bottom_scroll.min(cursor_scroll);
    let active_skill_trigger = (!state.busy)
        .then(|| active_skill_trigger(&prompt, &state.skill_names))
        .flatten();
    let input_lines = styled_text_lines(
        &prompt,
        active_skill_trigger,
        input_area.width.max(1) as usize,
        input_text_style,
    );
    let input = Paragraph::new(input_lines)
        .style(input_text_style)
        .scroll((input_scroll, 0))
        .block(input_block);
    frame.render_widget(input, input_chunk);

    let effort = state.effort.as_deref().unwrap_or("default");
    let status_text = format!("model={} · effort={effort}", state.model);
    let context_text = context_status_text(state);
    let context_width = UnicodeWidthStr::width(context_text.as_str()) as u16;
    let context_area_width = context_width.min(status_area.width.saturating_sub(1));
    let status_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(context_area_width)])
        .split(status_area);
    let status = Paragraph::new(redact_secret(&status_text, Some(&state.secret)))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, status_chunks[0]);
    if context_area_width > 0 {
        let context = Paragraph::new(context_text)
            .alignment(Alignment::Right)
            .style(context_status_style(state));
        frame.render_widget(context, status_chunks[1]);
    }
    // Ratatui shows the cursor when a frame requests a position and hides it
    // when this branch is skipped, which provides the blink phase.
    if state.cursor_visible() && !input_area.is_empty() && visible > 0 {
        let cursor_prefix: String = prompt.chars().take(state.cursor).collect();
        let cursor_rows = wrap_text(&cursor_prefix, input_area.width.max(1) as usize);
        let cursor_line = cursor_rows.last().map(String::as_str).unwrap_or("");
        let cursor_offset = UnicodeWidthStr::width(cursor_line) as u16;
        let cursor_x = input_area.x + cursor_offset.min(input_area.width.saturating_sub(1));
        let cursor_y = input_area.y + cursor_row.saturating_sub(input_scroll);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_skill_picker(frame: &mut Frame<'_>, state: &UiState, area: Rect) {
    let matches = state.matching_skill_names();
    let total = matches.len();
    if total == 0 || area.is_empty() {
        return;
    }
    // The picker intentionally overlays the activity row, including `ready`.
    // Clear the full row first so a short skill name cannot leave stale status
    // characters visible after the picker has been rendered.
    frame.render_widget(Clear, area);
    let focus = state.skill_picker_focus.min(total - 1);
    let header = Line::styled(
        format!("[{}/{}]", focus + 1, total),
        Style::default().fg(Color::DarkGray),
    );
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, 1),
    );

    for (row, index) in skill_picker_range(total, focus).enumerate() {
        let style = if index == focus {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let skill = Line::styled(format!("/{}", matches[index]), style);
        frame.render_widget(
            Paragraph::new(skill),
            Rect::new(area.x, area.y + 1 + row as u16, area.width, 1),
        );
    }
}

fn welcome_line() -> Line<'static> {
    let character_count = WELCOME_MESSAGE.chars().count();
    let spans = WELCOME_MESSAGE
        .chars()
        .enumerate()
        .map(|(index, character)| {
            let progress = if character_count <= 1 {
                0.0
            } else {
                index as f32 / (character_count - 1) as f32
            };
            let color = Color::Rgb(
                interpolate_color(WELCOME_START_COLOR.0, WELCOME_END_COLOR.0, progress),
                interpolate_color(WELCOME_START_COLOR.1, WELCOME_END_COLOR.1, progress),
                interpolate_color(WELCOME_START_COLOR.2, WELCOME_END_COLOR.2, progress),
            );
            Span::styled(character.to_string(), Style::default().fg(color))
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn interpolate_color(start: u8, end: u8, progress: f32) -> u8 {
    (start as f32 + (end as f32 - start as f32) * progress).round() as u8
}

fn welcome_lines(attached_agents: &[String]) -> Vec<Line<'static>> {
    let mut lines = vec![
        welcome_line(),
        Line::styled(WELCOME_TAGLINE, Style::default().fg(Color::DarkGray)),
        Line::raw(""),
    ];

    if attached_agents.is_empty() {
        lines.push(Line::styled(
            "Attached AGENTS.md: none",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        lines.push(Line::styled(
            "Attached AGENTS.md:",
            Style::default().fg(Color::DarkGray),
        ));
        lines.extend(
            attached_agents.iter().map(|path| {
                Line::styled(format!("• {path}"), Style::default().fg(Color::DarkGray))
            }),
        );
    }

    lines
}

fn transcript_lines(state: &UiState, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    let mut rendered_item = false;

    for (index, item) in state.transcript.iter().enumerate() {
        // Results are positioned on their matching call, even when the model
        // emitted several calls before execution produced any result.
        if is_result_attached_to_call(&state.transcript, index) {
            continue;
        }
        if rendered_item {
            lines.push(Line::raw(String::new()));
        }
        match item {
            TranscriptItem::User {
                text,
                skill_instruction_attached,
            } => {
                let text = redact_secret(text, Some(&state.secret));
                let trigger = skill_instruction_attached
                    .then(|| active_skill_trigger(&text, &state.skill_names))
                    .flatten();
                push_user_message_block(&mut lines, &text, trigger, width);
            }
            TranscriptItem::Assistant(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_wrapped(&mut lines, &text, width, Style::default());
            }
            TranscriptItem::ToolCall {
                id,
                name,
                arguments,
            } => {
                let call_text = format!("[tool:{name} {}]", call_arguments(arguments));
                let call_text = redact_secret(&call_text, Some(&state.secret));
                let result = matching_tool_result(&state.transcript, index, id);
                let mut segments = vec![(
                    call_text,
                    if result.is_some() {
                        tool_call_style()
                    } else {
                        pending_tool_call_style()
                    },
                )];
                if let Some(result) = result {
                    let result_text =
                        redact_secret(&format_tool_result(result), Some(&state.secret));
                    segments.push((" > ".to_owned(), Style::default()));
                    segments.push((result_text, tool_result_style()));
                } else {
                    segments.push((
                        format!(" {}", spinner_frame(state)),
                        pending_tool_call_style(),
                    ));
                }
                push_spans_wrapped(&mut lines, &segments, width);
            }
            TranscriptItem::ToolResult {
                id: _,
                name: _,
                result,
            } => {
                let result_text = format_tool_result(result);
                let result_text = redact_secret(&result_text, Some(&state.secret));
                push_spans_wrapped(&mut lines, &[(result_text, tool_result_style())], width);
            }
            TranscriptItem::Error(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_wrapped(&mut lines, &text, width, error_style());
            }
            TranscriptItem::Info(text) => {
                let text = redact_secret(text, Some(&state.secret));
                push_wrapped(&mut lines, &text, width, info_style());
            }
            TranscriptItem::Thinking => {
                let text = format!("{} Thinking...", spinner_frame(state));
                push_wrapped(&mut lines, &text, width, thinking_style());
            }
        }
        rendered_item = true;
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

fn matching_tool_result<'a>(
    transcript: &'a [TranscriptItem],
    call_index: usize,
    call_id: &str,
) -> Option<&'a Value> {
    transcript
        .iter()
        .skip(call_index + 1)
        .find_map(|item| match item {
            TranscriptItem::ToolResult { id, result, .. } if id == call_id => Some(result),
            _ => None,
        })
}

fn is_result_attached_to_call(transcript: &[TranscriptItem], result_index: usize) -> bool {
    let TranscriptItem::ToolResult { id, .. } = &transcript[result_index] else {
        return false;
    };
    let Some(call_index) = transcript[..result_index].iter().rposition(
        |item| matches!(item, TranscriptItem::ToolCall { id: call_id, .. } if call_id == id),
    ) else {
        return false;
    };
    !transcript[call_index + 1..result_index].iter().any(
        |item| matches!(item, TranscriptItem::ToolResult { id: result_id, .. } if result_id == id),
    )
}

/// Render tool call arguments as the command string inside double quotes, for
/// example `"cat README.md"`. Previews are truncated to the same limit as tool
/// results; malformed arguments fall back to truncated raw text.
fn call_arguments(arguments: &str) -> String {
    let parsed: Value = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => return truncate_output(arguments),
    };
    if let Some(command) = parsed.get("command").and_then(Value::as_str) {
        return format!("\"{}\"", truncate_output(command));
    }
    let serialized = serde_json::to_string(&parsed).unwrap_or_else(|_| arguments.to_owned());
    truncate_output(&serialized)
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
    Style::default().fg(USER_BORDER_COLOR)
}

/// Render a full-width rounded outline so user messages visually match the
/// rounded prompt border while assistant and tool output stays borderless.
fn push_user_message_block(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    active_skill_trigger: Option<&str>,
    width: usize,
) {
    if width < 2 {
        lines.extend(styled_text_lines(
            text,
            active_skill_trigger,
            width.max(1),
            Style::default().fg(Color::White),
        ));
        return;
    }

    let content_width = width - 2;
    let border_style = user_message_style();
    let rows = styled_text_lines(
        text,
        active_skill_trigger,
        content_width,
        Style::default().fg(Color::White),
    );
    lines.push(Line::styled(
        format!("╭{}╮", "─".repeat(content_width)),
        border_style,
    ));
    for row in rows {
        let row_width = UnicodeWidthStr::width(row.to_string().as_str());
        let padding = content_width.saturating_sub(row_width);
        let mut spans = Vec::with_capacity(row.spans.len() + 3);
        spans.push(Span::styled("│", border_style));
        spans.extend(row.spans);
        spans.push(Span::styled(
            " ".repeat(padding),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::styled("│", border_style));
        lines.push(Line::from(spans));
    }
    lines.push(Line::styled(
        format!("╰{}╯", "─".repeat(content_width)),
        border_style,
    ));
}

fn tool_call_style() -> Style {
    Style::default().fg(Color::Magenta)
}

fn pending_tool_call_style() -> Style {
    Style::default().fg(PENDING_TOOL_COLOR)
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

fn context_status_text(state: &UiState) -> String {
    let used = format_context_tokens(state.context_tokens);
    let Some(window) = state.context_window else {
        return format!("ctx={used}/? (?%)");
    };
    let percentage = context_percentage(state.context_tokens, window);
    format!(
        "ctx={used}/{} ({percentage}%)",
        format_context_tokens(window)
    )
}

fn context_status_style(state: &UiState) -> Style {
    if state
        .context_window
        .is_some_and(|window| context_over_threshold(state.context_tokens, window))
    {
        Style::default().fg(PENDING_TOOL_COLOR)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn context_percentage(used: usize, window: usize) -> usize {
    if window == 0 {
        return 0;
    }
    ((used as u128 * 100).div_ceil(window as u128)) as usize
}

fn context_over_threshold(used: usize, window: usize) -> bool {
    window > 0 && (used as u128 * 100) > (window as u128 * 80)
}

fn format_context_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn activity_style_for(state: &UiState) -> Style {
    if state.status == "working" {
        Style::default().fg(Color::LightGreen)
    } else if state.status == "compacting" {
        Style::default().fg(PENDING_TOOL_COLOR)
    } else {
        Style::default().fg(Color::Cyan)
    }
}

fn thinking_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn waiting_input_style() -> Style {
    Style::default().fg(BUSY_INPUT_COLOR)
}

fn spinner_frame(state: &UiState) -> char {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let tick = state.cursor_epoch.elapsed().as_millis() / 100;
    FRAMES[(tick as usize) % FRAMES.len()]
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
fn push_spans_wrapped(lines: &mut Vec<Line<'static>>, segments: &[(String, Style)], width: usize) {
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    for (text, style) in segments {
        for character in text.chars() {
            let char_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
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
    // `split` preserves a trailing empty row, so Shift+Enter renders an
    // immediate new line even before another character is typed.
    for line in text.split('\n') {
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
    fn turn_notifications_use_osc_777_with_fixed_secret_safe_messages() {
        let cases = [
            (TurnNotification::Completed, "Turn complete"),
            (TurnNotification::Interrupted, "Turn interrupted"),
            (TurnNotification::Failed, "Turn failed"),
        ];

        for (notification, body) in cases {
            let mut output = Vec::new();
            send_turn_notification(&mut output, notification).expect("notification");
            assert_eq!(
                output,
                format!("\x1b]777;notify;Lucy;{body}\x07").into_bytes()
            );
        }
    }

    #[test]
    fn turn_notifications_follow_the_terminal_turn_status() {
        assert_eq!(
            turn_notification_for_status("finalizing"),
            TurnNotification::Completed
        );
        assert_eq!(
            turn_notification_for_status("cancelling"),
            TurnNotification::Interrupted
        );
        assert_eq!(
            turn_notification_for_status("error"),
            TurnNotification::Failed
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("notification sink unavailable"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("notification sink unavailable"))
        }
    }

    #[test]
    fn notification_write_failure_does_not_keep_the_tui_busy() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.busy = true;
        state.active_cancel = Some(CancellationToken::new());
        let mut writer = FailingWriter;

        release_finished_turn(&mut writer, &mut state);

        assert!(!state.busy);
        assert!(state.active_cancel.is_none());
    }

    #[test]
    fn an_idle_finish_does_not_emit_a_duplicate_notification() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        let mut output = Vec::new();

        release_finished_turn(&mut output, &mut state);

        assert!(output.is_empty());
    }

    #[test]
    fn context_status_shows_used_window_and_percentage() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_context(Some(100_000), 80_000);

        assert_eq!(context_status_text(&state), "ctx=80.0K/100.0K (80%)");
        assert_eq!(context_status_style(&state).fg, Some(Color::DarkGray));

        state.context_tokens = 80_001;
        assert_eq!(context_status_text(&state), "ctx=80.0K/100.0K (81%)");
        assert_eq!(context_status_style(&state).fg, Some(PENDING_TOOL_COLOR));
    }

    #[test]
    fn context_status_handles_unknown_window_without_highlighting() {
        let state = UiState::from_history(&[], "secret", "model", None, false);

        assert_eq!(context_status_text(&state), "ctx=1/? (?%)");
        assert_eq!(context_status_style(&state).fg, Some(Color::DarkGray));
    }

    #[test]
    fn context_status_is_right_aligned_and_turns_orange_above_eighty_percent() {
        let state =
            UiState::from_history(&[], "secret", "model", None, false).with_context(Some(100), 81);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(60, 10)).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw statusline");

        let buffer = terminal.backend().buffer();
        let status_row = 9;
        let context = context_status_text(&state);
        let context_start = 60 - context.chars().count();
        assert_eq!(buffer[(context_start as u16, status_row)].symbol(), "c");
        assert_eq!(buffer[(59, status_row)].symbol(), ")");
        assert_eq!(buffer[(59, status_row)].fg, PENDING_TOOL_COLOR);
    }

    #[test]
    fn fresh_sessions_show_the_versioned_gradient_welcome_message() {
        let state = UiState::from_history(&[], "secret", "model", None, false);
        assert!(state.welcome_visible);

        let line = welcome_line();
        assert_eq!(line.to_string(), WELCOME_MESSAGE);
        assert_eq!(
            line.spans.first().and_then(|span| span.style.fg),
            Some(Color::Rgb(
                WELCOME_START_COLOR.0,
                WELCOME_START_COLOR.1,
                WELCOME_START_COLOR.2,
            ))
        );
        assert_eq!(
            line.spans.last().and_then(|span| span.style.fg),
            Some(Color::Rgb(
                WELCOME_END_COLOR.0,
                WELCOME_END_COLOR.1,
                WELCOME_END_COLOR.2,
            ))
        );
    }

    #[test]
    fn welcome_shows_the_tagline_and_attached_agents_paths() {
        let state = UiState::from_history(&[], "secret", "model", None, false)
            .with_attached_agents(vec![
                "/workspace/AGENTS.md".to_owned(),
                "/workspace/app/AGENTS.md".to_owned(),
            ]);
        let lines = welcome_lines(&state.attached_agents);

        assert_eq!(lines[1].to_string(), WELCOME_TAGLINE);
        assert_eq!(lines[1].style.fg, Some(Color::DarkGray));
        assert_eq!(lines[3].to_string(), "Attached AGENTS.md:");
        assert_eq!(lines[4].to_string(), "• /workspace/AGENTS.md");
        assert_eq!(lines[5].to_string(), "• /workspace/app/AGENTS.md");
        assert!(lines[3..]
            .iter()
            .all(|line| line.style.fg == Some(Color::DarkGray)));
    }

    #[test]
    fn welcome_reports_when_no_agents_file_is_attached() {
        let lines = welcome_lines(&[]);
        assert_eq!(
            lines.last().expect("empty context line").to_string(),
            "Attached AGENTS.md: none"
        );
    }

    #[test]
    fn resumed_sessions_do_not_show_the_welcome_message() {
        let state = UiState::from_history(&[], "secret", "model", None, true);
        assert!(!state.welcome_visible);
    }

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
        let state = UiState::from_history(&history, "provider-secret", "model", None, true);
        assert!(matches!(state.transcript[0], TranscriptItem::User { .. }));
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
        let state = UiState::from_history(&history, "provider-secret", "model", None, true);
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
        let state = UiState::from_history(&history, "provider-secret", "model", None, true);
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
    fn user_message_borders_remain_muted_yellow_while_its_text_is_white() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::user("hello".to_owned()),
        }];
        let state = UiState::from_history(&history, "provider-secret", "model", None, false);
        let lines = transcript_lines(&state, 12);

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].to_string(), "╭──────────╮");
        assert_eq!(lines[1].to_string(), "│hello     │");
        assert_eq!(lines[2].to_string(), "╰──────────╯");
        assert_eq!(lines[0].style.fg, Some(USER_BORDER_COLOR));
        assert_eq!(lines[2].style.fg, Some(USER_BORDER_COLOR));
        assert_eq!(lines[1].spans[0].style.fg, Some(USER_BORDER_COLOR));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::White));
        assert_eq!(
            lines[1].spans.last().map(|span| span.style.fg),
            Some(Some(USER_BORDER_COLOR))
        );
    }

    #[test]
    fn attached_skill_highlights_its_trigger_in_the_user_message_without_a_notice_line() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["release-notes".to_owned()]);
        state.add_user("/release-notes v1.2.0", "secret");
        state.mark_latest_user_skill_attached();

        let lines = transcript_lines(&state, 40);
        assert_eq!(lines.len(), 3);
        let cyan_text = lines[1]
            .spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::Cyan))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(cyan_text, "/release-notes");
        assert!(!lines
            .iter()
            .any(|line| line.to_string().contains("instruction attached")));
    }

    #[test]
    fn transcript_rendering_redacts_history_content() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::assistant("provider-secret".to_owned(), Vec::new()),
        }];
        let state = UiState::from_history(&history, "provider-secret", "model", None, false);
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
        let mut state = UiState::from_history(&history, "provider-secret", "model", None, false);
        handle_mouse_event(&mut state, MouseEventKind::ScrollUp, 10);
        assert!(!state.auto_scroll);
        assert_eq!(state.scroll, 7);
        handle_mouse_event(&mut state, MouseEventKind::ScrollDown, 10);
        assert!(
            state.auto_scroll,
            "reaching the bottom resumes transcript following"
        );
        assert_eq!(state.scroll, 0);
        scroll_up(&mut state, 10);
        assert!(!state.auto_scroll);
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
        let mut state = UiState::from_history(&history, "provider-secret", "model", None, false);
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
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let lines = transcript_lines(&state, 80);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].to_string(), format!("╭{}╮", "─".repeat(78)));
        assert_eq!(lines[1].to_string(), format!("│hi{}│", " ".repeat(76)));
        assert_eq!(lines[2].to_string(), format!("╰{}╯", "─".repeat(78)));
        assert_eq!(lines[3].to_string(), "");
        assert_eq!(lines[4].to_string(), "hello");
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
        let state = UiState::from_history(&history, "secret", "model", None, false);
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
    fn pending_tool_calls_are_orange_and_show_a_spinner() {
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
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let line = &transcript_lines(&state, 80)[0];

        assert!(line.to_string().contains("[tool:cmd \"pwd\"] "));
        assert!(line
            .spans
            .iter()
            .all(|span| span.style.fg == Some(PENDING_TOOL_COLOR)));
        assert!(line.spans.iter().any(|span| {
            span.content
                .chars()
                .any(|character| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(character))
        }));
    }

    #[test]
    fn tool_call_truncates_long_arguments() {
        let command = "a".repeat(80);
        let arguments = serde_json::json!({"command": command}).to_string();
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::assistant(
                String::new(),
                vec![crate::model::ChatToolCall {
                    id: "call-1".to_owned(),
                    name: "cmd".to_owned(),
                    arguments,
                }],
            ),
        }];
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let text = transcript_lines(&state, 200)[0].to_string();
        assert!(text.contains(&format!("[tool:cmd \"{}…\"]", "a".repeat(50))));
        assert!(!text.contains(&"a".repeat(51)));
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
        let state = UiState::from_history(&history, "secret", "model", None, false);
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
        let state = UiState::from_history(&history, "secret", "model", None, false);
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
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let lines = transcript_lines(&state, 200);
        let spans = &lines[0].spans;
        // Calls and results retain their distinct foreground styles, with an
        // unstyled separator between them.
        assert_eq!(spans[0].style.fg, Some(Color::Magenta));
        assert!(spans.iter().any(|span| span.style.fg.is_none()));
        let result_text = spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::DarkGray))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(result_text, "[\"x\"]");
        assert!(!lines[0]
            .to_string()
            .chars()
            .any(|character| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(character)));
    }

    #[test]
    fn recognized_skill_trigger_is_highlighted_but_arguments_remain_default_colored() {
        let trigger = active_skill_trigger("/release-notes v1.2.0", &["release-notes".to_owned()]);
        assert_eq!(trigger, Some("/release-notes"));

        let lines = styled_text_lines(
            "/release-notes v1.2.0",
            trigger,
            80,
            Style::default().fg(Color::White),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "/release-notes v1.2.0");
        assert_eq!(lines[0].spans[0].content, "/release-notes");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(lines[0].spans[1].content, " v1.2.0");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::White));
    }

    #[test]
    fn draw_renders_an_active_skill_trigger_in_cyan() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["release-notes".to_owned()]);
        state.input = "/release-notes v1.2.0".to_owned();
        state.cursor = state.input.chars().count();

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(40, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw input");

        // The one-row input area starts at (1, 7): trigger characters are cyan,
        // while the argument that follows keeps the normal white input color.
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 7)].fg, Color::Cyan);
        assert_eq!(buffer[(21, 7)].fg, Color::White);
    }

    #[test]
    fn busy_input_uses_a_dark_gray_border_and_waiting_message() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.busy = true;

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw waiting input");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 6)].fg, BUSY_INPUT_COLOR);
        assert_eq!(buffer[(1, 7)].symbol(), "T");
        assert_eq!(buffer[(1, 7)].fg, BUSY_INPUT_COLOR);
        assert_eq!(input_display_text(&state), WAITING_INPUT_MESSAGE);
    }

    #[test]
    fn only_known_leading_skill_commands_activate_input_highlighting() {
        let skills = ["release-notes".to_owned()];
        assert_eq!(
            active_skill_trigger("/missing", &skills),
            None,
            "unknown commands are rejected by the turn engine and must not look active"
        );
        assert_eq!(
            active_skill_trigger("/skill:release-notes", &skills),
            None,
            "the removed /skill: wrapper must not look active"
        );
        assert_eq!(
            active_skill_trigger("write /release-notes", &skills),
            None,
            "only the command prefix accepted by the turn engine is active"
        );
        assert_eq!(active_skill_trigger("/", &skills), None);
    }

    #[test]
    fn highlighted_skill_trigger_remains_styled_when_wrapped() {
        let input = "/release-notes argument";
        let trigger = active_skill_trigger(input, &["release-notes".to_owned()]);
        let lines = styled_text_lines(input, trigger, 8, Style::default().fg(Color::White));
        let highlighted = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.fg == Some(Color::Cyan))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(highlighted, "/release-notes");
    }

    #[test]
    fn input_has_no_prompt_marker_and_trailing_newline_is_visible() {
        assert_eq!(input_prompt("hello"), "hello");
        assert_eq!(wrap_text("hello\n", 80), vec!["hello", ""]);
    }

    #[test]
    fn input_prompt_wraps_to_multiple_rows_when_long() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.input = "abcdefghij".to_owned();
        // width 5: the input wraps across multiple rows without a prompt marker.
        let rows = input_visible_rows(&state, 5);
        assert!(rows >= 2);
    }

    #[test]
    fn cursor_editing_moves_by_characters_and_preserves_unicode() {
        let mut input = "가나".to_owned();
        let mut cursor = input.chars().count();
        cursor -= 1;
        insert_at_cursor(&mut input, &mut cursor, 'x');
        assert_eq!(input, "가x나");
        assert_eq!(cursor, 2);
        assert!(remove_before_cursor(&mut input, &mut cursor));
        assert_eq!(input, "가나");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn cursor_row_tracks_newlines_and_wrapping() {
        assert_eq!(cursor_row("hello\nworld", 6, 80), 1);
        assert_eq!(cursor_row("abcdef", 4, 3), 1);
    }

    #[test]
    fn shift_enter_inserts_at_the_cursor_and_moves_it_to_the_new_row() {
        let mut input = "beforeafter".to_owned();
        let mut cursor = 6;
        insert_at_cursor(&mut input, &mut cursor, '\n');

        assert_eq!(input, "before\nafter");
        assert_eq!(cursor, 7);
        assert_eq!(cursor_row(&input, cursor, 80), 1);
    }

    #[test]
    fn shift_enter_renders_the_cursor_on_the_new_input_row() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.input = "beforeafter".to_owned();
        state.cursor = 6;
        insert_at_cursor(&mut state.input, &mut state.cursor, '\n');
        state.cursor_epoch = Instant::now();

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(20, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw input cursor");

        // The input box starts at row 5; its inner area begins at (1, 6).
        // After inserting a newline, the cursor is at the start of row 7.
        terminal.backend_mut().assert_cursor_position((1, 7));
    }

    #[test]
    fn tool_results_attach_to_their_matching_call_after_consecutive_calls() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![
                        crate::model::ChatToolCall {
                            id: "call-first".to_owned(),
                            name: "cmd".to_owned(),
                            arguments: r#"{"command":"first"}"#.to_owned(),
                        },
                        crate::model::ChatToolCall {
                            id: "call-second".to_owned(),
                            name: "cmd".to_owned(),
                            arguments: r#"{"command":"second"}"#.to_owned(),
                        },
                    ],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-first".to_owned(),
                    "cmd".to_owned(),
                    serde_json::json!({"stdout":"first result","stderr":""}).to_string(),
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 3,
                message: ChatMessage::tool(
                    "call-second".to_owned(),
                    "cmd".to_owned(),
                    serde_json::json!({"stdout":"second result","stderr":""}).to_string(),
                ),
            },
        ];

        let state = UiState::from_history(&history, "secret", "model", None, false);
        let lines = transcript_lines(&state, 200);
        assert_eq!(
            lines.len(),
            3,
            "only the two call lines and their separator remain"
        );
        assert_eq!(
            lines[0].to_string(),
            "[tool:cmd \"first\"] > [\"first result\"]"
        );
        assert_eq!(
            lines[2].to_string(),
            "[tool:cmd \"second\"] > [\"second result\"]"
        );
    }
}

#[cfg(test)]
mod skill_picker_tests {
    use super::*;

    fn skill_names() -> Vec<String> {
        ["alpha", "beta", "build", "charlie", "deploy", "doctor"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn slash_picker_filters_only_leading_command_text_and_hides_without_matches() {
        let names = skill_names();
        assert_eq!(
            matching_skill_names("/", &names),
            vec!["alpha", "beta", "build", "charlie", "deploy", "doctor"]
        );
        assert_eq!(matching_skill_names("/b", &names), vec!["beta", "build"]);
        assert!(matching_skill_names("/missing", &names).is_empty());
        assert!(matching_skill_names("message /b", &names).is_empty());
        assert!(matching_skill_names("/beta arguments", &names).is_empty());
    }

    #[test]
    fn slash_picker_focuses_the_top_match_and_moves_within_filtered_results() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(skill_names());
        state.input = "/b".to_owned();
        state.input_changed();

        assert!(state.skill_picker_visible());
        assert_eq!(state.skill_picker_focus, 0);
        assert!(state.move_skill_picker(true));
        assert_eq!(state.skill_picker_focus, 1);
        assert!(state.move_skill_picker(true));
        assert_eq!(state.skill_picker_focus, 1, "focus does not leave the list");
        assert!(state.move_skill_picker(false));
        assert_eq!(state.skill_picker_focus, 0);

        state.input = "/missing".to_owned();
        state.input_changed();
        assert!(!state.skill_picker_visible());
        assert!(!state.move_skill_picker(true));
    }

    #[test]
    fn enter_selects_the_focused_skill_then_leaves_the_completed_command_ready_to_send() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(skill_names());
        state.input = "/b".to_owned();
        state.input_changed();
        state.move_skill_picker(true);

        assert!(state.select_focused_skill());
        assert_eq!(state.input, "/build");
        assert_eq!(state.cursor, "/build".chars().count());
        assert!(
            !state.skill_picker_visible(),
            "the first Enter completes the input rather than sending it"
        );
        assert!(
            !state.select_focused_skill(),
            "a second Enter follows the normal send/attachment path"
        );
    }

    #[test]
    fn slash_picker_keeps_the_focused_item_in_its_five_row_viewport() {
        assert_eq!(skill_picker_range(20, 0), 0..5);
        assert_eq!(skill_picker_range(20, 4), 0..5);
        assert_eq!(skill_picker_range(20, 5), 1..6);
        assert_eq!(skill_picker_range(20, 19), 15..20);
    }

    #[test]
    fn slash_picker_overlays_the_ready_activity_indicator() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["a".to_owned(), "b".to_owned()]);
        state.input = "/".to_owned();
        state.input_changed();
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(40, 12)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw TUI");

        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("/a"));
        assert!(
            !screen.contains("ready"),
            "the skill picker should cover the ready activity indicator"
        );
    }

    #[test]
    fn slash_picker_is_rendered_immediately_above_the_input() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(skill_names());
        state.input = "/".to_owned();
        state.input_changed();
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(40, 12)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw TUI");

        let buffer = terminal.backend().buffer();
        // With a one-row prompt, the six-row picker ends directly before the
        // rounded input box: header, five skills, then the input border.
        assert_eq!(buffer[(0, 2)].symbol(), "[");
        assert_eq!(buffer[(0, 7)].symbol(), "/");
        assert_eq!(buffer[(0, 8)].symbol(), "╭");
    }

    #[test]
    fn slash_picker_renders_count_and_distinct_focus_colors() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(skill_names());
        state.input = "/".to_owned();
        state.input_changed();
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(30, 8)).expect("test terminal");
        terminal
            .draw(|frame| draw_skill_picker(frame, &state, Rect::new(0, 0, 30, 6)))
            .expect("draw skill picker");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "[");
        assert_eq!(buffer[(0, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(0, 1)].symbol(), "/");
        assert_eq!(buffer[(0, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(0, 2)].symbol(), "/");
        assert_eq!(buffer[(0, 2)].fg, Color::DarkGray);
    }
}
