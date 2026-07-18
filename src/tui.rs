use std::collections::{HashMap, HashSet};
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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Terminal;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::app::{Harness, SubagentActivity};
use crate::cancellation::CancellationToken;
use crate::model::{estimate_context_tokens, ChatMessage};
use crate::protocol::{EventSink, ProtocolEvent};
use crate::provider::ProviderModel;
use crate::redaction::redact_secret;
use crate::session::SessionHistoryRecord;

const EVENT_POLL: Duration = Duration::from_millis(50);
const MAX_DISPLAY_INPUT_CHARS: usize = 16 * 1024;
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Maximum number of wrapped input rows the input box grows to before it
/// stops expanding and scrolls its contents internally.
const MAX_INPUT_ROWS: u16 = 12;
const WELCOME_MESSAGE: &str = "Coding Agent Harness LUCY";
const WELCOME_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const WELCOME_TAGLINE: &str = "An ultra-thin harness for tomorrow's most powerful models";
const WELCOME_START_COLOR: (u8, u8, u8) = (0, 180, 180);
const WELCOME_END_COLOR: (u8, u8, u8) = (255, 215, 0);
const USER_BORDER_COLOR: Color = Color::Rgb(192, 154, 0);
const USER_BORDER_GLYPH: &str = "▌";
const PROMPT_BORDER_START_COLOR: (u8, u8, u8) = (0, 190, 185);
const PROMPT_BORDER_END_COLOR: (u8, u8, u8) = (0, 205, 85);
const PENDING_TOOL_COLOR_RGB: (u8, u8, u8) = (255, 165, 0);
const PENDING_TOOL_COLOR: Color = Color::Rgb(
    PENDING_TOOL_COLOR_RGB.0,
    PENDING_TOOL_COLOR_RGB.1,
    PENDING_TOOL_COLOR_RGB.2,
);
/// A completed `cmd` call first retains its pending orange, then sweeps to the
/// final result colour from the left edge of the compact tool line.
const TOOL_RESULT_SWEEP_DURATION: Duration = Duration::from_millis(400);
const TOOL_RESULT_SWEEP_WIDTH: f32 = 4.0;
const TOOL_SUCCESS_GREEN_RGB: (u8, u8, u8) = (0, 128, 0);
const QUEUED_MESSAGE_BACKGROUND: Color = Color::Rgb(0, 38, 38);
const QUEUED_MESSAGE_COLOR: Color = Color::Rgb(150, 255, 245);
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
const SUBAGENT_TASK_PREVIEW_CHARS: usize = 25;
const SUBAGENT_STREAM_MAX_ROWS: usize = 8;
const SUBAGENT_STREAM_MAX_CHARS: usize = 12 * 1024;
const BUILTIN_COMMANDS: [&str; 2] = ["settings", "exit"];
const SUBAGENT_OVERLAY_COLOR: Color = Color::Rgb(255, 0, 255);
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
        while let Some(activity) = harness.next_subagent_activity() {
            let _ = messages.send(WorkerMessage::SubagentActivity(activity));
        }
        if let Some(completion) = harness.next_subagent_completion() {
            let task_id = completion.task_id.clone();
            let result = completion.result.clone();
            let notification = Harness::subagent_notification(&completion);
            let _ = messages.send(WorkerMessage::SubagentCompleted { task_id, result });
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
                while let Some(activity) = harness.next_subagent_activity() {
                    let _ = messages.send(WorkerMessage::SubagentActivity(activity));
                }
                if let Some(completion) = harness.next_subagent_completion() {
                    let task_id = completion.task_id.clone();
                    let result = completion.result.clone();
                    let notification = Harness::subagent_notification(&completion);
                    let _ = messages.send(WorkerMessage::SubagentCompleted { task_id, result });
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
                Ok(WorkerMessage::SubagentCompleted { task_id, result }) => {
                    state.complete_subagent(&task_id, result);
                }
                Ok(WorkerMessage::SubagentActivity(activity)) => {
                    state.apply_subagent_activity(activity);
                }
                Ok(WorkerMessage::Started { cancel, user_text }) => {
                    if let Some(text) = user_text {
                        state.start_queued_user(&text);
                    }
                    state.active_cancel = Some(cancel);
                    state.busy = true;
                    state.set_status("working");
                }
                Ok(WorkerMessage::Thinking) => state.show_thinking(),
                Ok(WorkerMessage::ReasoningCompleted) => state.complete_reasoning(),
                Ok(WorkerMessage::SkillInstructionAttached) => {
                    state.mark_latest_user_skill_attached()
                }
                Ok(WorkerMessage::ContextUsage(tokens)) => state.context_tokens = tokens,
                Ok(WorkerMessage::CompactionStarted) => state.set_status("compacting"),
                Ok(WorkerMessage::CompactionFinished {
                    tokens_before,
                    tokens_after,
                }) => {
                    state.context_tokens = tokens_after;
                    state.set_status("working");
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
                        "cancelling" => state.set_status("사용자 중단"),
                        "finalizing" => state.set_status("ready"),
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
                if state.subagent_focus.is_some() {
                    state.clear_subagent_focus();
                    continue;
                }
                if let Some(token) = state.active_cancel.as_ref() {
                    if token.cancel() {
                        state.set_status("cancelling");
                    }
                }
                continue;
            }
            // While the background-worker list owns focus, swallow typing so it
            // cannot mutate the prompt underneath the stream overlay.
            if state.subagent_focus.is_some()
                && matches!(
                    key.code,
                    KeyCode::Char(_)
                        | KeyCode::Backspace
                        | KeyCode::Enter
                        | KeyCode::Tab
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Home
                        | KeyCode::End
                )
            {
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
                    state.submit_user(&text);
                    state.busy = true;
                    state.set_status("working");
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
                }
                KeyCode::Right => {
                    state.cursor = (state.cursor + 1).min(state.input.chars().count());
                }
                KeyCode::Home => {
                    state.cursor = 0;
                }
                KeyCode::End => {
                    state.cursor = state.input.chars().count();
                }
                KeyCode::Up => {
                    if state.move_skill_picker(false) {
                        // skill picker owns the key
                    } else if state.subagent_focus.is_some() {
                        let _ = state.move_subagent_focus(false);
                    } else {
                        let size = terminal
                            .size()
                            .map_err(|error| format!("unable to read terminal size: {error}"))?;
                        let area = tui_viewport(Rect::new(0, 0, size.width, size.height));
                        let input_width = area.width.saturating_sub(2).max(1) as usize;
                        if input_cursor_on_first_row(state, input_width)
                            && state.focus_subagent_list_from_input()
                        {
                            // the list is physically above the prompt
                        } else if !move_input_cursor_vertical(state, input_width, false) {
                            let max_scroll = max_scroll_for_area(state, size);
                            scroll_up(state, max_scroll);
                        }
                    }
                }
                KeyCode::Down => {
                    if state.move_skill_picker(true) {
                        // skill picker owns the key
                    } else if state.subagent_focus.is_some() {
                        let _ = state.move_subagent_focus(true);
                    } else {
                        let size = terminal
                            .size()
                            .map_err(|error| format!("unable to read terminal size: {error}"))?;
                        let area = tui_viewport(Rect::new(0, 0, size.width, size.height));
                        let input_width = area.width.saturating_sub(2).max(1) as usize;
                        if !move_input_cursor_vertical(state, input_width, true) {
                            let max_scroll = max_scroll_for_area(state, size);
                            scroll_down(state, max_scroll);
                        }
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
    SubagentCompleted {
        task_id: String,
        result: Value,
    },
    SubagentActivity(SubagentActivity),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentStatus {
    Queued,
    Running,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
enum SubagentStreamItem {
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
}

#[derive(Debug, Clone, PartialEq)]
struct SubagentTask {
    call_id: String,
    task_id: Option<String>,
    task: String,
    model: Option<String>,
    effort: Option<String>,
    status: SubagentStatus,
    result: Option<Value>,
    creation_completed: bool,
    stream: Vec<SubagentStreamItem>,
    stream_chars: usize,
}

/// A bounded interpolation between the resting bars and a live pulse frame.
/// Keeping the source frame lets a completed turn settle instead of snapping
/// straight from its last pulse height to the resting indicator.
#[derive(Debug, Clone)]
struct ActivityTransition {
    started_at: Instant,
    from_levels: [usize; PULSE_BAR_PERIODS.len()],
    to_levels: [usize; PULSE_BAR_PERIODS.len()],
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
    tool_animation_epoch: Instant,
    activity_started_at: Instant,
    activity_transition: Option<ActivityTransition>,
    last_active_levels: [usize; PULSE_BAR_PERIODS.len()],
    last_active_elapsed: Duration,
    welcome_visible: bool,
    attached_agents: Vec<String>,
    subagents: Vec<SubagentTask>,
    subagent_focus: Option<usize>,
    completed_subagent_calls: HashSet<String>,
    failed_subagent_calls: HashSet<String>,
    cmd_result_started_at: HashMap<String, Instant>,
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
            tool_animation_epoch: Instant::now(),
            activity_started_at: Instant::now(),
            activity_transition: None,
            last_active_levels: [0; PULSE_BAR_PERIODS.len()],
            last_active_elapsed: Duration::ZERO,
            welcome_visible: !resumed && history.is_empty(),
            attached_agents: Vec::new(),
            subagents: Vec::new(),
            subagent_focus: None,
            completed_subagent_calls: HashSet::new(),
            failed_subagent_calls: HashSet::new(),
            cmd_result_started_at: HashMap::new(),
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

    fn set_status(&mut self, status: impl Into<String>) {
        let status = status.into();
        if self.status == status {
            return;
        }

        let now = Instant::now();
        let current_levels = self.activity_levels_at(now);
        let current_elapsed = self.working_elapsed_at(now);
        if matches!(self.status.as_str(), "working" | "compacting") {
            self.last_active_levels = current_levels;
            self.last_active_elapsed = current_elapsed;
        }

        match status.as_str() {
            "working" if !matches!(self.status.as_str(), "working" | "compacting") => {
                // Join a frame whose next pulses continue one level at a time
                // after the ramp. Sampling the current bars also makes a new
                // turn during the ready settle-down phase continuous.
                self.activity_started_at = now;
                self.activity_transition = Some(ActivityTransition {
                    started_at: now,
                    from_levels: current_levels,
                    to_levels: pulse_levels_at(PULSE_ENTRY_FRAME),
                });
            }
            "ready" if self.status != "ready" => {
                // TurnEnd is commonly followed by Finished before the next
                // draw, so retain the most recent working frame even if the
                // transient status was already changed to "finalizing".
                let from_levels = if matches!(self.status.as_str(), "working" | "compacting") {
                    current_levels
                } else {
                    self.last_active_levels
                };
                self.activity_transition = Some(ActivityTransition {
                    started_at: now,
                    from_levels,
                    to_levels: [0; PULSE_BAR_PERIODS.len()],
                });
            }
            _ => {}
        }
        self.status = status;
    }

    fn activity_levels_at(&self, now: Instant) -> [usize; PULSE_BAR_PERIODS.len()] {
        if let Some(transition) = &self.activity_transition {
            let elapsed = now.saturating_duration_since(transition.started_at);
            if elapsed < ACTIVITY_TRANSITION_DURATION {
                return interpolate_pulse_levels(
                    transition.from_levels,
                    transition.to_levels,
                    elapsed,
                );
            }
        }

        match self.status.as_str() {
            "working" | "compacting" => pulse_levels_at(self.working_elapsed_at(now)),
            _ => [0; PULSE_BAR_PERIODS.len()],
        }
    }

    fn working_elapsed_at(&self, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.activity_started_at);
        if self.status == "working" && self.activity_transition.is_some() {
            PULSE_ENTRY_FRAME
                .checked_add(elapsed.saturating_sub(ACTIVITY_TRANSITION_DURATION))
                .unwrap_or(PULSE_ENTRY_FRAME)
        } else {
            elapsed
        }
    }

    fn input_changed(&mut self) {
        self.reset_skill_picker();
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
                    "⚙ {model} ({})",
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
                let text = message.content.as_deref().unwrap_or("");
                if let Some((task_id, result)) = parse_subagent_notification(text) {
                    self.complete_subagent(&task_id, result);
                } else {
                    let secret = self.secret.clone();
                    self.add_user(text, &secret);
                }
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

    /// Show an idle submission in the transcript immediately. Only a turn
    /// submitted while another turn is active needs the visible queue.
    fn submit_user(&mut self, text: &str) {
        if self.busy {
            self.queue_user(text);
        } else {
            self.add_user(text, &self.secret.clone());
        }
    }

    fn queue_user(&mut self, text: &str) {
        self.queued_messages
            .push(redact_secret(text, Some(&self.secret)));
    }

    fn start_queued_user(&mut self, text: &str) {
        let safe = redact_secret(text, Some(&self.secret));
        let queued = if self.queued_messages.first() == Some(&safe) {
            self.queued_messages.remove(0);
            true
        } else if let Some(index) = self
            .queued_messages
            .iter()
            .position(|queued| queued == &safe)
        {
            self.queued_messages.remove(index);
            true
        } else {
            false
        };
        if queued {
            self.add_user(text, &self.secret.clone());
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
        if matches!(
            self.transcript.last(),
            Some(TranscriptItem::Reasoning { complete: false })
        ) {
            self.transcript.pop();
        }
    }

    fn show_thinking(&mut self) {
        self.set_status("working");
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
        if call.name == "spawn_subagent" {
            self.register_subagent_call(&call.id, &call.arguments);
        }
    }

    fn add_tool_result(&mut self, id: &str, name: &str, result: Value) {
        self.record_tool_result(id, name, result, false);
    }

    fn add_live_tool_result(&mut self, id: &str, name: &str, result: Value) {
        self.record_tool_result(id, name, result, true);
    }

    fn record_tool_result(&mut self, id: &str, name: &str, result: Value, animate: bool) {
        if name == "spawn_subagent" {
            self.update_subagent_queued(id, &result);
        }
        if animate && name == "cmd" {
            self.cmd_result_started_at
                .insert(id.to_owned(), Instant::now());
        }
        self.transcript.push(TranscriptItem::ToolResult {
            id: id.to_owned(),
            name: name.to_owned(),
            result,
        });
    }

    fn register_subagent_call(&mut self, call_id: &str, arguments: &str) {
        if self.subagents.iter().any(|task| task.call_id == call_id) {
            return;
        }
        self.completed_subagent_calls.remove(call_id);
        let parsed = serde_json::from_str::<Value>(arguments).ok();
        let task = parsed
            .as_ref()
            .and_then(|value| value.get("task"))
            .and_then(Value::as_str)
            .map(|task| redact_secret(task.trim(), Some(&self.secret)))
            .filter(|task| !task.is_empty())
            .unwrap_or_else(|| "invalid task".to_owned());
        let model = parsed
            .as_ref()
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            .map(|value| redact_secret(value, Some(&self.secret)));
        let effort = parsed
            .as_ref()
            .and_then(|value| value.get("effort"))
            .and_then(Value::as_str)
            .map(|value| redact_secret(value, Some(&self.secret)));
        self.subagents.push(SubagentTask {
            call_id: call_id.to_owned(),
            task_id: None,
            task,
            model,
            effort,
            status: SubagentStatus::Queued,
            result: None,
            creation_completed: false,
            stream: Vec::new(),
            stream_chars: 0,
        });
    }

    fn update_subagent_queued(&mut self, call_id: &str, result: &Value) {
        let Some(task) = self
            .subagents
            .iter_mut()
            .find(|task| task.call_id == call_id)
        else {
            return;
        };
        task.task_id = result
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        task.status = if result.get("error").is_some() {
            task.creation_completed = false;
            SubagentStatus::Failed
        } else {
            // Creation is complete once the queued acknowledgement carries a
            // task id; the worker itself remains live in the list.
            task.creation_completed = task.task_id.is_some();
            SubagentStatus::Running
        };
        task.result = Some(result.clone());
    }

    fn complete_subagent(&mut self, task_id: &str, result: Value) {
        // Completion is delivered to the main agent through the notification
        // message. The task list is only a live background-work view, so do
        // not retain finished workers there. Keep the call identity separately
        // so its transcript line can still say "completed" after removal.
        let removed_index = self
            .subagents
            .iter()
            .position(|task| task.task_id.as_deref() == Some(task_id));
        if let Some(index) = removed_index {
            let call_id = self.subagents[index].call_id.clone();
            let failed = result.get("error").is_some()
                || result.get("cancelled").is_some()
                || result.get("interrupted").is_some();
            if failed {
                self.failed_subagent_calls.insert(call_id);
            } else {
                self.completed_subagent_calls.insert(call_id);
            }
            self.subagents.remove(index);
            self.subagent_focus = match self.subagent_focus {
                None => None,
                Some(focus) if self.subagents.is_empty() => None,
                Some(focus) if focus > index => Some(focus - 1),
                Some(focus) if focus == index => Some(focus.min(self.subagents.len() - 1)),
                Some(focus) => Some(focus),
            };
        }
    }

    fn apply_subagent_activity(&mut self, activity: SubagentActivity) {
        if let SubagentActivity::Completed { task_id, result } = activity {
            self.complete_subagent(&task_id, result);
            return;
        }
        let (task_id, item, chars) = match activity {
            SubagentActivity::AssistantDelta { task_id, text } => {
                let text = redact_secret(&text, Some(&self.secret));
                let chars = text.chars().count();
                (task_id, SubagentStreamItem::Assistant(text), chars)
            }
            SubagentActivity::ToolCall {
                task_id,
                id,
                name,
                arguments,
            } => {
                let arguments = redact_secret(&arguments, Some(&self.secret));
                let chars = arguments.chars().count() + name.len() + id.len();
                (
                    task_id,
                    SubagentStreamItem::ToolCall {
                        id,
                        name,
                        arguments,
                    },
                    chars,
                )
            }
            SubagentActivity::ToolResult {
                task_id,
                id,
                name,
                result,
            } => {
                let encoded = result.to_string();
                let chars = encoded.chars().count() + name.len() + id.len();
                (
                    task_id,
                    SubagentStreamItem::ToolResult { id, name, result },
                    chars,
                )
            }
            SubagentActivity::Completed { .. } => {
                unreachable!("completed activity is handled above")
            }
        };
        let Some(task) = self
            .subagents
            .iter_mut()
            .find(|task| task.task_id.as_deref() == Some(task_id.as_str()))
        else {
            return;
        };
        // Merge consecutive assistant deltas into one stream item.
        if let SubagentStreamItem::Assistant(delta) = &item {
            if let Some(SubagentStreamItem::Assistant(existing)) = task.stream.last_mut() {
                existing.push_str(delta);
                task.stream_chars = task.stream_chars.saturating_add(chars);
                trim_subagent_stream(task);
                return;
            }
        }
        task.stream.push(item);
        task.stream_chars = task.stream_chars.saturating_add(chars);
        trim_subagent_stream(task);
    }

    fn clear_subagent_focus(&mut self) {
        self.subagent_focus = None;
    }

    fn focus_subagent_list_from_input(&mut self) -> bool {
        if self.subagents.is_empty() {
            return false;
        }
        self.subagent_focus = Some(self.subagents.len() - 1);
        true
    }

    fn move_subagent_focus(&mut self, down: bool) -> bool {
        let Some(focus) = self.subagent_focus else {
            return false;
        };
        if self.subagents.is_empty() {
            self.subagent_focus = None;
            return true;
        }
        if down {
            if focus + 1 >= self.subagents.len() {
                self.subagent_focus = None;
            } else {
                self.subagent_focus = Some(focus + 1);
            }
        } else if focus > 0 {
            self.subagent_focus = Some(focus - 1);
        }
        true
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
                self.add_live_tool_result(&id, &name, result)
            }
            ProtocolEvent::TurnEnd => {
                self.complete_reasoning();
                self.set_status("finalizing");
                self.transcript
                    .push(TranscriptItem::Info("✓ turn complete".to_owned()));
            }
            ProtocolEvent::TurnInterrupted { reason, phase } => {
                self.complete_reasoning();
                self.set_status("cancelling");
                self.transcript
                    .push(TranscriptItem::Info(format!("! {reason} ({phase})")));
            }
            ProtocolEvent::Error { message } => {
                self.complete_reasoning();
                self.set_status("error");
                self.transcript.push(TranscriptItem::Error(message));
            }
        }
    }
}

fn trim_subagent_stream(task: &mut SubagentTask) {
    while task.stream_chars > SUBAGENT_STREAM_MAX_CHARS && !task.stream.is_empty() {
        let removed = match task.stream.remove(0) {
            SubagentStreamItem::Assistant(text) => text.chars().count(),
            SubagentStreamItem::ToolCall {
                id,
                name,
                arguments,
            } => id.len() + name.len() + arguments.chars().count(),
            SubagentStreamItem::ToolResult { id, name, result } => {
                id.len() + name.len() + result.to_string().chars().count()
            }
        };
        task.stream_chars = task.stream_chars.saturating_sub(removed);
    }
}

fn parse_subagent_notification(text: &str) -> Option<(String, Value)> {
    let prefix = "Background subagent ";
    let suffix = ". Deliver this result to the user and continue the task: ";
    let rest = text.strip_prefix(prefix)?;
    let (header, encoded_result) = rest.split_once(suffix)?;
    let (task_id, status) = header.split_once(' ')?;
    if task_id.is_empty() || !matches!(status, "completed" | "failed" | "canceled" | "interrupted")
    {
        return None;
    }
    Some((
        task_id.to_owned(),
        serde_json::from_str(encoded_result).ok()?,
    ))
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

#[cfg(test)]
fn activity_text(state: &UiState) -> String {
    activity_text_at(state, Instant::now())
}

fn activity_text_at(state: &UiState, now: Instant) -> String {
    match state.status.as_str() {
        // Keep the flame visible while entering or leaving busy mode, but hide
        // it completely once the idle settle-down transition has finished.
        "working" | "compacting" => pulse_frame(state.activity_levels_at(now)),
        "ready"
            if state
                .activity_transition
                .as_ref()
                .is_some_and(|transition| transition_progress(now, transition) < 1.0) =>
        {
            pulse_frame(state.activity_levels_at(now))
        }
        "ready" => String::new(),
        _ => format!("● {}", state.status),
    }
}

/// Reserve one terminal cell on both sides of the TUI so every rendered
/// surface shares the same breathing room. Extremely narrow terminals retain
/// their full width because two margins would leave no usable content area.
fn tui_viewport(area: Rect) -> Rect {
    if area.width > 2 {
        Rect::new(area.x + 1, area.y, area.width - 2, area.height)
    } else {
        area
    }
}

fn ui_layout(
    state: &UiState,
    area: Rect,
) -> (Rect, Option<Rect>, Option<Rect>, Option<Rect>, Rect, Rect) {
    let prompt_rows = input_visible_rows(state, area.width.saturating_sub(2));
    let list_height = subagent_list_height(state);
    let queue_height = message_queue_height(state);
    let input_height = prompt_rows.clamp(1, MAX_INPUT_ROWS) + 2 + list_height;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(queue_height),
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
    let stream_area = subagent_stream_overlay_area(state, chunks[1], chunks[2]);
    let input_area = chunks[2];
    let queue_area = (queue_height > 0).then(|| {
        Rect::new(
            input_area.x.saturating_add(1),
            chunks[1].y,
            input_area.width.saturating_sub(2),
            chunks[1].height,
        )
    });
    (
        chunks[0],
        picker_area,
        stream_area,
        queue_area,
        input_area,
        chunks[3],
    )
}

fn prompt_area(input_area: Rect, state: &UiState) -> Rect {
    let list_height = subagent_list_height(state);
    let prompt_height =
        input_visible_rows(state, input_area.width.saturating_sub(2)).clamp(1, MAX_INPUT_ROWS) + 2;
    Rect::new(
        input_area.x,
        input_area.y.saturating_add(list_height),
        input_area.width,
        prompt_height.min(input_area.height.saturating_sub(list_height)),
    )
}

fn subagent_list_area(state: &UiState, input_area: Rect) -> Option<Rect> {
    let height = subagent_list_height(state);
    (height > 0).then(|| Rect::new(input_area.x, input_area.y, input_area.width, height))
}

fn subagent_list_height(state: &UiState) -> u16 {
    state.subagents.len().min(u16::MAX as usize) as u16
}

/// Focused worker output uses the same transient slot as the slash picker:
/// immediately above the input, without changing the transcript viewport.
fn subagent_stream_overlay_area(
    state: &UiState,
    queue_area: Rect,
    input_area: Rect,
) -> Option<Rect> {
    let focus = state.subagent_focus?;
    let task = state.subagents.get(focus)?;
    let stream_rows = task.stream.len().clamp(1, SUBAGENT_STREAM_MAX_ROWS) as u16;
    let height = stream_rows.saturating_add(2);
    let y = queue_area.y.saturating_sub(height);
    Some(Rect::new(input_area.x, y, input_area.width, height))
}

fn message_queue_height(state: &UiState) -> u16 {
    state.queued_messages.len().min(u16::MAX as usize) as u16
}

fn max_scroll_for_area(state: &UiState, size: Size) -> u16 {
    let area = tui_viewport(Rect::new(0, 0, size.width, size.height));
    let (chat_chunk, _, _, _, _, _) = ui_layout(state, area);
    let chat_height = chat_chunk.height;
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
        // Header, visible commands, and the top/bottom border.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputVisualRow {
    start: usize,
    end: usize,
}

fn input_visual_rows(input: &str, width: usize) -> Vec<InputVisualRow> {
    let width = width.max(1);
    let characters = input.chars().collect::<Vec<_>>();
    let mut rows = Vec::new();
    let mut start = 0;
    let mut row_width = 0;

    for (index, character) in characters.iter().enumerate() {
        if *character == '\n' {
            rows.push(InputVisualRow { start, end: index });
            start = index + 1;
            row_width = 0;
            continue;
        }

        let character_width = unicode_width::UnicodeWidthChar::width(*character).unwrap_or(0);
        if row_width + character_width > width && index > start {
            rows.push(InputVisualRow { start, end: index });
            start = index;
            row_width = 0;
        }
        row_width += character_width;
    }

    rows.push(InputVisualRow {
        start,
        end: characters.len(),
    });
    rows
}

fn input_cursor_row(input: &str, cursor: usize, width: usize) -> usize {
    let rows = input_visual_rows(input, width);
    let cursor = cursor.min(input.chars().count());
    for (index, row) in rows.iter().enumerate() {
        if cursor < row.end {
            return index;
        }
        if cursor == row.end && rows.get(index + 1).is_none_or(|next| next.start != cursor) {
            return index;
        }
    }
    rows.len().saturating_sub(1)
}

fn input_cursor_on_first_row(state: &UiState, width: usize) -> bool {
    input_cursor_row(&state.input, state.cursor, width) == 0
}

fn cursor_row(input: &str, cursor: usize, width: usize) -> u16 {
    input_cursor_row(input, cursor, width).min(u16::MAX as usize) as u16
}

fn move_input_cursor_vertical(state: &mut UiState, width: usize, down: bool) -> bool {
    let width = width.max(1);
    let rows = input_visual_rows(&state.input, width);
    let current_row = input_cursor_row(&state.input, state.cursor, width);
    let target_row = if down {
        current_row + 1
    } else {
        current_row.saturating_sub(1)
    };
    if target_row == current_row || target_row >= rows.len() {
        return false;
    }

    let characters = state.input.chars().collect::<Vec<_>>();
    let current = rows[current_row];
    let cursor = state.cursor.min(current.end);
    let desired_column = characters[current.start..cursor]
        .iter()
        .map(|character| unicode_width::UnicodeWidthChar::width(*character).unwrap_or(0))
        .sum::<usize>();
    let target = rows[target_row];
    let mut column = 0;
    let mut target_cursor = target.end;
    for (index, character) in characters
        .iter()
        .enumerate()
        .take(target.end)
        .skip(target.start)
    {
        let character_width = unicode_width::UnicodeWidthChar::width(*character).unwrap_or(0);
        if column + character_width > desired_column {
            target_cursor = index;
            break;
        }
        column += character_width;
        if column >= desired_column {
            target_cursor = index + 1;
            break;
        }
    }
    state.cursor = target_cursor;
    true
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
    let full_area = frame.area();
    // Clear the outer gutters too, so a resize or overlay cannot leave stale
    // cells in the one-column margins.
    frame.render_widget(Clear, full_area);
    let area = tui_viewport(full_area);
    let (chat_chunk, picker_area, overlay_area, queue_area, input_chunk, status_area) =
        ui_layout(state, area);

    // Queued user messages occupy a strip above the bordered prompt; active
    // background tasks float over the upper-right transcript.
    let visible_chat_area = chat_chunk;

    let width = chat_chunk.width;
    if state.welcome_visible {
        let welcome_lines = welcome_lines(&state.attached_agents);
        // Reserve the bottom row for the version so it remains visually
        // separated from the title, tagline, and attached instructions.
        let content_height = visible_chat_area.height.saturating_sub(2);
        let welcome_height = (welcome_lines.len() as u16).min(content_height);
        let welcome_area = Rect::new(
            visible_chat_area.x,
            visible_chat_area.y + content_height.saturating_sub(welcome_height) / 2,
            visible_chat_area.width,
            welcome_height,
        );
        let welcome = Paragraph::new(welcome_lines).alignment(Alignment::Center);
        frame.render_widget(welcome, welcome_area);
        if visible_chat_area.height > 0 {
            let version = Paragraph::new(Line::styled(
                WELCOME_VERSION,
                Style::default().fg(Color::DarkGray),
            ))
            .alignment(Alignment::Center);
            frame.render_widget(
                version,
                Rect::new(
                    visible_chat_area.x,
                    visible_chat_area.y + visible_chat_area.height - 1,
                    visible_chat_area.width,
                    1,
                ),
            );
        }
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

    let activity_now = Instant::now();
    let activity_text = activity_text_at(state, activity_now);
    let activity_elapsed = state.working_elapsed_at(activity_now);
    if let Some(picker_area) = picker_area {
        draw_skill_picker(frame, state, picker_area);
    }

    if let Some(list_area) = subagent_list_area(state, input_chunk) {
        draw_subagent_list(frame, state, list_area);
    }

    let input_text_style = Style::default().fg(Color::White);
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Plain);
    let prompt_chunk = prompt_area(input_chunk, state);
    let prompt_area = input_block.inner(prompt_chunk);
    let prompt = input_display_text(state);
    let input_rows =
        input_visible_rows(state, prompt_chunk.width.saturating_sub(2)).clamp(1, MAX_INPUT_ROWS);
    let wrapped = wrap_text(&prompt, prompt_area.width.max(1) as usize);
    let visible = (wrapped.len() as u16).clamp(1, input_rows);
    let cursor_row = cursor_row(&prompt, state.cursor, prompt_area.width.max(1) as usize);
    let bottom_scroll = (wrapped.len() as u16).saturating_sub(visible);
    let cursor_scroll = (cursor_row + 1).saturating_sub(visible);
    let input_scroll = bottom_scroll.min(cursor_scroll);
    let active_skill_trigger = (!state.busy)
        .then(|| active_skill_trigger(&prompt, &state.skill_names))
        .flatten();
    let input_lines = styled_text_lines(
        &prompt,
        active_skill_trigger,
        prompt_area.width.max(1) as usize,
        input_text_style,
    );
    frame.render_widget(input_block, prompt_chunk);
    if let Some(queue_area) = queue_area {
        draw_message_queue(frame, state, queue_area);
    }
    let input = Paragraph::new(input_lines)
        .style(input_text_style)
        .scroll((input_scroll, 0));
    frame.render_widget(input, prompt_area);
    draw_prompt_border_gradient(frame, prompt_chunk);

    let effort = state.effort.as_deref().unwrap_or("default");
    let context_text = context_status_text(state);
    let context_width = UnicodeWidthStr::width(context_text.as_str()) as u16;
    let context_area_width = context_width.min(status_area.width);
    let status_left_width = status_area.width.saturating_sub(context_area_width);
    let status_left_area = Rect::new(
        status_area.x,
        status_area.y,
        status_left_width,
        status_area.height,
    );
    if !status_left_area.is_empty() {
        let status_line = model_status_line(
            state,
            effort,
            &activity_text,
            activity_elapsed,
            activity_now,
        );
        frame.render_widget(Paragraph::new(status_line), status_left_area);
    }

    if context_area_width > 0 {
        let context_area = Rect::new(
            status_area
                .x
                .saturating_add(status_area.width.saturating_sub(context_area_width)),
            status_area.y,
            context_area_width,
            status_area.height,
        );
        let context = Paragraph::new(context_text)
            .alignment(Alignment::Right)
            .style(context_status_style(state));
        frame.render_widget(context, context_area);
    }

    // The task overlay is a floating surface: draw it after the transcript,
    // picker, input, and status layers so none can cut through its left border
    // or upper-left corner on a constrained terminal layout.
    if let Some(overlay_area) = overlay_area {
        draw_subagent_stream_overlay(frame, state, overlay_area);
    }
    if let Some(settings) = &state.settings {
        draw_settings(frame, settings, area);
    }

    // Keep the terminal cursor anchored to the input on every frame. Terminal
    // IMEs place their uncommitted CJK composition at the hardware cursor; a
    // blink frame that hides it can otherwise leave that composition beside a
    // freshly rendered status indicator instead of the prompt.
    if state.settings.is_none() && !prompt_area.is_empty() && visible > 0 {
        let cursor_prefix: String = prompt.chars().take(state.cursor).collect();
        let cursor_rows = wrap_text(&cursor_prefix, prompt_area.width.max(1) as usize);
        let cursor_line = cursor_rows.last().map(String::as_str).unwrap_or("");
        let cursor_offset = UnicodeWidthStr::width(cursor_line) as u16;
        let cursor_x = prompt_area.x + cursor_offset.min(prompt_area.width.saturating_sub(1));
        let cursor_y = prompt_area.y + cursor_row.saturating_sub(input_scroll);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_message_queue(frame: &mut Frame<'_>, state: &UiState, area: Rect) {
    if area.is_empty() {
        return;
    }

    let total = state.queued_messages.len();
    let lines = state
        .queued_messages
        .iter()
        .map(|message| Line::raw(format!("Queued {total}: {}", single_line_preview(message))))
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(QUEUED_MESSAGE_COLOR)
                .bg(QUEUED_MESSAGE_BACKGROUND),
        ),
        area,
    );
}

const SUBAGENT_ID_COLORS: [Color; 8] = [
    Color::Rgb(255, 128, 128),
    Color::Rgb(255, 190, 90),
    Color::Rgb(255, 235, 100),
    Color::Rgb(120, 225, 150),
    Color::Rgb(100, 220, 220),
    Color::Rgb(120, 175, 255),
    Color::Rgb(190, 145, 255),
    Color::Rgb(255, 130, 220),
];

fn subagent_id_color(id: &str) -> Color {
    let hash = id.bytes().fold(0x811c9dc5u32, |hash, byte| {
        hash.wrapping_mul(0x01000193) ^ u32::from(byte)
    });
    SUBAGENT_ID_COLORS[(hash as usize) % SUBAGENT_ID_COLORS.len()]
}

fn draw_subagent_list(frame: &mut Frame<'_>, state: &UiState, area: Rect) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    for (index, task) in state.subagents.iter().enumerate() {
        let selected = state.subagent_focus == Some(index);
        let id = task.task_id.as_deref().unwrap_or(&task.call_id);
        let mut style = Style::default().fg(subagent_id_color(id));
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        let model = task.model.as_deref().unwrap_or("session");
        let effort = task.effort.as_deref().unwrap_or("default");
        let preview = truncate_chars(
            &task.task.replace(['\n', '\r'], " ↵ "),
            SUBAGENT_TASK_PREVIEW_CHARS,
        );
        let line = format!("{id} · {model} · {effort} · {preview}");
        frame.render_widget(
            Paragraph::new(Line::styled(line, style)),
            Rect::new(area.x, area.y + index as u16, area.width, 1),
        );
    }
}

fn draw_subagent_stream_overlay(frame: &mut Frame<'_>, state: &UiState, area: Rect) {
    let Some(index) = state.subagent_focus else {
        return;
    };
    let Some(task) = state.subagents.get(index) else {
        return;
    };
    if area.is_empty() {
        return;
    }
    let style = Style::default().fg(SUBAGENT_OVERLAY_COLOR);
    let id = task.task_id.as_deref().unwrap_or("pending");
    let block = Block::default()
        .title(format!("Subagent {id}"))
        .borders(Borders::ALL)
        .style(style)
        .border_style(style);
    let inner = block.inner(area);
    let mut lines = Vec::new();
    if task.stream.is_empty() {
        lines.push(Line::styled("waiting for worker output", style));
    } else {
        for item in task
            .stream
            .iter()
            .rev()
            .take(SUBAGENT_STREAM_MAX_ROWS)
            .rev()
        {
            let line = match item {
                SubagentStreamItem::Assistant(text) => {
                    format!("assistant  {}", single_line_preview(text))
                }
                SubagentStreamItem::ToolCall {
                    name, arguments, ..
                } => format!("→ {name}  {}", call_arguments(arguments)),
                SubagentStreamItem::ToolResult { name, result, .. } => {
                    format!("← {name}  {}", format_tool_result(result))
                }
            };
            lines.push(Line::styled(
                redact_secret(&line, Some(&state.secret)),
                style,
            ));
        }
    }
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).style(style), inner);
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut value: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        value.push('…');
    }
    value
}

fn single_line_preview(text: &str) -> String {
    truncate_output(&text.replace(['\n', '\r'], " ↵ "))
}

fn subagent_status_label(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Queued => "queued",
        SubagentStatus::Running => "running",
        SubagentStatus::Failed => "error",
    }
}

