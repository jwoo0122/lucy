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
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Terminal;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::app::Harness;
use crate::cancellation::CancellationToken;
use crate::model::{estimate_context_tokens, ChatMessage};
use crate::protocol::{EventSink, ProtocolEvent};
use crate::provider::ProviderModel;
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
// The sequence travels through warm neighbouring hues, then returns along the
// same path. Each five-second phase blends only its adjacent palette colours.
const WORKING_GRADIENT_COLORS: [(u8, u8, u8); 4] = [
    (220, 35, 175), // magenta
    (235, 45, 65),  // red
    (255, 130, 25), // orange
    (235, 45, 65),  // red, returning to magenta
];
const WORKING_GRADIENT_CYCLE: Duration = Duration::from_millis(5000);
const SKILL_PICKER_MAX_ROWS: usize = 5;
const BUILTIN_COMMANDS: [&str; 2] = ["settings", "exit"];
const SETTINGS_MIN_WIDTH: u16 = 36;
const SETTINGS_MAX_WIDTH: u16 = 88;
const SETTINGS_MIN_HEIGHT: u16 = 8;
const SETTINGS_MAX_HEIGHT: u16 = 22;

pub(crate) fn run<W: Write>(mut harness: Harness, resumed: bool, stdout: W) -> Result<(), String> {
    let secret = harness.provider.api_key().to_owned();
    let context_window = harness
        .context_window
        .or_else(|| harness.provider.context_window());
    harness.context_window = context_window;
    let context_tokens = estimate_context_tokens(&harness.session.provider_messages());
    let skill_names = command_names(
        harness
            .session
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect(),
    );
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

    loop {
        if let Some(notification) = harness.next_subagent_notification() {
            let cancel = CancellationToken::new();
            let _ = messages.send(WorkerMessage::Started {
                cancel: cancel.clone(),
                user_text: None,
            });
            if let Err(error) = harness.handle_message(&notification, &mut sink, Some(&cancel)) {
                let message = redact_secret(&error, Some(harness.provider.api_key()));
                let _ = sink.emit_event(&ProtocolEvent::Error { message });
            }
            let _ = messages.send(WorkerMessage::Finished);
            continue;
        }
        let request = match requests.recv_timeout(EVENT_POLL) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(notification) = harness.next_subagent_notification() {
                    let cancel = CancellationToken::new();
                    let _ = messages.send(WorkerMessage::Started {
                        cancel: cancel.clone(),
                        user_text: None,
                    });
                    if let Err(error) =
                        harness.handle_message(&notification, &mut sink, Some(&cancel))
                    {
                        let message = redact_secret(&error, Some(harness.provider.api_key()));
                        let _ = sink.emit_event(&ProtocolEvent::Error { message });
                    }
                    let _ = messages.send(WorkerMessage::Finished);
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match request {
            WorkerRequest::Turn { text } => {
                let cancel = CancellationToken::new();
                let _ = messages.send(WorkerMessage::Started {
                    cancel: cancel.clone(),
                    user_text: Some(text.clone()),
                });
                if let Err(error) = harness.handle_message(&text, &mut sink, Some(&cancel)) {
                    let message = redact_secret(&error, Some(harness.provider.api_key()));
                    let _ = sink.emit_event(&ProtocolEvent::Error { message });
                }
                let _ = messages.send(WorkerMessage::Finished);
            }
            WorkerRequest::Catalog => {
                let _ = messages.send(WorkerMessage::Catalog(
                    harness.provider.models().map_err(|error| error.to_string()),
                ));
            }
            WorkerRequest::ApplySettings { model, effort } => {
                let result = harness.apply_settings(&harness.home.clone(), model, effort);
                let _ = messages.send(WorkerMessage::SettingsApplied(
                    result,
                    harness.session.llm.model.clone(),
                    harness.session.llm.effort.clone(),
                    harness.context_window,
                ));
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
                Ok(WorkerMessage::Started { cancel, user_text }) => {
                    if let Some(text) = user_text {
                        state.start_queued_user(&text);
                    }
                    state.active_cancel = Some(cancel);
                    state.busy = true;
                    state.status = "working".to_owned();
                }
                Ok(WorkerMessage::Thinking) => state.show_thinking(),
                Ok(WorkerMessage::ReasoningCompleted) => state.complete_reasoning(),
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
                Ok(WorkerMessage::Catalog(result)) => state.open_catalog(result),
                Ok(WorkerMessage::SettingsApplied(result, model, effort, context_window)) => {
                    state.settings_applied(result, model, effort, context_window)
                }
                Ok(WorkerMessage::Finished) => {
                    release_finished_turn(terminal.backend_mut(), state);
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
            if !state.busy && state.settings.is_some() {
                if let Some((model, effort)) = state.handle_settings_key(&key) {
                    state.settings = Some(SettingsState::Applying {
                        model: model.clone(),
                        effort: effort.clone(),
                    });
                    requests
                        .send(WorkerRequest::ApplySettings { model, effort })
                        .map_err(|_| "TUI worker is unavailable".to_owned())?;
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
                    // A focused built-in is an action, unlike a skill: Enter
                    // invokes it immediately. Tab remains completion-only.
                    let text = if let Some(command) = state.focused_builtin_command() {
                        state.input.clear();
                        format!("/{}", command.name())
                    } else {
                        if state.select_focused_skill() {
                            continue;
                        }
                        std::mem::take(&mut state.input)
                    };
                    state.cursor = 0;
                    if let Some(command) = builtin_command(&text) {
                        state.reset_skill_picker();
                        if state.busy {
                            state.transcript.push(TranscriptItem::Info(format!(
                                "/{} is available when the current turn finishes",
                                command.name()
                            )));
                            continue;
                        }
                        match command {
                            BuiltinCommand::Settings => {
                                state.settings = Some(SettingsState::Loading);
                                requests
                                    .send(WorkerRequest::Catalog)
                                    .map_err(|_| "TUI worker is unavailable".to_owned())?;
                                continue;
                            }
                            BuiltinCommand::Exit => return Ok(()),
                        }
                    }
                    state.reset_skill_picker();
                    if text.trim().is_empty() {
                        continue;
                    }
                    state.auto_scroll = true;
                    state.scroll = 0;
                    state.queue_user(&text);
                    state.busy = true;
                    state.status = "working".to_owned();
                    requests
                        .send(WorkerRequest::Turn { text })
                        .map_err(|_| "TUI worker is unavailable".to_owned())?;
                }
                KeyCode::Tab => {
                    // Tab completes the focused skill while the slash picker
                    // is active, using the same first-selection path as Enter.
                    state.select_focused_skill();
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
    },
    Catalog,
    ApplySettings {
        model: String,
        effort: Option<String>,
    },
    Shutdown,
}

enum WorkerMessage {
    Event(ProtocolEvent),
    Started {
        cancel: CancellationToken,
        user_text: Option<String>,
    },
    Thinking,
    ReasoningCompleted,
    SkillInstructionAttached,
    ContextUsage(usize),
    CompactionStarted,
    CompactionFinished {
        tokens_before: usize,
        tokens_after: usize,
    },
    Catalog(Result<Vec<ProviderModel>, String>),
    SettingsApplied(Result<(), String>, String, Option<String>, Option<usize>),
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

    fn reasoning_completed(&mut self) -> io::Result<()> {
        self.sender
            .send(WorkerMessage::ReasoningCompleted)
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
    queued_messages: Vec<String>,
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
    settings: Option<SettingsState>,
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
            queued_messages: Vec::new(),
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
            settings: None,
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
    /// Return the built-in represented by the focused slash-picker row, if
    /// any. Built-ins execute on Enter while skills merely complete there.
    fn focused_builtin_command(&self) -> Option<BuiltinCommand> {
        let name = *self.matching_skill_names().get(self.skill_picker_focus)?;
        builtin_command(&format!("/{name}"))
    }

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

    fn open_catalog(&mut self, result: Result<Vec<ProviderModel>, String>) {
        self.settings = Some(match result {
            Ok(models) => {
                let focus = models
                    .iter()
                    .position(|model| model.id == self.model)
                    .unwrap_or(0);
                SettingsState::Models {
                    models,
                    query: String::new(),
                    focus,
                }
            }
            Err(error) => SettingsState::Error(error),
        });
    }
    fn settings_applied(
        &mut self,
        result: Result<(), String>,
        model: String,
        effort: Option<String>,
        context_window: Option<usize>,
    ) {
        match result {
            Ok(()) => {
                self.model = model;
                self.effort = effort;
                self.context_window = context_window;
                self.settings = None;
                self.transcript
                    .push(TranscriptItem::Info("⚙ settings applied".to_owned()));
            }
            Err(error) => self.settings = Some(SettingsState::Error(error)),
        }
    }
    fn handle_settings_key(&mut self, key: &KeyEvent) -> Option<(String, Option<String>)> {
        let current_effort = self.effort.clone();
        match self.settings.as_mut()? {
            SettingsState::Loading => {
                if key.code == KeyCode::Esc {
                    self.settings = None;
                }
            }
            SettingsState::Applying { .. } => {}
            SettingsState::Error(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.settings = None;
                }
            }
            SettingsState::Models {
                models,
                query,
                focus,
            } => match key.code {
                KeyCode::Esc => self.settings = None,
                KeyCode::Char(c) => {
                    query.push(c);
                    *focus = 0;
                }
                KeyCode::Backspace => {
                    query.pop();
                    *focus = 0;
                }
                KeyCode::Up => *focus = focus.saturating_sub(1),
                KeyCode::Down => {
                    let n = models
                        .iter()
                        .filter(|m| m.id.to_lowercase().contains(&query.to_lowercase()))
                        .count();
                    *focus = (*focus + 1).min(n.saturating_sub(1));
                }
                KeyCode::Enter => {
                    let selected = models
                        .iter()
                        .filter(|m| m.id.to_lowercase().contains(&query.to_lowercase()))
                        .nth(*focus)
                        .cloned();
                    if let Some(model) = selected {
                        let focus = model
                            .efforts
                            .as_ref()
                            .and_then(|efforts| {
                                current_effort.as_ref().and_then(|current| {
                                    efforts.iter().position(|effort| effort == current)
                                })
                            })
                            .map_or(0, |index| index + 1);
                        self.settings = Some(SettingsState::Effort {
                            model,
                            input: current_effort.unwrap_or_default(),
                            focus,
                        });
                    }
                }
                _ => {}
            },
            SettingsState::Effort {
                model,
                input,
                focus,
            } => match key.code {
                KeyCode::Esc => self.settings = None,
                KeyCode::Char(c) if model.efforts.is_none() => input.push(c),
                KeyCode::Backspace if model.efforts.is_none() => {
                    input.pop();
                }
                KeyCode::Up => *focus = focus.saturating_sub(1),
                KeyCode::Down => {
                    if let Some(efforts) = &model.efforts {
                        *focus = (*focus + 1).min(efforts.len());
                    }
                }
                KeyCode::Enter => {
                    let effort = match &model.efforts {
                        Some(efforts) => {
                            if *focus == 0 {
                                None
                            } else {
                                efforts.get(focus.saturating_sub(1)).cloned()
                            }
                        }
                        None => (!input.trim().is_empty()).then(|| input.trim().to_owned()),
                    };
                    return Some((model.id.clone(), effort));
                }
                _ => {}
            },
        };
        None
    }

    fn add_history_record(&mut self, record: &SessionHistoryRecord) {
        match record {
            SessionHistoryRecord::ProviderSettings { model, effort, .. } => {
                self.transcript.push(TranscriptItem::Info(format!(
                    "⚙ model={model} · effort={}",
                    effort.as_deref().unwrap_or("default")
                )))
            }
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

    fn queue_user(&mut self, text: &str) {
        self.queued_messages
            .push(redact_secret(text, Some(&self.secret)));
    }

    fn start_queued_user(&mut self, text: &str) {
        let safe = redact_secret(text, Some(&self.secret));
        if self.queued_messages.first() == Some(&safe) {
            self.queued_messages.remove(0);
        } else if let Some(index) = self
            .queued_messages
            .iter()
            .position(|queued| queued == &safe)
        {
            self.queued_messages.remove(index);
        }
        self.add_user(text, &self.secret.clone());
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
        if matches!(
            self.transcript.last(),
            Some(TranscriptItem::Reasoning { complete: false })
        ) {
            self.transcript.pop();
        }
    }

    fn show_thinking(&mut self) {
        self.status = "working".to_owned();
        if !matches!(
            self.transcript.last(),
            Some(TranscriptItem::Reasoning { complete: false })
        ) {
            self.transcript
                .push(TranscriptItem::Reasoning { complete: false });
        }
    }

    fn complete_reasoning(&mut self) {
        if let Some(TranscriptItem::Reasoning { complete }) = self.transcript.last_mut() {
            *complete = true;
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
        (self.cursor_epoch.elapsed().as_millis() / CURSOR_BLINK_INTERVAL.as_millis())
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
                self.complete_reasoning();
                self.status = "finalizing".to_owned();
                self.transcript
                    .push(TranscriptItem::Info("✓ turn complete".to_owned()));
            }
            ProtocolEvent::TurnInterrupted { reason, phase } => {
                self.complete_reasoning();
                self.status = "cancelling".to_owned();
                self.transcript
                    .push(TranscriptItem::Info(format!("! {reason} ({phase})")));
            }
            ProtocolEvent::Error { message } => {
                self.complete_reasoning();
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
    Reasoning {
        complete: bool,
    },
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
    let (chat_chunk, _, _, _) = ui_layout(state, area);
    let chat_height = chat_chunk.height.saturating_sub(1);
    let lines = transcript_lines(state, chat_chunk.width);
    lines
        .len()
        .saturating_sub(chat_height as usize)
        .min(u16::MAX as usize) as u16
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
    redact_secret(&input_prompt(&state.input), Some(&state.secret))
}

fn command_names(mut skill_names: Vec<String>) -> Vec<String> {
    skill_names.extend(BUILTIN_COMMANDS.into_iter().map(str::to_owned));
    skill_names.sort();
    skill_names.dedup();
    skill_names
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuiltinCommand {
    Settings,
    Exit,
}

impl BuiltinCommand {
    fn name(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Exit => "exit",
        }
    }
}

fn builtin_command(input: &str) -> Option<BuiltinCommand> {
    match input.split_whitespace().next()? {
        "/settings" => Some(BuiltinCommand::Settings),
        "/exit" => Some(BuiltinCommand::Exit),
        _ => None,
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
        // Header, visible commands, and the rounded top/bottom border.
        (state
            .matching_skill_names()
            .len()
            .min(SKILL_PICKER_MAX_ROWS)
            + 3) as u16
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
    // The skill picker is a true overlay: it must not resize or scroll the transcript.
    let visible_chat_area = chat_area;
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

    let queued_suffix = (!state.queued_messages.is_empty()).then(|| {
        format!(
            " · queued {}: {}",
            state.queued_messages.len(),
            state.queued_messages.join(" | ")
        )
    });
    let activity_text = if state.status == "working" {
        format!("{} working", spinner_frame(state))
    } else if state.status == "compacting" {
        format!("{} compacting", spinner_frame(state))
    } else {
        format!("● {}", state.status)
    };
    let activity = activity_line(state, &activity_text, queued_suffix.as_deref());
    frame.render_widget(Paragraph::new(activity), activity_area);

    if let Some(picker_area) = picker_area {
        draw_skill_picker(frame, state, picker_area);
    }

    let input_style = user_message_style();
    let input_text_style = Style::default().fg(Color::White);
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
    if let Some(settings) = &state.settings {
        draw_settings(frame, settings, frame.area());
    }

    // Ratatui shows the cursor when a frame requests a position and hides it
    // when this branch is skipped, which provides the blink phase.
    if state.settings.is_none() && state.cursor_visible() && !input_area.is_empty() && visible > 0 {
        let cursor_prefix: String = prompt.chars().take(state.cursor).collect();
        let cursor_rows = wrap_text(&cursor_prefix, input_area.width.max(1) as usize);
        let cursor_line = cursor_rows.last().map(String::as_str).unwrap_or("");
        let cursor_offset = UnicodeWidthStr::width(cursor_line) as u16;
        let cursor_x = input_area.x + cursor_offset.min(input_area.width.saturating_sub(1));
        let cursor_y = input_area.y + cursor_row.saturating_sub(input_scroll);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

enum SettingsState {
    Loading,
    Applying {
        model: String,
        effort: Option<String>,
    },
    Error(String),
    Models {
        models: Vec<ProviderModel>,
        query: String,
        focus: usize,
    },
    Effort {
        model: ProviderModel,
        input: String,
        focus: usize,
    },
}
fn draw_settings(frame: &mut Frame<'_>, settings: &SettingsState, area: Rect) {
    let width = area
        .width
        .saturating_sub(2)
        .min(SETTINGS_MAX_WIDTH)
        .max(SETTINGS_MIN_WIDTH.min(area.width));
    let height = area
        .height
        .saturating_sub(2)
        .min(SETTINGS_MAX_HEIGHT)
        .max(SETTINGS_MIN_HEIGHT.min(area.height));
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" /settings ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = match settings {
        SettingsState::Loading => vec![
            Line::styled("Loading provider models…", Style::default().fg(Color::Cyan)),
            Line::raw(""),
            Line::styled("Esc  cancel", Style::default().fg(Color::DarkGray)),
        ],
        SettingsState::Applying { model, effort } => vec![
            Line::styled("Applying selection…", Style::default().fg(Color::Cyan)),
            Line::raw(model.clone()),
            Line::raw(format!(
                "effort: {}",
                effort.as_deref().unwrap_or("default")
            )),
        ],
        SettingsState::Error(error) => vec![
            Line::styled("Unable to update settings", Style::default().fg(Color::Red)),
            Line::raw(""),
            Line::raw(error.clone()),
            Line::raw(""),
            Line::styled("Enter/Esc  close", Style::default().fg(Color::DarkGray)),
        ],
        SettingsState::Models {
            models,
            query,
            focus,
        } => {
            let query_lower = query.to_lowercase();
            let filtered = models
                .iter()
                .filter(|model| model.id.to_lowercase().contains(&query_lower))
                .collect::<Vec<_>>();
            let focus = (*focus).min(filtered.len().saturating_sub(1));
            let list_rows = inner.height.saturating_sub(4) as usize;
            let range = selection_range(filtered.len(), focus, list_rows);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Model  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if query.is_empty() {
                            "type to filter…"
                        } else {
                            query
                        },
                        Style::default().fg(if query.is_empty() {
                            Color::DarkGray
                        } else {
                            Color::White
                        }),
                    ),
                ]),
                Line::styled(
                    format!(
                        "{} models{}",
                        filtered.len(),
                        if filtered.is_empty() {
                            ""
                        } else {
                            " · ↑/↓ move · Enter choose"
                        }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if filtered.is_empty() {
                lines.push(Line::styled(
                    "No matching models",
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                for index in range {
                    let selected = index == focus;
                    lines.push(Line::styled(
                        format!(
                            "{} {}",
                            if selected { "›" } else { " " },
                            filtered[index].id
                        ),
                        if selected {
                            Style::default().fg(Color::Black).bg(Color::Cyan)
                        } else {
                            Style::default().fg(Color::White)
                        },
                    ));
                }
            }
            lines.push(Line::styled(
                "Esc  cancel",
                Style::default().fg(Color::DarkGray),
            ));
            lines
        }
        SettingsState::Effort {
            model,
            input,
            focus,
        } => {
            let mut lines = vec![
                Line::styled(model.id.clone(), Style::default().fg(Color::Cyan)),
                Line::styled("Reasoning effort", Style::default().fg(Color::DarkGray)),
            ];
            match &model.efforts {
                Some(efforts) => {
                    let total = efforts.len() + 1;
                    let focus = (*focus).min(total.saturating_sub(1));
                    let list_rows = inner.height.saturating_sub(4) as usize;
                    for index in selection_range(total, focus, list_rows) {
                        let value = if index == 0 {
                            "default"
                        } else {
                            efforts[index - 1].as_str()
                        };
                        let selected = index == focus;
                        lines.push(Line::styled(
                            format!("{} {value}", if selected { "›" } else { " " }),
                            if selected {
                                Style::default().fg(Color::Black).bg(Color::Cyan)
                            } else {
                                Style::default().fg(Color::White)
                            },
                        ));
                    }
                    lines.push(Line::styled(
                        "↑/↓ move · Enter save · Esc cancel",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                None => {
                    lines.push(Line::raw("Provider did not advertise allowed efforts."));
                    lines.push(Line::from(vec![
                        Span::styled("Value  ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            if input.is_empty() { "default" } else { input },
                            Style::default().fg(Color::White),
                        ),
                    ]));
                    lines.push(Line::styled(
                        "Type a value · Enter save · Esc cancel",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            lines
        }
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn selection_range(total: usize, focus: usize, max_rows: usize) -> std::ops::Range<usize> {
    if total == 0 || max_rows == 0 {
        return 0..0;
    }
    let focus = focus.min(total - 1);
    let visible = total.min(max_rows);
    let start = focus
        .saturating_add(1)
        .saturating_sub(visible)
        .min(total - visible);
    start..start + visible
}

fn draw_skill_picker(frame: &mut Frame<'_>, state: &UiState, area: Rect) {
    let matches = state.matching_skill_names();
    let total = matches.len();
    if total == 0 || area.is_empty() {
        return;
    }

    // The picker is painted last, over the existing transcript and activity;
    // its geometry never participates in the underlying layout.
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let focus = state.skill_picker_focus.min(total - 1);
    let header = Line::styled(
        format!("[{}/{}]", focus + 1, total),
        Style::default().fg(Color::DarkGray),
    );
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, 1),
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
            Rect::new(inner.x, inner.y + 1 + row as u16, inner.width, 1),
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
            TranscriptItem::Reasoning { complete } => {
                let text = if *complete {
                    "Reasoning Complete".to_owned()
                } else {
                    format!("Reasoning... {}", spinner_frame(state))
                };
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

fn activity_line<'a>(state: &UiState, text: &'a str, queued_suffix: Option<&'a str>) -> Line<'a> {
    if state.status != "working" {
        return Line::styled(text, activity_style_for(state));
    }
    let elapsed = state.cursor_epoch.elapsed();
    let character_count = text.chars().count().max(1);
    let mut spans = text
        .chars()
        .enumerate()
        .map(|(index, character)| {
            Span::styled(
                character.to_string(),
                Style::default().fg(working_gradient_color_at(elapsed, index, character_count)),
            )
        })
        .collect::<Vec<_>>();
    if let Some(suffix) = queued_suffix {
        // Queue state is information, not activity: retain a stable style so
        // the broad working sweep never makes it harder to scan.
        spans.push(Span::styled(suffix, Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn activity_style_for(state: &UiState) -> Style {
    if state.status == "compacting" {
        Style::default().fg(PENDING_TOOL_COLOR)
    } else {
        Style::default().fg(Color::Cyan)
    }
}

fn working_gradient_colors_at(position: f64) -> (Color, Color, f64) {
    let position = position.rem_euclid(WORKING_GRADIENT_COLORS.len() as f64);
    let phase = position.floor() as usize;
    let progress = position.fract();
    let start = WORKING_GRADIENT_COLORS[phase];
    let end = WORKING_GRADIENT_COLORS[(phase + 1) % WORKING_GRADIENT_COLORS.len()];
    (
        Color::Rgb(start.0, start.1, start.2),
        Color::Rgb(end.0, end.1, end.2),
        progress,
    )
}

/// Move a continuous warm gradient across the activity text. Offset each
/// character slightly along the same time-based palette, rather than swapping
/// whole character bands at a boundary.
fn working_gradient_color_at(
    elapsed: Duration,
    character_index: usize,
    character_count: usize,
) -> Color {
    const SWEEP_WIDTH: f64 = 0.35;

    let position = elapsed.as_secs_f64() / WORKING_GRADIENT_CYCLE.as_secs_f64()
        + character_index as f64 / character_count.max(1) as f64 * SWEEP_WIDTH;
    let (
        Color::Rgb(red_start, green_start, blue_start),
        Color::Rgb(red_end, green_end, blue_end),
        progress,
    ) = working_gradient_colors_at(position)
    else {
        unreachable!("working gradient palette always uses RGB colours")
    };

    Color::Rgb(
        interpolate_color(red_start, red_end, progress as f32),
        interpolate_color(green_start, green_end, progress as f32),
        interpolate_color(blue_start, blue_end, progress as f32),
    )
}

fn thinking_style() -> Style {
    Style::default().fg(Color::DarkGray)
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
    fn working_activity_uses_a_continuous_warm_gradient() {
        let text = "⠋ working";
        let colors = text
            .chars()
            .enumerate()
            .map(|(index, _)| {
                working_gradient_color_at(Duration::from_millis(2_500), index, text.chars().count())
            })
            .collect::<Vec<_>>();

        assert_eq!(colors.first(), Some(&Color::Rgb(228, 40, 120)));
        assert_eq!(colors.last(), Some(&Color::Rgb(232, 43, 86)));
        assert!(colors.windows(2).all(|pair| pair[0] != pair[1]));

        let (start, end, _) = working_gradient_colors_at(1.0);
        assert_eq!(start, Color::Rgb(235, 45, 65));
        assert_eq!(end, Color::Rgb(255, 130, 25));

        let (start, end, _) = working_gradient_colors_at(3.0);
        assert_eq!(start, Color::Rgb(235, 45, 65));
        assert_eq!(end, Color::Rgb(220, 35, 175));
    }

    #[test]
    fn queued_activity_suffix_is_not_in_the_working_gradient() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.status = "working".to_owned();
        let line = activity_line(&state, "⠋ working", Some(" · queued 1: next task"));
        assert!(line.spans[.."⠋ working".chars().count()]
            .iter()
            .all(|span| span.style.fg != Some(Color::DarkGray)));
        assert_eq!(
            line.spans.last().expect("queue suffix").style.fg,
            Some(Color::DarkGray)
        );
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
    fn busy_input_keeps_normal_style_and_a_blinking_input_cursor() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.busy = true;
        state.cursor_epoch = Instant::now();

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw waiting input");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 6)].fg, USER_BORDER_COLOR);
        assert_eq!(buffer[(1, 7)].symbol(), " ");
        assert_eq!(input_display_text(&state), "");
        terminal.backend_mut().assert_cursor_position((1, 7));
        state.input = "queued message".to_owned();
        assert_eq!(input_display_text(&state), "queued message");
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
    fn built_in_commands_share_the_slash_catalog_without_becoming_skills() {
        assert_eq!(
            command_names(vec!["release-notes".to_owned(), "settings".to_owned()]),
            vec!["exit", "release-notes", "settings"]
        );
        assert_eq!(
            builtin_command("/settings ignored arguments"),
            Some(BuiltinCommand::Settings)
        );
        assert_eq!(builtin_command("  /exit  "), Some(BuiltinCommand::Exit));
        assert_eq!(builtin_command("/settings-extra"), None);
    }

    #[test]
    fn settings_viewport_follows_focus_instead_of_truncating_the_catalog_head() {
        assert_eq!(selection_range(30, 0, 12), 0..12);
        assert_eq!(selection_range(30, 11, 12), 0..12);
        assert_eq!(selection_range(30, 12, 12), 1..13);
        assert_eq!(selection_range(30, 29, 12), 18..30);
    }

    #[test]
    fn model_selection_uses_advertised_efforts_and_preserves_the_current_choice() {
        let mut state = UiState::from_history(&[], "secret", "old", Some("medium"), false);
        state.open_catalog(Ok(vec![ProviderModel {
            id: "openai/gpt-5.6-sol".to_owned(),
            efforts: Some(vec![
                "max".to_owned(),
                "high".to_owned(),
                "medium".to_owned(),
                "low".to_owned(),
            ]),
        }]));
        state.handle_settings_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let SettingsState::Effort { model, focus, .. } =
            state.settings.as_ref().expect("effort picker")
        else {
            panic!("model selection should open the effort picker");
        };
        assert_eq!(model.id, "openai/gpt-5.6-sol");
        assert_eq!(*focus, 3, "default occupies index zero before medium");

        let selected = state
            .handle_settings_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("effort selection");
        assert_eq!(
            selected,
            ("openai/gpt-5.6-sol".to_owned(), Some("medium".to_owned()))
        );
    }

    #[test]
    fn effort_default_selection_does_not_shift_to_the_first_advertised_effort() {
        let mut state = UiState::from_history(&[], "secret", "old", None, false);
        state.open_catalog(Ok(vec![ProviderModel {
            id: "model".to_owned(),
            efforts: Some(vec!["high".to_owned(), "low".to_owned()]),
        }]));
        state.handle_settings_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let selected = state
            .handle_settings_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("default effort selection");
        assert_eq!(selected, ("model".to_owned(), None));
    }

    #[test]
    fn reasoning_indicator_changes_to_complete_and_stays_dark_gray() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.show_thinking();

        let active_lines = transcript_lines(&state, 80);
        let active = active_lines.last().expect("reasoning line");
        assert!(active.to_string().starts_with("Reasoning... "));
        assert_eq!(active.style.fg, Some(Color::DarkGray));

        state.complete_reasoning();
        let complete_lines = transcript_lines(&state, 80);
        let complete = complete_lines.last().expect("complete line");
        assert_eq!(complete.to_string(), "Reasoning Complete");
        assert_eq!(complete.style.fg, Some(Color::DarkGray));
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
    fn focused_builtins_are_distinguished_from_skills() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(command_names(skill_names()));
        state.input = "/se".to_owned();
        state.input_changed();
        assert_eq!(
            state.focused_builtin_command(),
            Some(BuiltinCommand::Settings)
        );

        state.input = "/be".to_owned();
        state.input_changed();
        assert_eq!(state.focused_builtin_command(), None);
    }

    #[test]
    fn selecting_the_focused_skill_leaves_the_completed_command_ready_to_send() {
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
    fn slash_picker_overlays_without_reflowing_the_transcript_when_match_count_changes() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(skill_names());
        let area = Rect::new(0, 0, 40, 16);
        state.transcript = (0..20)
            .map(|index| TranscriptItem::Assistant(format!("message {index}")))
            .collect();

        state.input = "/a".to_owned();
        state.input_changed();
        let (narrow_chat, narrow_picker, narrow_input, _) = ui_layout(&state, area);
        let narrow_scroll = max_scroll_for_area(&state, Size::new(area.width, area.height));

        state.input = "/".to_owned();
        state.input_changed();
        let (broad_chat, broad_picker, broad_input, _) = ui_layout(&state, area);
        let broad_scroll = max_scroll_for_area(&state, Size::new(area.width, area.height));

        assert_ne!(
            narrow_picker, broad_picker,
            "the overlay may fit its contents"
        );
        assert_eq!(narrow_chat, broad_chat);
        assert_eq!(narrow_input, broad_input);
        assert_eq!(
            narrow_scroll, broad_scroll,
            "the overlay does not reduce the transcript viewport"
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
        // The rounded picker is an overlay that ends directly before the input.
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(1, 1)].symbol(), "[");
        assert_eq!(buffer[(1, 2)].symbol(), "/");
        assert_eq!(buffer[(0, 8)].symbol(), "╭");
        assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
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
            .draw(|frame| draw_skill_picker(frame, &state, Rect::new(0, 0, 30, 8)))
            .expect("draw skill picker");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
        assert_eq!(buffer[(1, 1)].symbol(), "[");
        assert_eq!(buffer[(1, 1)].fg, Color::DarkGray);
        assert_eq!(buffer[(1, 2)].symbol(), "/");
        assert_eq!(buffer[(1, 2)].fg, Color::Cyan);
        assert_eq!(buffer[(1, 3)].symbol(), "/");
        assert_eq!(buffer[(1, 3)].fg, Color::DarkGray);
    }
}