fn subagent_status_display(status: SubagentStatus, state: &UiState) -> String {
    let label = subagent_status_label(status);
    if status == SubagentStatus::Running {
        format!("{label} {}", tool_spinner_frame(state))
    } else {
        label.to_owned()
    }
}

fn subagent_status_style(status: SubagentStatus) -> Style {
    match status {
        SubagentStatus::Queued => Style::default().fg(PENDING_TOOL_COLOR),
        SubagentStatus::Running => Style::default().fg(Color::Cyan),
        SubagentStatus::Failed => Style::default().fg(Color::Red),
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
        .border_type(BorderType::Plain)
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
                let result = matching_tool_result(&state.transcript, index, id);
                let segments = if name == "cmd" {
                    cmd_tool_segments(id, arguments, result, state)
                } else if name == "spawn_subagent" {
                    subagent_tool_segments(id, arguments, state)
                } else if name == "check_subagent" {
                    check_subagent_tool_segments(arguments, result, state)
                } else {
                    generic_tool_segments(name, arguments, result, state)
                };
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

/// Tool work can outlive a main-agent turn (for example, background
/// subagents), so it uses its own clock instead of the main status animation.
fn running_tool_status(state: &UiState) -> String {
    tool_spinner_frame(state)
}

fn cmd_tool_segments(
    call_id: &str,
    arguments: &str,
    result: Option<&Value>,
    state: &UiState,
) -> Vec<(String, Style)> {
    let command = redact_secret(&command_display(arguments), Some(&state.secret));
    if let Some(result) = result {
        let (icon, status, status_style) = cmd_result_status(result);
        if status == "done" || state.cmd_result_started_at.contains_key(call_id) {
            let text = if status == "done" {
                format!("{icon} cmd  $ {command}")
            } else {
                format!("{icon} cmd  $ {command}  → {status}")
            };
            return cmd_result_segments(call_id, &text, cmd_result_target_color(result), state);
        }
        vec![
            (format!("{icon} cmd  $ {command}  → "), status_style),
            (status, status_style),
        ]
    } else {
        vec![
            (format!("· cmd  $ {command}  "), pending_tool_call_style()),
            (running_tool_status(state), pending_tool_call_style()),
        ]
    }
}

/// During the brief post-result window, turn the compact `cmd` line from the
/// pending orange into its final result colour one character at a time. A few
/// adjacent characters blend at the leading edge so the visual is a true
/// gradient, rather than a hard colour boundary.
fn cmd_result_segments(
    call_id: &str,
    text: &str,
    target: Color,
    state: &UiState,
) -> Vec<(String, Style)> {
    let now = Instant::now();
    let Some(started_at) = state.cmd_result_started_at.get(call_id).copied() else {
        return vec![(text.to_owned(), Style::default().fg(target))];
    };
    if now.saturating_duration_since(started_at) >= TOOL_RESULT_SWEEP_DURATION {
        return vec![(text.to_owned(), Style::default().fg(target))];
    }

    let character_count = text.chars().count();
    text.chars()
        .enumerate()
        .map(|(index, character)| {
            (
                character.to_string(),
                Style::default().fg(cmd_result_color_at(
                    started_at,
                    now,
                    index,
                    character_count,
                    target,
                )),
            )
        })
        .collect()
}

fn cmd_result_color_at(
    started_at: Instant,
    now: Instant,
    character_index: usize,
    character_count: usize,
    target: Color,
) -> Color {
    let elapsed = now.saturating_duration_since(started_at);
    if elapsed >= TOOL_RESULT_SWEEP_DURATION {
        return target;
    }

    let progress = elapsed.as_secs_f32() / TOOL_RESULT_SWEEP_DURATION.as_secs_f32();
    // Start the leading blend at the left edge and carry it past the last
    // character, so no character has to jump to its settled result colour.
    let sweep_distance = character_count as f32 + TOOL_RESULT_SWEEP_WIDTH - 1.0;
    let front = progress * sweep_distance;
    let character_progress =
        ((front - character_index as f32) / TOOL_RESULT_SWEEP_WIDTH).clamp(0.0, 1.0);
    let (target_red, target_green, target_blue) = tool_result_color_rgb(target);
    Color::Rgb(
        interpolate_color(PENDING_TOOL_COLOR_RGB.0, target_red, character_progress),
        interpolate_color(PENDING_TOOL_COLOR_RGB.1, target_green, character_progress),
        interpolate_color(PENDING_TOOL_COLOR_RGB.2, target_blue, character_progress),
    )
}

fn command_display(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(|command| truncate_tool_call(&command))
        .unwrap_or_else(|| truncate_tool_call(arguments))
}

fn cmd_result_target_color(result: &Value) -> Color {
    if result
        .get("canceled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || result
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Color::Yellow;
    }
    if result.get("error").is_some()
        || matches!(result.get("exit_code").and_then(Value::as_i64), Some(code) if code != 0)
    {
        return Color::Red;
    }
    Color::Green
}

fn tool_result_color_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Green => TOOL_SUCCESS_GREEN_RGB,
        Color::Red => (255, 0, 0),
        Color::Yellow => (255, 255, 0),
        _ => unreachable!("cmd result transition uses a named result colour"),
    }
}

fn cmd_result_status(result: &Value) -> (char, String, Style) {
    let target = cmd_result_target_color(result);
    if result
        .get("canceled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ('!', "canceled".to_owned(), Style::default().fg(target));
    }
    if result
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ('!', "timeout".to_owned(), Style::default().fg(target));
    }
    if result.get("error").is_some() {
        return ('×', "error".to_owned(), Style::default().fg(target));
    }
    match result.get("exit_code").and_then(Value::as_i64) {
        Some(0) => ('✓', "done".to_owned(), Style::default().fg(target)),
        Some(code) => ('×', format!("exit {code}"), Style::default().fg(target)),
        None => ('✓', "done".to_owned(), Style::default().fg(target)),
    }
}

fn subagent_task_from_arguments(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("task").and_then(Value::as_str).map(str::to_owned))
        .map(|task| truncate_tool_call(&task))
        .unwrap_or_else(|| command_display(arguments))
}

fn subagent_tool_segments(call_id: &str, arguments: &str, state: &UiState) -> Vec<(String, Style)> {
    let live_task = state.subagents.iter().find(|task| task.call_id == call_id);
    let task = live_task
        .map(|task| task.task.clone())
        .unwrap_or_else(|| subagent_task_from_arguments(arguments));
    let task = redact_secret(&truncate_tool_call(&task), Some(&state.secret));
    let (status, style) = if let Some(task) = live_task {
        if task.creation_completed {
            ("completed".to_owned(), Style::default().fg(Color::Green))
        } else {
            let status = if task.status == SubagentStatus::Running {
                running_tool_status(state)
            } else {
                subagent_status_display(task.status, state)
            };
            (status, subagent_status_style(task.status))
        }
    } else if state.failed_subagent_calls.contains(call_id) {
        ("error".to_owned(), Style::default().fg(Color::Red))
    } else if state.completed_subagent_calls.contains(call_id) {
        ("completed".to_owned(), Style::default().fg(Color::Green))
    } else {
        ("queued".to_owned(), pending_tool_call_style())
    };
    let separator = if live_task
        .is_some_and(|task| task.status == SubagentStatus::Running && !task.creation_completed)
    {
        "  "
    } else {
        "  → "
    };
    vec![(format!("↗ subagent  {task}{separator}{status}"), style)]
}

fn check_subagent_tool_segments(
    arguments: &str,
    result: Option<&Value>,
    state: &UiState,
) -> Vec<(String, Style)> {
    let task_id = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "invalid task".to_owned());
    let task_id = redact_secret(&task_id, Some(&state.secret));
    let text = if let Some(result) = result {
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("error");
        let style = if status == "completed" {
            Style::default().fg(Color::Green)
        } else {
            tool_result_style()
        };
        return vec![(format!("⌕ check  {task_id}  → {status}"), style)];
    } else {
        format!("⌕ check  {task_id}  ")
    };
    vec![(
        text + &tool_spinner_frame(state).to_string(),
        pending_tool_call_style(),
    )]
}

fn generic_tool_segments(
    name: &str,
    arguments: &str,
    result: Option<&Value>,
    state: &UiState,
) -> Vec<(String, Style)> {
    let call_text = redact_secret(
        &format!("[tool:{name} {}]", call_arguments(arguments)),
        Some(&state.secret),
    );
    let mut segments = vec![(
        call_text,
        if result.is_some() {
            tool_call_style()
        } else {
            pending_tool_call_style()
        },
    )];
    if let Some(result) = result {
        let result_text = redact_secret(&format_tool_result(result), Some(&state.secret));
        segments.push((" > ".to_owned(), Style::default()));
        segments.push((result_text, tool_result_style()));
    } else {
        segments.push((
            format!(" {}", tool_spinner_frame(state)),
            pending_tool_call_style(),
        ));
    }
    segments
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

const TOOL_CALL_PREVIEW_CHARS: usize = 100;

fn truncate_tool_call(output: &str) -> String {
    let mut result: String = output.chars().take(TOOL_CALL_PREVIEW_CHARS).collect();
    if output.chars().count() > TOOL_CALL_PREVIEW_CHARS {
        result.push('…');
    }
    result
}

/// Render tool call arguments as the command string inside double quotes, for
/// example `"cat README.md"`. Tool-call previews are limited to 100 characters;
/// malformed arguments fall back to the same bounded raw-text preview.
fn call_arguments(arguments: &str) -> String {
    let parsed: Value = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => return truncate_tool_call(arguments),
    };
    if let Some(command) = parsed.get("command").and_then(Value::as_str) {
        return format!("\"{}\"", truncate_tool_call(command));
    }
    let serialized = serde_json::to_string(&parsed).unwrap_or_else(|_| arguments.to_owned());
    truncate_tool_call(&serialized)
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

/// Render user messages with a one-cell yellow block rule, one inner left
/// padding cell, and blank rows above and below; assistant and tool output remains borderless.
fn push_user_message_block(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    active_skill_trigger: Option<&str>,
    width: usize,
) {
    if width < 3 {
        lines.extend(styled_text_lines(
            text,
            active_skill_trigger,
            width.max(1),
            Style::default().fg(Color::White),
        ));
        return;
    }

    let border_style = user_message_style();
    let rows = styled_text_lines(
        text,
        active_skill_trigger,
        width - 2,
        Style::default().fg(Color::White),
    );
    lines.push(Line::from(Span::styled(USER_BORDER_GLYPH, border_style)));
    for row in rows {
        let mut spans = Vec::with_capacity(row.spans.len() + 2);
        spans.push(Span::styled(USER_BORDER_GLYPH, border_style));
        spans.push(Span::styled(" ", Style::default().fg(Color::White)));
        spans.extend(row.spans);
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(USER_BORDER_GLYPH, border_style)));
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

fn model_status_line(
    state: &UiState,
    effort: &str,
    activity_text: &str,
    elapsed: Duration,
    now: Instant,
) -> Line<'static> {
    let model = redact_secret(&state.model, Some(&state.secret));
    let effort = redact_secret(effort, Some(&state.secret));
    let prefix = format!("{model} · {effort}");
    let activity_start = prefix.chars().count() + usize::from(!activity_text.is_empty());
    let text = if activity_text.is_empty() {
        prefix
    } else {
        format!("{prefix} {activity_text}")
    };
    let busy = matches!(state.status.as_str(), "working" | "compacting");
    let settling = state.status == "ready"
        && state
            .activity_transition
            .as_ref()
            .is_some_and(|transition| transition_progress(now, transition) < 1.0);
    let animated = (busy || settling) && !activity_text.is_empty();
    let character_count = text.chars().count().max(1);

    let spans: Vec<Span<'static>> = text
        .chars()
        .enumerate()
        .map(|(index, character)| {
            let style = if animated {
                let target = if state.status == "compacting" {
                    PENDING_TOOL_COLOR
                } else {
                    working_gradient_color_for_status(state, elapsed, index, character_count)
                };
                Style::default().fg(activity_color_at(state, target, now))
            } else if !activity_text.is_empty() && index >= activity_start {
                activity_style_at_now(state, elapsed, now)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Span::styled(character.to_string(), style)
        })
        .collect();

    Line::from(spans)
}

fn working_gradient_color_for_status(
    state: &UiState,
    elapsed: Duration,
    index: usize,
    character_count: usize,
) -> Color {
    let gradient_elapsed = if state.status == "ready" {
        state.last_active_elapsed
    } else {
        elapsed
    };
    working_gradient_color_at(gradient_elapsed, index, character_count.max(1))
}

#[cfg(test)]
fn activity_line_at<'a>(
    state: &UiState,
    text: &'a str,
    elapsed: Duration,
    now: Instant,
) -> Line<'a> {
    let character_count = text.chars().count().max(1);
    match state.status.as_str() {
        "working" => Line::from(
            text.chars()
                .enumerate()
                .map(|(index, character)| {
                    let target = working_gradient_color_at(elapsed, index, character_count);
                    Span::styled(
                        character.to_string(),
                        Style::default().fg(activity_color_at(state, target, now)),
                    )
                })
                .collect::<Vec<_>>(),
        ),
        "ready"
            if state
                .activity_transition
                .as_ref()
                .is_some_and(|transition| transition_progress(now, transition) < 1.0) =>
        {
            Line::from(
                text.chars()
                    .enumerate()
                    .map(|(index, character)| {
                        let source = working_gradient_color_at(
                            state.last_active_elapsed,
                            index,
                            character_count,
                        );
                        Span::styled(
                            character.to_string(),
                            Style::default().fg(activity_color_at(state, source, now)),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        }
        _ => Line::styled(text, activity_style_at_now(state, elapsed, now)),
    }
}

fn activity_style_at_now(state: &UiState, elapsed: Duration, now: Instant) -> Style {
    let target = if state.status == "working" {
        working_gradient_color_at(elapsed, 0, 1)
    } else if state.status == "compacting" {
        PENDING_TOOL_COLOR
    } else {
        Color::Cyan
    };
    Style::default().fg(activity_color_at(state, target, now))
}

fn activity_color_at(state: &UiState, target: Color, now: Instant) -> Color {
    let Some(transition) = &state.activity_transition else {
        return target;
    };
    let progress = transition_progress(now, transition);
    if progress >= 1.0 {
        return target;
    }

    match state.status.as_str() {
        "working" => blend_rgb(Color::Cyan, target, progress),
        "ready" => blend_rgb(target, Color::Cyan, progress),
        _ => target,
    }
}

fn transition_progress(now: Instant, transition: &ActivityTransition) -> f32 {
    // Rendering is polled at this cadence, so quantize the colour blend to the
    // same frames as the bar-height interpolation. This also keeps a single
    // draw internally consistent when its status line is inspected twice.
    let elapsed_ticks = now
        .saturating_duration_since(transition.started_at)
        .as_millis()
        / PULSE_TICK.as_millis();
    let transition_ticks = ACTIVITY_TRANSITION_DURATION.as_millis() / PULSE_TICK.as_millis();
    (elapsed_ticks as f32 / transition_ticks as f32).min(1.0)
}
fn blend_rgb(from: Color, to: Color, progress: f32) -> Color {
    let (from_red, from_green, from_blue) = activity_rgb(from);
    let (to_red, to_green, to_blue) = activity_rgb(to);
    Color::Rgb(
        interpolate_color(from_red, to_red, progress),
        interpolate_color(from_green, to_green, progress),
        interpolate_color(from_blue, to_blue, progress),
    )
}

fn activity_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(red, green, blue) => (red, green, blue),
        // The resting indicator uses ratatui's named cyan. Convert it to its
        // ANSI RGB equivalent while blending into and out of the warm pulse.
        Color::Cyan => (0, 255, 255),
        _ => unreachable!("activity transition colours are cyan or RGB"),
    }
}

/// Color the prompt outline from teal at its left edge to green at its right
/// edge. Vertical sides retain their respective endpoint colours.
fn draw_prompt_border_gradient(frame: &mut Frame<'_>, area: Rect) {
    if area.is_empty() {
        return;
    }

    let right = area.x + area.width.saturating_sub(1);
    let bottom = area.y + area.height.saturating_sub(1);
    let buffer = frame.buffer_mut();
    for x in area.x..=right {
        let style = Style::default().fg(prompt_border_gradient_color_at(x - area.x, area.width));
        buffer[(x, area.y)].set_style(style);
        buffer[(x, bottom)].set_style(style);
    }
    for y in area.y..=bottom {
        buffer[(area.x, y)].set_style(Style::default().fg(PROMPT_BORDER_START_COLOR.into()));
        buffer[(right, y)].set_style(Style::default().fg(PROMPT_BORDER_END_COLOR.into()));
    }
}

fn prompt_border_gradient_color_at(column: u16, width: u16) -> Color {
    let progress = column as f32 / width.saturating_sub(1).max(1) as f32;
    Color::Rgb(
        interpolate_color(
            PROMPT_BORDER_START_COLOR.0,
            PROMPT_BORDER_END_COLOR.0,
            progress,
        ),
        interpolate_color(
            PROMPT_BORDER_START_COLOR.1,
            PROMPT_BORDER_END_COLOR.1,
            progress,
        ),
        interpolate_color(
            PROMPT_BORDER_START_COLOR.2,
            PROMPT_BORDER_END_COLOR.2,
            progress,
        ),
    )
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
    const SWEEP_WIDTH: f64 = 0.65;

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

// Unicode block elements occupy the full cell width. Rendering them without
// separators keeps the five bars visually continuous in terminal fonts.
const PULSE_LEVELS: [char; 7] = ['▁', '▂', '▃', '▅', '▆', '▇', '█'];
const PULSE_BAR_PERIODS: [u128; 5] = [12, 16, 20, 24, 15];
const PULSE_BAR_PHASES: [u128; 5] = [0, 5, 13, 9, 3];
const PULSE_TICK: Duration = Duration::from_millis(50);
const TOOL_SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];
const TOOL_SPINNER_FRAME_DURATION: Duration = Duration::from_millis(100);

/// Five independently phased triangle waves make the bars feel irregular
/// without random jumps: every rendered tick changes a bar by at most one
/// level, and the combined pattern repeats every 12 seconds.
const ACTIVITY_TRANSITION_DURATION: Duration = Duration::from_millis(400);
// This frame gives all five bars room to rise from the resting level while
// preserving the pulse waveform's one-level-per-tick continuity afterwards.
const PULSE_ENTRY_FRAME: Duration = Duration::from_millis(950);

fn spinner_frame(state: &UiState) -> String {
    pulse_frame(state.activity_levels_at(Instant::now()))
}

#[cfg(test)]
fn spinner_frame_at(elapsed: Duration) -> String {
    pulse_frame(pulse_levels_at(elapsed))
}

/// A compact, traditional spinner for tool calls that are awaiting a result.
/// It deliberately has a separate epoch because background work can outlive a
/// main-agent turn.
fn tool_spinner_frame(state: &UiState) -> String {
    tool_spinner_frame_at(state.tool_animation_epoch.elapsed()).to_string()
}

fn tool_spinner_frame_at(elapsed: Duration) -> char {
    let frame = (elapsed.as_millis() / TOOL_SPINNER_FRAME_DURATION.as_millis()) as usize;
    TOOL_SPINNER_FRAMES[frame % TOOL_SPINNER_FRAMES.len()]
}

fn pulse_frame(levels: [usize; PULSE_BAR_PERIODS.len()]) -> String {
    levels
        .into_iter()
        .map(|level| PULSE_LEVELS[level])
        .collect()
}

fn pulse_levels_at(elapsed: Duration) -> [usize; PULSE_BAR_PERIODS.len()] {
    let tick = elapsed.as_millis() / PULSE_TICK.as_millis();
    std::array::from_fn(|index| {
        pulse_level_at(tick, PULSE_BAR_PERIODS[index], PULSE_BAR_PHASES[index])
    })
}

fn interpolate_pulse_levels(
    from: [usize; PULSE_BAR_PERIODS.len()],
    to: [usize; PULSE_BAR_PERIODS.len()],
    elapsed: Duration,
) -> [usize; PULSE_BAR_PERIODS.len()] {
    let elapsed = elapsed.min(ACTIVITY_TRANSITION_DURATION).as_millis();
    let duration = ACTIVITY_TRANSITION_DURATION.as_millis();
    std::array::from_fn(|index| {
        let start = from[index] as i128;
        let distance = to[index] as i128 - start;
        (start + distance * elapsed as i128 / duration as i128) as usize
    })
}

fn pulse_level_at(tick: u128, period: u128, phase: u128) -> usize {
    let position = (tick + phase) % period;
    let half_period = period / 2;
    let distance_from_floor = if position <= half_period {
        position
    } else {
        period - position
    };
    (distance_from_floor * (PULSE_LEVELS.len() - 1) as u128 / half_period) as usize
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
    fn tui_viewport_reserves_one_column_on_each_side_when_possible() {
        assert_eq!(
            tui_viewport(Rect::new(0, 0, 80, 10)),
            Rect::new(1, 0, 78, 10)
        );
        assert_eq!(
            tui_viewport(Rect::new(0, 0, 2, 10)),
            Rect::new(0, 0, 2, 10),
            "a two-column terminal cannot reserve two gutters"
        );
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
        let status_area = ui_layout(&state, tui_viewport(Rect::new(0, 0, 60, 10))).5;
        let context = context_status_text(&state);
        let context_start = status_area.x + status_area.width - context.chars().count() as u16;
        let context_end = status_area.x + status_area.width - 1;
        assert_eq!(buffer[(context_start, status_area.y)].symbol(), "c");
        assert_eq!(buffer[(context_end, status_area.y)].symbol(), ")");
        assert_eq!(buffer[(context_end, status_area.y)].fg, PENDING_TOOL_COLOR);
    }

    #[test]
    fn activity_indicator_uses_only_contiguous_block_bars() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        assert_eq!(activity_text(&state), "");

        state.set_status("working");
        let working = activity_text(&state);
        assert_eq!(working.chars().count(), 5);
        assert!(working.chars().all(|bar| PULSE_LEVELS.contains(&bar)));
        assert!(!working.chars().any(char::is_whitespace));
    }

    #[test]
    fn activity_indicator_ramps_up_and_settles_down_one_level_per_tick() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.set_status("working");
        let entry = state
            .activity_transition
            .clone()
            .expect("working begins with a ramp");
        let entry_frames = (0..=ACTIVITY_TRANSITION_DURATION.as_millis() / PULSE_TICK.as_millis())
            .map(|tick| state.activity_levels_at(entry.started_at + PULSE_TICK * tick as u32))
            .collect::<Vec<_>>();
        assert_eq!(entry_frames.first(), Some(&[0; PULSE_BAR_PERIODS.len()]));
        assert_eq!(entry_frames.last(), Some(&entry.to_levels));
        assert_levels_change_gradually(&entry_frames, "working entry");

        // Complete the entry ramp first so ready samples an active pulse frame
        // rather than merely reversing a partially completed entry transition.
        state.activity_transition = None;
        state.activity_started_at = Instant::now() - PULSE_ENTRY_FRAME;
        state.set_status("ready");
        let exit = state
            .activity_transition
            .clone()
            .expect("ready begins with a settle-down ramp");
        let exit_frames = (0..=ACTIVITY_TRANSITION_DURATION.as_millis() / PULSE_TICK.as_millis())
            .map(|tick| state.activity_levels_at(exit.started_at + PULSE_TICK * tick as u32))
            .collect::<Vec<_>>();
        assert_eq!(exit_frames.first(), Some(&exit.from_levels));
        assert_eq!(exit_frames.last(), Some(&[0; PULSE_BAR_PERIODS.len()]));
        assert_levels_change_gradually(&exit_frames, "ready exit");
        assert!(exit_frames.windows(2).all(|pair| {
            pair[0]
                .iter()
                .zip(pair[1])
                .all(|(before, after)| after <= *before)
        }));

        let settled_at = exit.started_at + ACTIVITY_TRANSITION_DURATION;
        let settled_text = activity_text_at(&state, settled_at);
        let settled = activity_line_at(
            &state,
            &settled_text,
            state.working_elapsed_at(settled_at),
            settled_at,
        );
        assert_eq!(
            settled.style.fg,
            Some(Color::Cyan),
            "the completed settle-down transition restores the ready colour"
        );
    }

    fn assert_levels_change_gradually(
        frames: &[[usize; PULSE_BAR_PERIODS.len()]],
        transition: &str,
    ) {
        assert!(
            frames.windows(2).all(|pair| {
                pair[0]
                    .iter()
                    .zip(pair[1])
                    .all(|(before, after)| before.abs_diff(after) <= 1)
            }),
            "{transition} must not jump more than one pulse level per rendered tick"
        );
    }

    #[test]
    fn pulse_spinner_moves_each_bar_one_level_at_a_time() {
        let frames = (0..=240)
            .map(|tick| spinner_frame_at(PULSE_TICK * tick))
            .collect::<Vec<_>>();
        assert!(frames.iter().any(|frame| frame != &frames[0]));
        assert_eq!(PULSE_TICK, Duration::from_millis(50));

        for pair in frames.windows(2) {
            let levels = pair
                .iter()
                .map(|frame| {
                    frame
                        .chars()
                        .map(|bar| {
                            PULSE_LEVELS
                                .iter()
                                .position(|level| *level == bar)
                                .expect("known pulse level")
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(levels[0].len(), 5);
            assert!(
                levels[0]
                    .iter()
                    .zip(&levels[1])
                    .all(|(before, after)| before.abs_diff(*after) <= 1),
                "pulse bars must not jump between adjacent ticks: {:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn working_activity_uses_a_continuous_warm_gradient() {
        let text = "▁▅▆▆▃";
        let colors = text
            .chars()
            .enumerate()
            .map(|(index, _)| {
                working_gradient_color_at(Duration::from_millis(2_500), index, text.chars().count())
            })
            .collect::<Vec<_>>();

        assert_eq!(colors.first(), Some(&Color::Rgb(228, 40, 120)));
        assert_eq!(colors.last(), Some(&Color::Rgb(235, 47, 64)));
        assert!(colors.windows(2).all(|pair| pair[0] != pair[1]));

        let (start, end, _) = working_gradient_colors_at(1.0);
        assert_eq!(start, Color::Rgb(235, 45, 65));
        assert_eq!(end, Color::Rgb(255, 130, 25));

        let (start, end, _) = working_gradient_colors_at(3.0);
        assert_eq!(start, Color::Rgb(235, 45, 65));
        assert_eq!(end, Color::Rgb(220, 35, 175));
    }

    #[test]
    fn queued_messages_render_above_the_prompt_border_with_their_existing_style() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.queue_user("first task");
        state.queue_user("second task");
        let area = Rect::new(0, 0, 80, 12);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        let mut rendered_area = Rect::default();
        terminal
            .draw(|frame| {
                rendered_area = frame.area();
                draw(frame, &state);
            })
            .expect("draw queued message");
        let (_, _, _, queue_area, input_area, status_area) =
            ui_layout(&state, tui_viewport(rendered_area));
        let queue_area = queue_area.expect("message queue area");
        assert_eq!(queue_area.x, input_area.x + 1);
        assert_eq!(queue_area.y + queue_area.height, input_area.y);
        assert_eq!(queue_area.width, input_area.width.saturating_sub(2));
        assert_eq!(queue_area.height, 2, "each queued message owns one row");
        let buffer = terminal.backend().buffer();
        let queued_rows = (queue_area.y..queue_area.y + queue_area.height)
            .map(|y| {
                (queue_area.x..queue_area.x + queue_area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let status_row = (status_area.x..status_area.x + status_area.width)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert_eq!(buffer[(input_area.x, input_area.y)].symbol(), "┌");
        assert_eq!(
            buffer[(input_area.x + input_area.width - 1, input_area.y)].symbol(),
            "┐"
        );
        assert!(queued_rows[0].contains("Queued 2: first task"));
        assert!(queued_rows[1].contains("Queued 2: second task"));
        assert!(queued_rows.iter().all(|row| !row.contains('|')));
        for y in queue_area.y..queue_area.y + queue_area.height {
            assert_eq!(buffer[(queue_area.x, y)].fg, QUEUED_MESSAGE_COLOR);
            assert_eq!(
                buffer[(queue_area.x + queue_area.width - 1, y)].bg,
                QUEUED_MESSAGE_BACKGROUND,
                "the dark teal background fills every queue row"
            );
        }
        assert!(!status_row.contains("first task"));
        assert!(!status_row.contains("second task"));
    }

    #[test]
    fn ready_submission_bypasses_queue_and_is_not_added_twice_when_started() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);

        state.submit_user("send now");

        assert!(state.queued_messages.is_empty());
        assert_eq!(state.transcript.len(), 1);
        assert!(matches!(
            &state.transcript[0],
            TranscriptItem::User { text, .. } if text == "send now"
        ));

        // The worker's Started notification still arrives asynchronously, but
        // must not promote an already visible direct submission a second time.
        state.start_queued_user("send now");
        assert_eq!(state.transcript.len(), 1);
    }

    #[test]
    fn busy_submission_remains_queued_until_its_turn_starts() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.busy = true;

        state.submit_user("send later");

        assert_eq!(state.queued_messages, ["send later"]);
        assert!(state.transcript.is_empty());

        state.start_queued_user("send later");
        assert!(state.queued_messages.is_empty());
        assert!(matches!(
            &state.transcript[..],
            [TranscriptItem::User { text, .. }] if text == "send later"
        ));
    }

    #[test]
    fn skill_picker_stays_above_a_visible_message_queue() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["release-notes".to_owned()]);
        state.queue_user("next task");
        state.input = "/".to_owned();
        state.input_changed();

        let area = Rect::new(0, 0, 80, 12);
        let (_, picker_area, _, queue_area, input_area, _) = ui_layout(&state, tui_viewport(area));
        let picker_area = picker_area.expect("skill picker area");
        let queue_area = queue_area.expect("message queue area");
        assert_eq!(picker_area.y + picker_area.height, queue_area.y);
        assert_eq!(queue_area.y + queue_area.height, input_area.y);
        assert_eq!(queue_area.x, input_area.x + 1);
    }

    #[test]
    fn fresh_sessions_show_the_versioned_gradient_welcome_message() {
        let state = UiState::from_history(&[], "secret", "model", None, false);
        assert!(state.welcome_visible);

        let line = welcome_line();
        assert_eq!(line.to_string(), WELCOME_MESSAGE);
        assert_eq!(WELCOME_VERSION, concat!("v", env!("CARGO_PKG_VERSION")));
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
    fn welcome_renders_the_gray_version_on_the_bottom_chat_row() {
        let state = UiState::from_history(&[], "secret", "model", None, false);
        let area = Rect::new(0, 0, 80, 12);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw welcome screen");

        let chat_area = ui_layout(&state, tui_viewport(area)).0;
        let version_width = WELCOME_VERSION.chars().count() as u16;
        let version_x = chat_area.x + (chat_area.width - version_width) / 2;
        let version_y = chat_area.y + chat_area.height - 1;
        let buffer = terminal.backend().buffer();
        let rendered_version = (version_x..version_x + version_width)
            .map(|x| buffer[(x, version_y)].symbol())
            .collect::<String>();
        assert_eq!(rendered_version, WELCOME_VERSION);
        assert!((version_x..version_x + version_width)
            .all(|x| buffer[(x, version_y)].fg == Color::DarkGray));
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
    fn user_messages_have_a_single_block_rule_with_inner_and_vertical_padding() {
        let history = [SessionHistoryRecord::Message {
            timestamp: 1,
            message: ChatMessage::user("hello\nworld".to_owned()),
        }];
        let state = UiState::from_history(&history, "provider-secret", "model", None, false);
        let lines = transcript_lines(&state, 12);

        assert_eq!(UnicodeWidthStr::width(USER_BORDER_GLYPH), 1);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].to_string(), "▌");
        assert_eq!(lines[1].to_string(), "▌ hello");
        assert_eq!(lines[2].to_string(), "▌ world");
        assert_eq!(lines[3].to_string(), "▌");
        for line in &lines {
            assert_eq!(line.spans[0].content, USER_BORDER_GLYPH);
            assert_eq!(line.spans[0].style.fg, Some(USER_BORDER_COLOR));
            assert!(!line.to_string().contains(['┌', '┐', '└', '┘', '│']));
        }
        for line in &lines[1..3] {
            assert_eq!(line.spans[1].content, " ");
            assert_eq!(line.spans[1].style.fg, Some(Color::White));
            assert_eq!(line.spans[2].style.fg, Some(Color::White));
        }
    }

    #[test]
    fn attached_skill_highlights_its_trigger_in_the_user_message_without_a_notice_line() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["release-notes".to_owned()]);
        state.add_user("/release-notes v1.2.0", "secret");
        state.mark_latest_user_skill_attached();

        let lines = transcript_lines(&state, 40);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].spans[1].content, " ");
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
    fn multiline_input_arrows_move_cursor_between_explicit_and_wrapped_rows() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.input = "ab\ncd\nef".to_owned();
        state.cursor = 1;

        assert!(move_input_cursor_vertical(&mut state, 10, true));
        assert_eq!(
            state.cursor, 4,
            "preserve the column on the next explicit row"
        );
        assert!(move_input_cursor_vertical(&mut state, 10, true));
        assert_eq!(state.cursor, 7);
        assert!(!move_input_cursor_vertical(&mut state, 10, true));
        assert!(move_input_cursor_vertical(&mut state, 10, false));
        assert_eq!(state.cursor, 4);

        state.input = "abcdef".to_owned();
        state.cursor = 1;
        assert!(move_input_cursor_vertical(&mut state, 3, true));
        assert_eq!(state.cursor, 4, "wrapped rows use the same visual column");
        assert!(move_input_cursor_vertical(&mut state, 3, false));
        assert_eq!(state.cursor, 1);
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
        assert_eq!(lines[0].to_string(), "▌");
        assert_eq!(lines[1].to_string(), "▌ hi");
        assert_eq!(lines[2].to_string(), "▌");
        assert_eq!(lines[3].to_string(), "");
        assert_eq!(lines[4].to_string(), "hello");
    }

    #[test]
    fn cmd_call_renders_as_a_compact_status_line_without_raw_json() {
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
                    serde_json::json!({"exit_code": 0, "stdout": "secret output"}).to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let text = transcript_lines(&state, 80)[0].to_string();

        assert_eq!(text, "✓ cmd  $ pwd");
        assert!(!text.contains("secret output"));
        assert!(!text.contains("{\"command\":\"pwd\"}"));
    }

    #[test]
    fn pending_cmd_calls_use_a_compact_running_status() {
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

        let text = line.to_string();
        let prefix = "· cmd  $ pwd  ";
        assert!(text.starts_with(prefix));
        assert!(!text.contains("→ running"));
        let frame = &text[prefix.len()..];
        assert_eq!(frame.chars().count(), 1);
        assert!(frame
            .chars()
            .all(|spinner| TOOL_SPINNER_FRAMES.contains(&spinner)));
        assert!(line
            .spans
            .iter()
            .all(|span| span.style.fg == Some(PENDING_TOOL_COLOR)));
    }

    #[test]
    fn running_tool_indicators_use_a_traditional_spinner_with_their_own_clock() {
        assert_eq!(tool_spinner_frame_at(Duration::ZERO), '|');
        assert_eq!(tool_spinner_frame_at(TOOL_SPINNER_FRAME_DURATION), '/');
        assert_eq!(tool_spinner_frame_at(TOOL_SPINNER_FRAME_DURATION * 2), '-');
        assert_eq!(tool_spinner_frame_at(TOOL_SPINNER_FRAME_DURATION * 3), '\\');

        let state = UiState::from_history(&[], "secret", "model", None, false);
        let spinner = running_tool_status(&state);
        assert_eq!(spinner.chars().count(), 1);
        assert!(spinner
            .chars()
            .all(|spinner| TOOL_SPINNER_FRAMES.contains(&spinner)));
        assert!(
            subagent_status_display(SubagentStatus::Running, &state).starts_with("running "),
            "the background-task list uses the same spinner"
        );
    }

    #[test]
    fn successful_cmd_sweeps_from_orange_to_green_left_to_right() {
        let started_at = Instant::now();
        let character_count = 12;
        let halfway = started_at + Duration::from_millis(175);

        assert_eq!(
            cmd_result_color_at(started_at, started_at, 0, character_count, Color::Green),
            PENDING_TOOL_COLOR,
            "a new success must retain the pending orange at the sweep start"
        );
        assert_eq!(
            cmd_result_color_at(started_at, halfway, 0, character_count, Color::Green),
            Color::Rgb(
                TOOL_SUCCESS_GREEN_RGB.0,
                TOOL_SUCCESS_GREEN_RGB.1,
                TOOL_SUCCESS_GREEN_RGB.2,
            ),
            "the left edge completes before the rest of the line"
        );
        assert_eq!(
            cmd_result_color_at(started_at, halfway, 9, character_count, Color::Green),
            PENDING_TOOL_COLOR,
            "the right edge remains orange until the sweep reaches it"
        );
        assert_eq!(
            cmd_result_color_at(
                started_at,
                started_at + TOOL_RESULT_SWEEP_DURATION,
                9,
                character_count,
                Color::Green,
            ),
            Color::Green,
            "the completed sweep settles on the existing success green"
        );
    }

    #[test]
    fn only_live_cmd_results_start_a_result_sweep() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        let succeeded = serde_json::json!({"exit_code": 0});

        state.add_tool_result("historic", "cmd", succeeded.clone());
        state.add_live_tool_result("success", "cmd", succeeded);
        state.add_live_tool_result("failed", "cmd", serde_json::json!({"exit_code": 1}));

        assert!(!state.cmd_result_started_at.contains_key("historic"));
        assert!(state.cmd_result_started_at.contains_key("success"));
        assert!(state.cmd_result_started_at.contains_key("failed"));
    }

    #[test]
    fn cmd_result_sweeps_to_failure_red_with_intermediate_gradient() {
        let started_at = Instant::now();
        let character_count = 12;
        let halfway = started_at + TOOL_RESULT_SWEEP_DURATION / 2;

        assert_eq!(
            cmd_result_color_at(started_at, started_at, 0, character_count, Color::Red),
            PENDING_TOOL_COLOR,
        );
        let intermediate = cmd_result_color_at(started_at, halfway, 6, character_count, Color::Red);
        assert_ne!(intermediate, PENDING_TOOL_COLOR);
        assert_ne!(intermediate, Color::Red);
        assert_eq!(
            cmd_result_color_at(started_at, halfway, 0, character_count, Color::Red),
            Color::Rgb(255, 0, 0),
            "the leading edge reaches the failure colour first"
        );
        assert_eq!(
            cmd_result_color_at(
                started_at,
                halfway,
                character_count - 1,
                character_count,
                Color::Red,
            ),
            PENDING_TOOL_COLOR,
            "the right edge remains at the starting colour"
        );
        assert_eq!(
            cmd_result_color_at(
                started_at,
                started_at + TOOL_RESULT_SWEEP_DURATION,
                character_count - 1,
                character_count,
                Color::Red,
            ),
            Color::Red,
        );
    }

    #[test]
    fn live_failed_cmd_sweep_keeps_the_final_status_text() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        let result = serde_json::json!({"exit_code": 1});
        state.add_live_tool_result("failed", "cmd", result.clone());

        let segments = cmd_tool_segments("failed", r#"{"command":"bad"}"#, Some(&result), &state);
        let text = segments
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<String>();

        assert_eq!(text, "× cmd  $ bad  → exit 1");
    }

    #[test]
    fn cmd_result_target_colors_follow_the_final_status() {
        assert_eq!(
            cmd_result_target_color(&serde_json::json!({"exit_code": 0})),
            Color::Green
        );
        assert_eq!(
            cmd_result_target_color(&serde_json::json!({"exit_code": 1})),
            Color::Red
        );
        assert_eq!(
            cmd_result_target_color(&serde_json::json!({"timed_out": true})),
            Color::Yellow
        );
    }

    #[test]
    fn cmd_status_distinguishes_nonzero_exit_timeout_and_cancellation() {
        let cases = [
            (
                serde_json::json!({"exit_code": 127}),
                "× cmd  $ bad  → exit 127",
            ),
            (
                serde_json::json!({"timed_out": true, "exit_code": null}),
                "! cmd  $ slow  → timeout",
            ),
            (
                serde_json::json!({"canceled": true}),
                "! cmd  $ stop  → canceled",
            ),
        ];
        for (result, expected) in cases {
            let history = vec![
                SessionHistoryRecord::Message {
                    timestamp: 1,
                    message: ChatMessage::assistant(
                        String::new(),
                        vec![crate::model::ChatToolCall {
                            id: "call-1".to_owned(),
                            name: "cmd".to_owned(),
                            arguments: serde_json::json!({"command": expected.split("$ ").nth(1).unwrap().split("  ").next().unwrap()}).to_string(),
                        }],
                    ),
                },
                SessionHistoryRecord::Message {
                    timestamp: 2,
                    message: ChatMessage::tool(
                        "call-1".to_owned(),
                        "cmd".to_owned(),
                        result.to_string(),
                    ),
                },
            ];
            let state = UiState::from_history(&history, "secret", "model", None, false);
            assert_eq!(transcript_lines(&state, 80)[0].to_string(), expected);
        }
    }

    #[test]
    fn cmd_line_truncates_long_commands_but_never_renders_output() {
        let command = "a".repeat(120);
        let arguments = serde_json::json!({"command": command}).to_string();
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-1".to_owned(),
                        name: "cmd".to_owned(),
                        arguments,
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-1".to_owned(),
                    "cmd".to_owned(),
                    serde_json::json!({"exit_code": 0, "stdout": "output"}).to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let text = transcript_lines(&state, 200)[0].to_string();
        assert!(text.contains(&format!("$ {}…", "a".repeat(100))));
        assert!(!text.contains(&"a".repeat(101)));
        assert!(!text.contains("output"));
    }

    #[test]
    fn cmd_lines_remain_compact_for_consecutive_calls() {
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
                    serde_json::json!({"exit_code": 0}).to_string(),
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 3,
                message: ChatMessage::tool(
                    "call-second".to_owned(),
                    "cmd".to_owned(),
                    serde_json::json!({"exit_code": 0}).to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let lines = transcript_lines(&state, 200);
        assert_eq!(lines[0].to_string(), "✓ cmd  $ first");
        assert_eq!(lines[2].to_string(), "✓ cmd  $ second");
    }

    #[test]
    fn cmd_status_styles_use_success_failure_and_pending_colors() {
        assert_eq!(
            cmd_result_status(&serde_json::json!({"exit_code": 0})).2.fg,
            Some(Color::Green)
        );
        assert_eq!(
            cmd_result_status(&serde_json::json!({"exit_code": 1})).2.fg,
            Some(Color::Red)
        );
        assert_eq!(
            cmd_tool_segments(
                "call-1",
                "{\"command\":\"pwd\"}",
                None,
                &UiState::from_history(&[], "secret", "model", None, false)
            )[0]
            .1
            .fg,
            Some(PENDING_TOOL_COLOR)
        );
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

        // The full-width input block keeps trigger characters cyan while the
        // argument that follows stays white.
        let buffer = terminal.backend().buffer();
        let (_, _, _, _, input_area, _) = ui_layout(&state, tui_viewport(Rect::new(0, 0, 40, 10)));
        let input_x = input_area.x + 1;
        let input_y = input_area.y + 1;
        assert_eq!(buffer[(input_x, input_y)].fg, Color::Cyan);
        assert_eq!(
            buffer[(input_x + "/release-notes".chars().count() as u16, input_y)].fg,
            Color::White
        );
    }

    #[test]
    fn main_agent_status_attaches_busy_indicator_to_model_and_hides_it_when_idle() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        let area = Rect::new(0, 0, 80, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw ready status");
        let viewport = tui_viewport(area);
        let (_, _, _, _, _input_area, status_area) = ui_layout(&state, viewport);
        let buffer = terminal.backend().buffer();
        let idle_row = (status_area.x..status_area.x + status_area.width)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert!(idle_row.starts_with("model · default"));
        assert!(!idle_row.contains("▁▁▁▁▁"));
        assert_eq!(buffer[(status_area.x, status_area.y)].fg, Color::DarkGray);

        state.set_status("working");
        state.busy = true;
        // Sample the steady working gradient rather than the entry ramp so
        // this assertion checks the full left-to-right statusline flow.
        state.activity_transition = None;
        state.activity_started_at = Instant::now() - Duration::from_secs(1);
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw working status");
        let (_, _, _, _, _input_area, status_area) = ui_layout(&state, viewport);
        let buffer = terminal.backend().buffer();
        let activity = activity_text(&state);
        let expected_prefix = format!("{} · default ", state.model);
        let expected_line = format!("{expected_prefix}{activity}");
        let rendered_line = (status_area.x..status_area.x + expected_line.chars().count() as u16)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert_eq!(rendered_line, expected_line);
        let prefix_width = expected_prefix.chars().count() as u16;
        assert_eq!(
            buffer[(status_area.x + prefix_width - 1, status_area.y)].symbol(),
            ' '.to_string()
        );
        assert!(
            (0..expected_line.chars().count() as u16).all(|offset| {
                buffer[(status_area.x + offset, status_area.y)].fg != Color::DarkGray
            }),
            "the model, separator, effort, and busy indicator share the animated colour flow"
        );
        assert!(
            (0..expected_line.chars().count() as u16 - 1).any(|offset| {
                buffer[(status_area.x + offset, status_area.y)].fg
                    != buffer[(status_area.x + offset + 1, status_area.y)].fg
            }),
            "the statusline uses a left-to-right gradient rather than one flat colour"
        );

        state.input = "first\nsecond\nthird".to_owned();
        state.cursor = state.input.chars().count();
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw multiline working status");
        let (_, _, _, _, input_area, status_area) = ui_layout(&state, viewport);
        assert!(input_area.height > 3);
        assert_eq!(input_area.y + input_area.height, status_area.y);
    }

    #[test]
    fn cjk_input_keeps_the_terminal_cursor_in_the_prompt_without_resetting_activity() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.set_status("working");
        state.busy = true;
        state.input = "한글".to_owned();
        state.cursor = state.input.chars().count();
        let activity_started_at = state.activity_started_at;
        let tool_animation_epoch = state.tool_animation_epoch;
        let sample_at = Instant::now();
        let activity_before = state.activity_levels_at(sample_at);

        // A committed CJK character must move the hardware cursor by its
        // display width, and input edits must not restart either animation.
        state.input_changed();
        assert_eq!(state.activity_started_at, activity_started_at);
        assert_eq!(state.tool_animation_epoch, tool_animation_epoch);
        assert_eq!(state.activity_levels_at(sample_at), activity_before);

        let area = Rect::new(0, 0, 80, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw CJK input while working");
        let (_, _, _, _, input_area, status_area) = ui_layout(&state, tui_viewport(area));
        assert_ne!(input_area.y, status_area.y);
        terminal.backend_mut().assert_cursor_position((
            input_area.x + 1 + UnicodeWidthStr::width(state.input.as_str()) as u16,
            input_area.y + 1,
        ));
    }

    #[test]
    fn input_border_has_a_teal_to_green_gradient_and_keeps_the_cursor() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw ready input");

        let (_, _, _, _, input_area, _) = ui_layout(&state, tui_viewport(Rect::new(0, 0, 80, 10)));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, input_area.y)].symbol(), " ");
        assert_eq!(buffer[(79, input_area.y)].symbol(), " ");
        assert_eq!(
            buffer[(input_area.x, input_area.y)].fg,
            Color::Rgb(
                PROMPT_BORDER_START_COLOR.0,
                PROMPT_BORDER_START_COLOR.1,
                PROMPT_BORDER_START_COLOR.2,
            )
        );
        assert_eq!(
            buffer[(
                input_area.x + input_area.width.saturating_sub(1),
                input_area.y
            )]
                .fg,
            Color::Rgb(
                PROMPT_BORDER_END_COLOR.0,
                PROMPT_BORDER_END_COLOR.1,
                PROMPT_BORDER_END_COLOR.2,
            )
        );
        assert_eq!(
            buffer[(input_area.x, input_area.y + 1)].fg,
            Color::Rgb(
                PROMPT_BORDER_START_COLOR.0,
                PROMPT_BORDER_START_COLOR.1,
                PROMPT_BORDER_START_COLOR.2,
            ),
            "the left side uses the teal endpoint"
        );
        assert_eq!(
            buffer[(
                input_area.x + input_area.width.saturating_sub(1),
                input_area.y + 1,
            )]
                .fg,
            Color::Rgb(
                PROMPT_BORDER_END_COLOR.0,
                PROMPT_BORDER_END_COLOR.1,
                PROMPT_BORDER_END_COLOR.2,
            ),
            "the right side uses the green endpoint"
        );
        assert_ne!(
            prompt_border_gradient_color_at(input_area.width / 2, input_area.width),
            Color::Rgb(
                PROMPT_BORDER_START_COLOR.0,
                PROMPT_BORDER_START_COLOR.1,
                PROMPT_BORDER_START_COLOR.2,
            ),
            "middle border cells interpolate away from teal"
        );
        assert_ne!(
            prompt_border_gradient_color_at(input_area.width / 2, input_area.width),
            Color::Rgb(
                PROMPT_BORDER_END_COLOR.0,
                PROMPT_BORDER_END_COLOR.1,
                PROMPT_BORDER_END_COLOR.2,
            ),
            "middle border cells interpolate before green"
        );
        assert_eq!(
            buffer[(input_area.x, input_area.y)].symbol(),
            "┌",
            "the prompt uses square rather than rounded corners"
        );
        assert_eq!(
            buffer[(
                input_area.x + input_area.width.saturating_sub(1),
                input_area.y + input_area.height.saturating_sub(1),
            )]
                .symbol(),
            "┘"
        );
        assert_eq!(buffer[(input_area.x + 1, input_area.y + 1)].symbol(), " ");
        assert_eq!(input_display_text(&state), "");
        terminal
            .backend_mut()
            .assert_cursor_position((input_area.x + 1, input_area.y + 1));

        state.busy = true;
        state.set_status("working");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw working input");
        let (_, _, _, _, input_area, _) = ui_layout(&state, tui_viewport(Rect::new(0, 0, 80, 10)));
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(input_area.x, input_area.y)].fg,
            Color::Rgb(
                PROMPT_BORDER_START_COLOR.0,
                PROMPT_BORDER_START_COLOR.1,
                PROMPT_BORDER_START_COLOR.2,
            ),
            "working state preserves the teal endpoint"
        );
        assert_eq!(
            buffer[(
                input_area.x + input_area.width.saturating_sub(1),
                input_area.y
            )]
                .fg,
            Color::Rgb(
                PROMPT_BORDER_END_COLOR.0,
                PROMPT_BORDER_END_COLOR.1,
                PROMPT_BORDER_END_COLOR.2,
            ),
            "working state preserves the green endpoint"
        );

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

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(20, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw input cursor");

        // After inserting a newline, the cursor is at the start of the second
        // input row.
        let (_, _, _, _, input_area, _) = ui_layout(&state, tui_viewport(Rect::new(0, 0, 20, 10)));
        terminal
            .backend_mut()
            .assert_cursor_position((input_area.x + 1, input_area.y + 2));
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
        assert_eq!(lines[0].to_string(), "✓ cmd  $ first");
        assert_eq!(lines[2].to_string(), "✓ cmd  $ second");
    }
    #[test]
    fn subagent_tasks_keep_metadata_until_completion_then_leave_live_list() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-worker".to_owned(),
                        name: "spawn_subagent".to_owned(),
                        arguments: serde_json::json!({
                            "task": "Inspect the command UI",
                            "model": "worker-model",
                            "effort": "high"
                        })
                        .to_string(),
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-worker".to_owned(),
                    "spawn_subagent".to_owned(),
                    serde_json::json!({"task_id":"subagent-1","status":"queued"}).to_string(),
                ),
            },
        ];
        let mut state = UiState::from_history(&history, "secret", "model", None, false);
        assert_eq!(state.subagents.len(), 1);
        let task = &state.subagents[0];
        assert_eq!(task.task, "Inspect the command UI");
        assert_eq!(task.task_id.as_deref(), Some("subagent-1"));
        assert_eq!(task.model.as_deref(), Some("worker-model"));
        assert_eq!(task.effort.as_deref(), Some("high"));
        assert_eq!(task.status, SubagentStatus::Running);
        assert!(transcript_lines(&state, 80)[0]
            .to_string()
            .contains("↗ subagent  Inspect the command UI  "));
        assert!(!transcript_lines(&state, 80)[0]
            .to_string()
            .contains("→ running"));

        state.complete_subagent(
            "subagent-1",
            serde_json::json!({"model":"worker-model","output":"finished"}),
        );
        assert!(
            state.subagents.is_empty(),
            "completed workers are removed from the live background-task list"
        );
        let completed_line = transcript_lines(&state, 80)[0].to_string();
        assert!(completed_line.contains("Inspect the command UI"));
        assert!(completed_line.contains("→ completed"));
        assert!(!completed_line.contains("{\"task\""));
    }

    #[test]
    fn resumed_subagent_completion_clears_the_live_list_without_rendering_internal_prompt() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-worker".to_owned(),
                        name: "spawn_subagent".to_owned(),
                        arguments: serde_json::json!({"task":"Inspect"}).to_string(),
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-worker".to_owned(),
                    "spawn_subagent".to_owned(),
                    serde_json::json!({"task_id":"subagent-1","status":"queued"}).to_string(),
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 3,
                message: ChatMessage::user(Harness::subagent_notification(
                    &crate::app::SubagentCompletion {
                        task_id: "subagent-1".to_owned(),
                        result: serde_json::json!({"output":"resumed result"}),
                    },
                )),
            },
        ];
        let state = UiState::from_history(&history, "secret", "model", None, true);
        assert!(
            state.subagents.is_empty(),
            "a completed worker is not restored into the live background-task list"
        );
        assert!(!state.transcript.iter().any(|item| {
            matches!(item, TranscriptItem::User { text, .. } if text.contains("Background subagent"))
        }));
    }

    #[test]
    fn subagent_list_reserves_rows_between_prompt_and_status() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.subagents.push(SubagentTask {
            call_id: "call-worker".to_owned(),
            task_id: Some("subagent-1".to_owned()),
            task: "Inspect the command UI and report findings".to_owned(),
            model: Some("worker-model".to_owned()),
            effort: Some("high".to_owned()),
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: Vec::new(),
            stream_chars: 0,
        });
        let area = Rect::new(0, 0, 80, 20);
        let (_, _, _, _, input, status) = ui_layout(&state, area);
        let list = subagent_list_area(&state, input).expect("worker list");
        let prompt = prompt_area(input, &state);
        assert_eq!(list.y, input.y);
        assert_eq!(list.y + list.height, prompt.y);
        assert_eq!(status.y, input.y + input.height);

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("test terminal");
        terminal.draw(|frame| draw(frame, &state)).expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("subagent-1"));
        assert!(screen.contains("worker-model"));
        assert!(screen.contains("high"));
        assert!(screen.contains("Inspect the command UI"));
    }

    #[test]
    fn subagent_rows_use_stable_id_hash_colors() {
        assert_eq!(
            subagent_id_color("subagent-1"),
            subagent_id_color("subagent-1")
        );
        assert_ne!(
            subagent_id_color("subagent-1"),
            subagent_id_color("subagent-2")
        );

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 2)).expect("test terminal");
        let state = UiState::from_history(&[], "secret", "model", None, false);
        let mut state = state;
        state.subagents.push(SubagentTask {
            call_id: "call-worker".to_owned(),
            task_id: Some("subagent-1".to_owned()),
            task: "Inspect".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: Vec::new(),
            stream_chars: 0,
        });
        terminal
            .draw(|frame| draw_subagent_list(frame, &state, Rect::new(0, 0, 80, 1)))
            .expect("draw subagent list");
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].fg,
            subagent_id_color("subagent-1")
        );
        assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Color::Reset);
    }

    #[test]
    fn focused_subagent_renders_live_stream_in_picker_slot() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.subagents.push(SubagentTask {
            call_id: "call-worker".to_owned(),
            task_id: Some("subagent-1".to_owned()),
            task: "Inspect".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: Vec::new(),
            stream_chars: 0,
        });
        state.apply_subagent_activity(SubagentActivity::AssistantDelta {
            task_id: "subagent-1".to_owned(),
            text: "worker output".to_owned(),
        });
        assert!(state.focus_subagent_list_from_input());
        let area = Rect::new(0, 0, 80, 20);
        let (_, picker, stream, _, input, _) = ui_layout(&state, area);
        let stream = stream.expect("stream overlay");
        assert_eq!(stream.y + stream.height, input.y);
        assert!(
            picker.is_none(),
            "focused worker replaces the skill overlay slot"
        );

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("test terminal");
        terminal.draw(|frame| draw(frame, &state)).expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Subagent subagent-1"));
        assert!(screen.contains("worker output"));
        assert_eq!(
            terminal.backend().buffer()[(stream.x + 1, stream.y + 1)].bg,
            Color::Reset
        );
    }

    #[test]
    fn subagent_focus_moves_between_prompt_and_list_with_spatial_arrows() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.input = "prompt".to_owned();
        state.cursor = state.input.chars().count();
        state.subagents.push(SubagentTask {
            call_id: "call-one".to_owned(),
            task_id: Some("one".to_owned()),
            task: "one".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: Vec::new(),
            stream_chars: 0,
        });
        assert!(input_cursor_on_first_row(&state, 20));
        assert!(state.focus_subagent_list_from_input());
        assert_eq!(state.subagent_focus, Some(0));
        assert!(state.move_subagent_focus(false));
        assert_eq!(state.subagent_focus, Some(0));
        assert!(state.move_subagent_focus(true));
        assert_eq!(state.subagent_focus, None);
        assert!(state.focus_subagent_list_from_input());
        state.clear_subagent_focus();
        assert_eq!(state.subagent_focus, None);
    }

    #[test]
    fn spawn_card_completes_on_successful_creation_while_worker_remains_live() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-worker".to_owned(),
                        name: "spawn_subagent".to_owned(),
                        arguments: serde_json::json!({"task":"Inspect"}).to_string(),
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-worker".to_owned(),
                    "spawn_subagent".to_owned(),
                    serde_json::json!({"task_id":"subagent-1","status":"queued"}).to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "model", None, false);
        assert_eq!(state.subagents.len(), 1);
        let line = transcript_lines(&state, 100)[0].to_string();
        assert!(line.contains("→ completed"));
        assert!(!line.contains("running"));
    }

    #[test]
    fn check_subagent_uses_a_compact_one_line_card() {
        let history = vec![
            SessionHistoryRecord::Message {
                timestamp: 1,
                message: ChatMessage::assistant(
                    String::new(),
                    vec![crate::model::ChatToolCall {
                        id: "call-check".to_owned(),
                        name: "check_subagent".to_owned(),
                        arguments: serde_json::json!({"task_id":"subagent-1"}).to_string(),
                    }],
                ),
            },
            SessionHistoryRecord::Message {
                timestamp: 2,
                message: ChatMessage::tool(
                    "call-check".to_owned(),
                    "check_subagent".to_owned(),
                    serde_json::json!({"task_id":"subagent-1","status":"running"}).to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "model", None, false);
        let line = transcript_lines(&state, 100)[0].to_string();
        assert_eq!(line, "⌕ check  subagent-1  → running");
        assert!(!line.contains("[tool:"));
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
        let (narrow_chat, narrow_picker, _, _, narrow_input, _) = ui_layout(&state, area);
        let narrow_scroll = max_scroll_for_area(&state, Size::new(area.width, area.height));

        state.input = "/".to_owned();
        state.input_changed();
        let (broad_chat, broad_picker, _, _, broad_input, _) = ui_layout(&state, area);
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
    fn slash_picker_leaves_the_ready_indicator_in_the_status_line() {
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
            !screen.contains("▁▁▁▁▁"),
            "the idle indicator is hidden from the status line"
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
        let area = tui_viewport(Rect::new(0, 0, 40, 12));
        let (_, picker_area, _, _, input_area, _) = ui_layout(&state, area);
        let picker_area = picker_area.expect("picker area");
        // The square picker shares a boundary with the prompt; no blank row separates them.
        assert_eq!(picker_area.y + picker_area.height, input_area.y);
        assert_eq!(buffer[(picker_area.x, picker_area.y)].symbol(), "┌");
        assert_eq!(
            buffer[(picker_area.x, picker_area.y + picker_area.height - 1)].symbol(),
            "└"
        );
        assert_eq!(buffer[(input_area.x, input_area.y)].symbol(), "┌");
        assert_eq!(buffer[(picker_area.x + 1, picker_area.y + 1)].symbol(), "[");
        assert_eq!(buffer[(picker_area.x + 1, picker_area.y + 2)].symbol(), "/");
        assert_eq!(buffer[(picker_area.x, picker_area.y)].fg, Color::Cyan);
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
        assert_eq!(buffer[(0, 0)].symbol(), "┌");
        assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
        assert_eq!(buffer[(1, 1)].symbol(), "[");
        assert_eq!(buffer[(1, 1)].fg, Color::DarkGray);
        assert_eq!(buffer[(1, 2)].symbol(), "/");
        assert_eq!(buffer[(1, 2)].fg, Color::Cyan);
        assert_eq!(buffer[(1, 3)].symbol(), "/");
        assert_eq!(buffer[(1, 3)].fg, Color::DarkGray);
    }
}
