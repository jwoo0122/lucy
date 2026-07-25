use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect, Size};
use ratatui::prelude::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Terminal;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image as TuiImage, Resize};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::app::Harness;
use crate::cancellation::CancellationToken;
use crate::model::{estimate_context_tokens, ChatMessage};
use crate::protocol::{EventSink, ProtocolEvent};
use crate::provider::ProviderModel;
use crate::redaction::redact_secret;
use crate::session::{Session, SessionHistoryRecord, SessionMetadata};

const EVENT_POLL: Duration = Duration::from_millis(50);
const MAX_DISPLAY_INPUT_CHARS: usize = 16 * 1024;
/// Maximum number of wrapped input rows the input box grows to before it
/// stops expanding and scrolls its contents internally.
const MAX_INPUT_ROWS: u16 = 12;
const TUI_MAX_WIDTH: u16 = 100;
const WELCOME_MESSAGE: &str = "Coding Agent Harness LUCY";
const WELCOME_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const WELCOME_TAGLINE: &str = "An ultra-thin harness for tomorrow's most powerful models";
const GREETING_IMAGE_BYTES: &[u8] = include_bytes!("../assets/greeting.png");
const GREETING_IMAGE_SIZE: Size = Size::new(80, 20);
const GREETING_IMAGE_MIN_SIZE: Size = Size::new(40, 10);
const LOGO_TEXT: &str = include_str!("../logo.txt");
/// Gradient endpoints sampled from the logo.png that logo.txt replaces.
const LOGO_START_COLOR: (u8, u8, u8) = (165, 200, 250);
const LOGO_END_COLOR: (u8, u8, u8) = (221, 144, 234);
const WELCOME_IMAGE_GAP: u16 = 1;
const WELCOME_IMAGE_BRIGHTNESS_PERCENT: u16 = 85;
const WELCOME_START_COLOR: (u8, u8, u8) = (180, 130, 245);
const WELCOME_END_COLOR: (u8, u8, u8) = (0, 180, 180);
const USER_BORDER_COLOR: Color = Color::Rgb(192, 154, 0);
const USER_BORDER_GLYPH: &str = "▌";
const PROMPT_BACKGROUND: Color = Color::Rgb(24, 24, 27);
const BACKGROUND_INDICATOR_BACKGROUND: Color = Color::Rgb(40, 24, 56);
const BACKGROUND_INDICATOR_COLOR: Color = Color::Rgb(190, 140, 255);
const BUSY_INDICATOR_FADE_BASE_RGB: (u8, u8, u8) = (42, 42, 46);
const CONSOLE_STATUS_COLOR: Color = Color::Rgb(144, 144, 148);
const CONSOLE_ACCENT_LAVENDER: (u8, u8, u8) = (145, 70, 220);
const CONSOLE_ACCENT_TEAL: (u8, u8, u8) = (0, 180, 180);
const CONSOLE_ACCENT_CYCLE_DURATION: Duration = Duration::from_secs(15);
const CONSOLE_ACCENT_DESATURATION: f32 = 0.15;
const SKILL_TRIGGER_COLOR: Color = Color::Rgb(80, 255, 245);
const PENDING_TOOL_COLOR_RGB: (u8, u8, u8) = (255, 165, 0);
const PENDING_TOOL_COLOR: Color = Color::Rgb(
    PENDING_TOOL_COLOR_RGB.0,
    PENDING_TOOL_COLOR_RGB.1,
    PENDING_TOOL_COLOR_RGB.2,
);
/// A completed `cmd` call first retains its pending orange, then sweeps to the
/// final result colour from the left edge of the compact tool line.
const TOOL_RESULT_SWEEP_DURATION: Duration = Duration::from_millis(600);
/// Each character spends this portion of the sweep cross-fading. The remaining
/// time staggers those fades from the first character to the last.
const TOOL_RESULT_CHARACTER_FADE_PORTION: f32 = 0.4;
const TOOL_SUCCESS_COLOR_RGB: (u8, u8, u8) = (0, 210, 175);
const TOOL_SUCCESS_COLOR: Color = Color::Rgb(
    TOOL_SUCCESS_COLOR_RGB.0,
    TOOL_SUCCESS_COLOR_RGB.1,
    TOOL_SUCCESS_COLOR_RGB.2,
);
const TOOL_FAILURE_COLOR: Color = Color::Rgb(255, 0, 0);
const TOOL_WARNING_COLOR: Color = Color::Rgb(255, 255, 0);
const QUEUED_MESSAGE_COLOR: Color = Color::Rgb(150, 255, 245);
/// Floating panels are deliberately darker than the console while remaining neutral gray.
const FLOATING_PANEL_BACKGROUND: Color = Color::Rgb(28, 28, 30);
const SKILL_PICKER_BACKGROUND: Color = FLOATING_PANEL_BACKGROUND;
const SECTION_CHROME_COLOR: Color = Color::Rgb(0, 180, 180);
const SKILL_PICKER_MAX_ROWS: usize = 5;
const BUILTIN_COMMANDS: [&str; 3] = ["settings", "session", "exit"];
const SETTINGS_MIN_WIDTH: u16 = 36;
const SETTINGS_MAX_WIDTH: u16 = 88;
const SETTINGS_MIN_HEIGHT: u16 = 8;
const SETTINGS_MAX_HEIGHT: u16 = 22;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TuiOutcome {
    Exit,
    Attach(String),
}

pub(crate) fn run<W: Write>(
    mut harness: Harness,
    resumed: bool,
    stdout: W,
) -> Result<TuiOutcome, String> {
    let secret = harness.provider.api_key();
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
        &harness.session.id,
        &secret,
        &harness.session.llm.model,
        harness.session.llm.effort.as_deref(),
        resumed,
    )
    .with_attached_agents(harness.attached_agents.clone())
    .with_skill_names(skill_names)
    .with_context(context_window, context_tokens);
    state.background_active_count = harness.background_active_count();
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
        EnableFocusChange,
        EnableMouseCapture,
        Hide
    ) {
        return Err(format!("unable to enter terminal UI: {error}"));
    }
    // Kitty keyboard protocol makes Shift+Enter (and other modified keys)
    // distinguishable from plain Enter. Only push it on terminals known to
    // support it; otherwise the enhancement sequence would leak as literal
    // text on screen.
    let keyboard_enhanced = supports_keyboard_enhancement();
    if keyboard_enhanced {
        let _ = execute!(
            backend,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
            )
        );
    }
    // tmux does not proxy the kitty keyboard protocol, but it does
    // recognize modifyOtherKeys (CSI > 4;1m). Enable it so tmux sends
    // extended key sequences in CSI u format, which crossterm parses
    // when PushKeyboardEnhancementFlags has been sent.
    let in_tmux = is_inside_tmux();
    if in_tmux {
        let _ = backend
            .write_all(b"\x1b[>4;1m")
            .and_then(|_| backend.flush());
    }
    // `backend` borrows from `terminal_guard`; all writes are done so
    // the borrow has ended and we can now set the guard flags.
    if keyboard_enhanced {
        terminal_guard.keyboard_enhancement = true;
    }
    if in_tmux {
        terminal_guard.modify_other_keys = true;
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
    wait_for_worker(worker, Duration::from_secs(2));
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
        let request = match requests.recv_timeout(EVENT_POLL) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if harness.has_completed_background_commands() {
                    let cancel = CancellationToken::new();
                    let _ = messages.send(WorkerMessage::Started {
                        cancel: cancel.clone(),
                        user_text: None,
                    });
                    if let Err(error) =
                        harness.handle_background_completions(&mut sink, Some(&cancel))
                    {
                        let message =
                            redact_secret(&error, Some(harness.provider.api_key().as_str()));
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
                    let message = redact_secret(&error, Some(harness.provider.api_key().as_str()));
                    let _ = sink.emit_event(&ProtocolEvent::Error { message });
                }
                let _ = messages.send(WorkerMessage::Finished);
            }
            WorkerRequest::Catalog => {
                let _ = messages.send(WorkerMessage::Catalog(
                    harness.provider.models().map_err(|error| error.to_string()),
                ));
            }
            WorkerRequest::Sessions => {
                let secret = harness.provider.api_key();
                let result = Session::list_with_secret(&harness.home, Some(&secret))
                    .map_err(|error| error.to_string());
                let _ = messages.send(WorkerMessage::Sessions(result));
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
) -> Result<TuiOutcome, String> {
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
                    state.set_busy(true);
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
                Ok(WorkerMessage::Sessions(result)) => state.open_sessions(result),
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
                        return Ok(TuiOutcome::Exit);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if state.busy {
                        return Err("TUI worker stopped unexpectedly".to_owned());
                    }
                    return Ok(TuiOutcome::Exit);
                }
            }
        }

        // Ratatui flushes the buffer diff (which issues MoveTo for every
        // changed cell) before it hides or shows the cursor. If the hardware
        // cursor is visible during that flush it briefly appears at each
        // changed cell. Hide it first so the flush phase never shows it; Ratatui
        // will re-show it at the prompt position after flush when needed.
        let _ = execute!(terminal.backend_mut(), Hide);

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
            if handle_terminal_focus_event(state, &event) {
                continue;
            }
            let key = match event {
                Event::Mouse(mouse) => {
                    let size = terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?;
                    let max_scroll = max_scroll_for_area(state, size);
                    handle_mouse_event(state, mouse.kind, max_scroll);
                    continue;
                }
                Event::Key(key) => key,
                _ => continue,
            };
            if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                continue;
            }
            if is_ctrl_c(&key) {
                if let Some(token) = state.active_cancel.as_ref() {
                    let _ = token.cancel();
                    quitting = true;
                } else {
                    return Ok(TuiOutcome::Exit);
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
            if !state.busy && state.sessions.is_some() {
                if let Some(session_id) = state.handle_sessions_key(&key) {
                    return Ok(TuiOutcome::Attach(session_id));
                }
                continue;
            }
            if key.code == KeyCode::Esc {
                if let Some(token) = state.active_cancel.as_ref() {
                    if token.cancel() {
                        state.set_status("cancelling");
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
                            BuiltinCommand::Session => {
                                state.sessions = Some(SessionsState::Loading);
                                requests
                                    .send(WorkerRequest::Sessions)
                                    .map_err(|_| "TUI worker is unavailable".to_owned())?;
                                continue;
                            }
                            BuiltinCommand::Exit => return Ok(TuiOutcome::Exit),
                        }
                    }
                    state.reset_skill_picker();
                    if text.trim().is_empty() {
                        continue;
                    }
                    state.auto_scroll = true;
                    state.scroll = 0;
                    state.submit_user(&text);
                    state.set_busy(true);
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
                    let size = terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?;
                    let area = tui_viewport(Rect::new(0, 0, size.width, size.height));
                    let input_width = ui_prompt_content_width(area).max(1) as usize;
                    if !move_up_from_input(state, input_width) {
                        let max_scroll = max_scroll_for_area(state, size);
                        scroll_up(state, max_scroll);
                    }
                }
                KeyCode::Down => {
                    let size = terminal
                        .size()
                        .map_err(|error| format!("unable to read terminal size: {error}"))?;
                    let area = tui_viewport(Rect::new(0, 0, size.width, size.height));
                    let input_width = ui_prompt_content_width(area).max(1) as usize;
                    if !move_down_from_input(state, input_width) {
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

fn handle_terminal_focus_event(state: &mut UiState, event: &Event) -> bool {
    match event {
        Event::FocusGained => state.terminal_focused = true,
        Event::FocusLost => state.terminal_focused = false,
        _ => return false,
    }
    true
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
    modify_other_keys: bool,
}

impl<W: Write> TerminalGuard<W> {
    fn new(terminal: Terminal<CrosstermBackend<W>>) -> Self {
        Self {
            terminal: Some(terminal),
            keyboard_enhancement: false,
            modify_other_keys: false,
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
        if self.modify_other_keys {
            let _ = terminal
                .backend_mut()
                .write_all(b"\x1b[>4;0m")
                .and_then(|_| terminal.backend_mut().flush());
        }
        if self.keyboard_enhancement {
            let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        let _ = terminal.show_cursor();
        let _ = disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            DisableFocusChange,
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
    if matches!(
        program.as_str(),
        "ghostty" | "kitty" | "wezterm" | "alacritty" | "foot" | "footclient" | "iterm.app"
    ) {
        return true;
    }
    // tmux does not support the kitty keyboard protocol (CSI > flags u)
    // passthrough, but it does support modifyOtherKeys (CSI > 4;1m). Push
    // kitty flags anyway so crossterm parses CSI u format sequences, and
    // separately enable modifyOtherKeys so tmux sends extended keys.
    if program == "tmux" {
        return true;
    }
    false
}

/// Whether the process is running inside a tmux session.
fn is_inside_tmux() -> bool {
    std::env::var("TERM_PROGRAM")
        .map(|value| value.eq_ignore_ascii_case("tmux"))
        .unwrap_or(false)
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
    state.set_busy(false);
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
    Sessions,
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
    Sessions(Result<Vec<SessionMetadata>, String>),
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

#[derive(Debug, Clone)]
struct ActivityTransition {
    started_at: Instant,
    from_levels: [usize; PULSE_BAR_PERIODS.len()],
    to_levels: [usize; PULSE_BAR_PERIODS.len()],
}

struct UiState {
    active_session_id: String,
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
    terminal_focused: bool,
    active_cancel: Option<CancellationToken>,
    scroll: u16,
    auto_scroll: bool,
    tool_animation_epoch: Instant,
    console_animation_epoch: Instant,
    activity_started_at: Instant,
    activity_transition: Option<ActivityTransition>,
    last_active_levels: [usize; PULSE_BAR_PERIODS.len()],
    last_active_elapsed: Duration,
    welcome_visible: bool,
    attached_agents: Vec<String>,
    cmd_result_started_at: HashMap<String, Instant>,
    skill_names: Vec<String>,
    skill_picker_focus: usize,
    skill_picker_suppressed: bool,
    settings: Option<SettingsState>,
    sessions: Option<SessionsState>,
    background_active_count: Arc<AtomicUsize>,
}

impl UiState {
    fn from_history(
        history: &[SessionHistoryRecord],
        active_session_id: &str,
        secret: &str,
        model: &str,
        effort: Option<&str>,
        resumed: bool,
    ) -> Self {
        let mut state = Self {
            active_session_id: active_session_id.to_owned(),
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
            terminal_focused: true,
            active_cancel: None,
            scroll: 0,
            auto_scroll: true,
            tool_animation_epoch: Instant::now(),
            console_animation_epoch: Instant::now(),
            activity_started_at: Instant::now(),
            activity_transition: None,
            last_active_levels: [0; PULSE_BAR_PERIODS.len()],
            last_active_elapsed: Duration::ZERO,
            welcome_visible: !resumed && history.is_empty(),
            attached_agents: Vec::new(),
            cmd_result_started_at: HashMap::new(),
            skill_names: Vec::new(),
            skill_picker_focus: 0,
            skill_picker_suppressed: false,
            settings: None,
            sessions: None,
            background_active_count: Arc::new(AtomicUsize::new(0)),
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

    fn set_busy(&mut self, busy: bool) {
        self.set_busy_at(busy, Instant::now());
    }

    fn set_busy_at(&mut self, busy: bool, now: Instant) {
        if self.busy == busy {
            return;
        }
        if busy {
            self.console_animation_epoch = now;
        }
        self.busy = busy;
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

    fn console_animation_elapsed_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.console_animation_epoch)
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
    fn open_sessions(&mut self, result: Result<Vec<SessionMetadata>, String>) {
        if self.sessions.is_none() {
            return;
        }
        self.sessions = Some(match result {
            Ok(mut sessions) => {
                sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
                SessionsState::Sessions {
                    sessions,
                    query: String::new(),
                    focus: 0,
                }
            }
            Err(error) => SessionsState::Error(error),
        });
    }
    fn handle_sessions_key(&mut self, key: &KeyEvent) -> Option<String> {
        let active_session_id = self.active_session_id.clone();
        match self.sessions.as_mut()? {
            SessionsState::Loading => {
                if key.code == KeyCode::Esc {
                    self.sessions = None;
                }
            }
            SessionsState::Error(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.sessions = None;
                }
            }
            SessionsState::Sessions {
                sessions,
                query,
                focus,
            } => match key.code {
                KeyCode::Esc => self.sessions = None,
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
                    let count = filtered_sessions(sessions, query).count();
                    *focus = (*focus + 1).min(count.saturating_sub(1));
                }
                KeyCode::Enter => {
                    let selected_session_id = filtered_sessions(sessions, query)
                        .nth(*focus)
                        .map(|session| session.session_id.clone());
                    if selected_session_id.as_deref() == Some(active_session_id.as_str()) {
                        self.sessions = None;
                        return None;
                    }
                    return selected_session_id;
                }
                _ => {}
            },
        }
        None
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
                let secret = self.secret.clone();
                self.add_user(text, &secret);
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
        self.record_tool_call(call, false);
    }

    fn add_live_tool_call(&mut self, call: &crate::model::ChatToolCall) {
        self.record_tool_call(call, true);
    }

    fn record_tool_call(&mut self, call: &crate::model::ChatToolCall, _live: bool) {
        self.clear_thinking();
        self.transcript.push(TranscriptItem::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
    }

    fn add_tool_result(&mut self, id: &str, name: &str, result: Value) {
        self.record_tool_result(id, name, result, false);
    }

    fn add_live_tool_result(&mut self, id: &str, name: &str, result: Value) {
        self.record_tool_result(id, name, result, true);
    }

    fn record_tool_result(&mut self, id: &str, name: &str, result: Value, animate: bool) {
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

    fn apply_event(&mut self, event: ProtocolEvent) {
        match event {
            ProtocolEvent::Session { .. } => {}
            ProtocolEvent::AssistantDelta { text } => self.add_assistant(&text),
            ProtocolEvent::ToolCall {
                id,
                name,
                arguments,
            } => self.add_live_tool_call(&crate::model::ChatToolCall {
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

/// Center the TUI while reserving one terminal cell on each side when possible.
/// Extremely narrow terminals retain their full width because two margins would
/// leave no usable content area.
fn tui_viewport(area: Rect) -> Rect {
    if area.width <= 2 {
        return area;
    }

    let width = area.width.saturating_sub(2).min(TUI_MAX_WIDTH);
    let x = area.x + area.width.saturating_sub(width) / 2;
    Rect::new(x, area.y, width, area.height)
}

fn background_indicator_height(state: &UiState) -> u16 {
    u16::from(state.background_active_count.load(Ordering::Relaxed) > 0)
}

fn background_indicator_area(state: &UiState, input_area: Rect) -> Option<Rect> {
    (background_indicator_height(state) > 0).then(|| {
        Rect::new(
            input_area.x,
            input_area.y + input_area.height,
            input_area.width,
            1,
        )
    })
}

fn ui_layout(
    state: &UiState,
    area: Rect,
) -> (Rect, Option<Rect>, Option<Rect>, Option<Rect>, Rect, Rect) {
    let prompt_rows = input_visible_rows(state, ui_prompt_content_width(area));
    let list_height = 0;
    let queue_height = message_queue_height(state);
    let queue_separator_height = u16::from(queue_height > 0);
    let list_separator_height = u16::from(list_height > 0);
    let requested_input_height = prompt_rows.clamp(1, MAX_INPUT_ROWS)
        + queue_height
        + queue_separator_height
        + list_height
        + list_separator_height
        + 1 // prompt/status separator
        + 1 // status line
        + 2; // blank outer border space
             // Preserve a one-row footer around the console when there is room for a
             // console at all. On a one-row terminal the console takes that row rather
             // than collapsing to an unusable rectangle.
    let bottom_margin = u16::from(area.height > 1);
    let usable_height = area
        .height
        .saturating_sub(bottom_margin)
        .saturating_sub(background_indicator_height(state));
    let input_height = requested_input_height.min(usable_height);
    let transcript_gap_height = u16::from(usable_height >= input_height.saturating_add(2));
    let chat_height = usable_height.saturating_sub(input_height + transcript_gap_height);
    let chat_chunk = bottom_console_area(area, area.y, chat_height);
    let input_area = bottom_console_area(
        area,
        area.y + chat_height + transcript_gap_height,
        input_height,
    );
    let inner = console_content_area(input_area);
    let content = bottom_content_heights(state, input_area);
    let available_above = input_area.y.saturating_sub(area.y);
    let picker_height = skill_picker_height(state).min(available_above);
    let picker_area = (picker_height > 0).then(|| {
        Rect::new(
            input_area.x,
            input_area.y - picker_height,
            input_area.width,
            picker_height,
        )
    });
    let stream_area = None;
    let queue_area =
        (content.queue > 0).then(|| Rect::new(inner.x, inner.y, inner.width, content.queue));
    let status_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(content.status),
        inner.width,
        content.status,
    );
    (
        chat_chunk,
        picker_area,
        stream_area,
        queue_area,
        input_area,
        status_area,
    )
}

/// Keep the content area inset without allowing margins to consume all
/// available width. A narrow terminal sheds margin cells before it sheds the
/// console.
const CONTENT_HORIZONTAL_MARGIN: u16 = 7;
const MIN_CONSOLE_WIDTH: u16 = 14;

fn bottom_console_area(area: Rect, y: u16, height: u16) -> Rect {
    let horizontal_margin = area.width.saturating_sub(1) / 2;
    let margin_cap = if area.width < MIN_CONSOLE_WIDTH {
        2
    } else {
        CONTENT_HORIZONTAL_MARGIN.min(area.width.saturating_sub(MIN_CONSOLE_WIDTH) / 2)
    };
    let horizontal_margin = horizontal_margin.min(margin_cap);
    Rect::new(
        area.x.saturating_add(horizontal_margin),
        y,
        area.width
            .saturating_sub(horizontal_margin.saturating_mul(2)),
        height,
    )
}

fn ui_prompt_content_width(area: Rect) -> u16 {
    prompt_content_width(bottom_console_area(area, area.y, 0).width)
}

fn console_content_area(input_area: Rect) -> Rect {
    let top_padding = input_area.height.min(1);
    let bottom_padding = input_area.height.saturating_sub(top_padding).min(1);
    Rect::new(
        input_area.x.saturating_add(2),
        input_area.y.saturating_add(top_padding),
        input_area.width.saturating_sub(4),
        input_area
            .height
            .saturating_sub(top_padding + bottom_padding),
    )
}

fn prompt_content_width(input_width: u16) -> u16 {
    input_width.saturating_sub(4)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BottomContentHeights {
    queue: u16,
    queue_separator: u16,
    list: u16,
    list_separator: u16,
    prompt: u16,
    status_separator: u16,
    status: u16,
}

// Constrained layouts keep the status and prompt first. Queue and worker
// sections each require a header, one entry, and their following spacer so a
// clipped console never renders an orphaned section header.
fn bottom_content_heights(state: &UiState, input_area: Rect) -> BottomContentHeights {
    let mut available = console_content_area(input_area).height;
    let status = available.min(1);
    available -= status;

    let prompt = input_visible_rows(state, prompt_content_width(input_area.width))
        .clamp(1, MAX_INPUT_ROWS)
        .min(available);
    available -= prompt;

    let status_separator = u16::from(status > 0 && prompt > 0 && available > 0);
    available -= status_separator;

    let requested_queue = message_queue_height(state);
    let (queue, queue_separator) = if requested_queue > 0 && available >= 3 {
        (requested_queue.min(available - 1), 1)
    } else {
        (0, 0)
    };
    available -= queue + queue_separator;

    let requested_list = 0;
    let (list, list_separator) = if requested_list > 0 && available >= 3 {
        (requested_list.min(available - 1), 1)
    } else {
        (0, 0)
    };

    BottomContentHeights {
        queue,
        queue_separator,
        list,
        list_separator,
        prompt,
        status_separator,
        status,
    }
}

fn prompt_area(input_area: Rect, state: &UiState) -> Rect {
    let inner = console_content_area(input_area);
    let content = bottom_content_heights(state, input_area);
    Rect::new(
        inner.x,
        inner.y + content.queue + content.queue_separator,
        inner.width,
        content.prompt,
    )
}

fn message_queue_height(state: &UiState) -> u16 {
    let messages = state.queued_messages.len().min(u16::MAX as usize - 1) as u16;
    u16::from(messages > 0) + messages
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

const TRANSCRIPT_SCROLLBAR_TRACK: &str = "┆";
const TRANSCRIPT_SCROLLBAR_THUMB: &str = "█";
const TRANSCRIPT_SCROLLBAR_TRACK_COLOR: Color = Color::Rgb(72, 72, 76);

fn draw_transcript_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    total_lines: usize,
    max_scroll: u16,
    scroll: u16,
) {
    if area.width == 0 || area.height == 0 || total_lines == 0 || max_scroll == 0 {
        return;
    }

    let track_height = area.height as usize;
    let thumb_height = ((track_height * track_height) / total_lines)
        .max(1)
        .min(track_height);
    let thumb_range = track_height.saturating_sub(thumb_height);
    let thumb_start = (usize::from(scroll.min(max_scroll)) * thumb_range / usize::from(max_scroll))
        .min(thumb_range);
    // Keep the transcript's final column visible. Cramped layouts without a
    // right gutter omit the scrollbar rather than covering message content.
    let x = area.x.saturating_add(area.width);
    let frame_right = frame.area().x.saturating_add(frame.area().width);
    if x >= frame_right {
        return;
    }
    let buffer = frame.buffer_mut();

    for offset in 0..track_height {
        let y = area.y + offset as u16;
        buffer[(x, y)].set_symbol(TRANSCRIPT_SCROLLBAR_TRACK);
        buffer[(x, y)].set_fg(TRANSCRIPT_SCROLLBAR_TRACK_COLOR);
    }
    for offset in thumb_start..thumb_start + thumb_height {
        let y = area.y + offset as u16;
        buffer[(x, y)].set_symbol(TRANSCRIPT_SCROLLBAR_THUMB);
        buffer[(x, y)].set_fg(CONSOLE_STATUS_COLOR);
    }
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
    Session,
    Exit,
}

impl BuiltinCommand {
    fn name(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Session => "session",
            Self::Exit => "exit",
        }
    }
}

fn builtin_command(input: &str) -> Option<BuiltinCommand> {
    match input.split_whitespace().next()? {
        "/settings" => Some(BuiltinCommand::Settings),
        "/session" => Some(BuiltinCommand::Session),
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
        // Header, visible commands, and the vertical inset.
        (state
            .matching_skill_names()
            .len()
            .min(SKILL_PICKER_MAX_ROWS)
            + 3) as u16
    } else {
        0
    }
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
        Span::styled(text, Style::default().fg(SKILL_TRIGGER_COLOR))
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

fn cursor_row(input: &str, cursor: usize, width: usize) -> u16 {
    input_cursor_row(input, cursor, width).min(u16::MAX as usize) as u16
}

fn move_up_from_input(state: &mut UiState, width: usize) -> bool {
    state.move_skill_picker(false) || move_input_cursor_vertical(state, width, false)
}

fn move_down_from_input(state: &mut UiState, width: usize) -> bool {
    let width = width.max(1);
    state.move_skill_picker(true) || move_input_cursor_vertical(state, width, true)
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
    let (chat_chunk, picker_area, _, queue_area, input_chunk, status_area) = ui_layout(state, area);

    // The queue, prompt, and status line share one background surface.
    // The transient picker remains above it.
    let visible_chat_area = chat_chunk;

    let width = chat_chunk.width;
    let welcome_image_layout = if state.welcome_visible && greeting_image_enabled() {
        let welcome_lines = welcome_lines(&state.attached_agents);
        welcome_image_layout(visible_chat_area, welcome_lines.len() as u16)
    } else {
        None
    };
    if state.welcome_visible {
        let welcome_lines = welcome_lines(&state.attached_agents);
        if let Some(layout) = welcome_image_layout {
            let welcome = Paragraph::new(welcome_lines).alignment(Alignment::Center);
            frame.render_widget(welcome, layout.intro_area);
        } else {
            let logo = logo_lines();
            let logo_gap = 2u16;
            let total_height = logo.len() as u16 + logo_gap + welcome_lines.len() as u16;
            // Show the logo only when the chat area can fit the logo, gap,
            // and welcome text; otherwise fall back to text-only.
            let lines = if total_height <= visible_chat_area.height {
                let mut all = logo;
                all.push(Line::raw(""));
                all.push(Line::raw(""));
                all.extend(welcome_lines);
                all
            } else {
                welcome_lines
            };
            let welcome_height = (lines.len() as u16).min(visible_chat_area.height);
            let welcome_area = Rect::new(
                visible_chat_area.x,
                visible_chat_area.y + visible_chat_area.height.saturating_sub(welcome_height) / 2,
                visible_chat_area.width,
                welcome_height,
            );
            let welcome = Paragraph::new(lines).alignment(Alignment::Center);
            frame.render_widget(welcome, welcome_area);
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
        let total_lines = lines.len();
        let transcript = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(transcript, visible_chat_area);
        if !state.auto_scroll {
            draw_transcript_scrollbar(frame, visible_chat_area, total_lines, max_scroll, scroll);
        }
    }

    frame.render_widget(
        Block::default().style(Style::default().bg(PROMPT_BACKGROUND)),
        input_chunk,
    );
    if let Some(indicator_area) = background_indicator_area(state, input_chunk) {
        let active_count = state.background_active_count.load(Ordering::Relaxed);
        let indicator_style = Style::default()
            .fg(BACKGROUND_INDICATOR_COLOR)
            .bg(BACKGROUND_INDICATOR_BACKGROUND);
        frame.render_widget(
            Block::default().style(Style::default().bg(BACKGROUND_INDICATOR_BACKGROUND)),
            indicator_area,
        );
        frame.render_widget(
            Paragraph::new(format!("Background task(s) {active_count} is running..."))
                .style(indicator_style),
            indicator_area,
        );
    }

    if let Some(layout) = welcome_image_layout {
        let image = welcome_image(layout.image_size);
        frame.render_widget(TuiImage::new(image.as_ref()), layout.image_area);
    }
    if let Some(picker_area) = picker_area {
        draw_skill_picker(frame, state, picker_area);
    }

    if let Some(queue_area) = queue_area {
        draw_message_queue(frame, state, queue_area);
    }

    let input_text_style = Style::default().fg(Color::White);
    let prompt_area = prompt_area(input_chunk, state);
    let prompt = input_display_text(state);
    let input_rows = input_visible_rows(state, prompt_area.width).clamp(1, MAX_INPUT_ROWS);
    let wrapped = wrap_text(&prompt, prompt_area.width.max(1) as usize);
    let visible = (wrapped.len() as u16)
        .clamp(1, input_rows)
        .min(prompt_area.height);
    let cursor_row = cursor_row(&prompt, state.cursor, prompt_area.width.max(1) as usize);
    let bottom_scroll = (wrapped.len() as u16).saturating_sub(visible);
    let cursor_scroll = (cursor_row + 1).saturating_sub(visible);
    let input_scroll = cursor_scroll.min(bottom_scroll);
    let active_skill_trigger = (!state.busy)
        .then(|| active_skill_trigger(&prompt, &state.skill_names))
        .flatten();
    let input_lines = styled_text_lines(
        &prompt,
        active_skill_trigger,
        prompt_area.width.max(1) as usize,
        input_text_style,
    );
    let input = Paragraph::new(input_lines)
        .style(input_text_style)
        .scroll((input_scroll, 0));
    frame.render_widget(input, prompt_area);

    let effort = state.effort.as_deref().unwrap_or("default");
    frame.render_widget(
        Paragraph::new(model_status_line(state, effort, status_area.width)),
        status_area,
    );

    if let Some(settings) = &state.settings {
        draw_settings(frame, settings, area);
    }
    if let Some(sessions) = &state.sessions {
        draw_sessions(frame, sessions, area, &state.secret);
    }

    // A frame cursor makes Ratatui issue `Show` after every redraw. Only set
    // one while focused.
    if state.terminal_focused
        && state.settings.is_none()
        && state.sessions.is_none()
        && !prompt_area.is_empty()
        && visible > 0
    {
        let cursor_prefix: String = prompt.chars().take(state.cursor).collect();
        let cursor_rows = wrap_text(&cursor_prefix, prompt_area.width.max(1) as usize);
        let cursor_line = cursor_rows.last().map(String::as_str).unwrap_or("");
        let cursor_offset = UnicodeWidthStr::width(cursor_line) as u16;
        let cursor_x = prompt_area.x + cursor_offset.min(prompt_area.width.saturating_sub(1));
        let cursor_y = prompt_area.y
            + cursor_row
                .saturating_sub(input_scroll)
                .min(prompt_area.height.saturating_sub(1));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_message_queue(frame: &mut Frame<'_>, state: &UiState, area: Rect) {
    if area.is_empty() || state.queued_messages.is_empty() {
        return;
    }

    let chrome = Style::default().fg(SECTION_CHROME_COLOR);
    let message = Style::default().fg(QUEUED_MESSAGE_COLOR);
    let mut lines = vec![Line::styled("Queued", chrome)];
    lines.extend(
        state
            .queued_messages
            .iter()
            .take(area.height.saturating_sub(1) as usize)
            .enumerate()
            .map(|(index, queued)| {
                Line::from(vec![
                    Span::styled("│ ", chrome),
                    Span::styled(
                        format!("{}) {}", index + 1, single_line_preview(queued)),
                        message,
                    ),
                ])
            }),
    );
    frame.render_widget(Paragraph::new(lines), area);
}

fn single_line_preview(text: &str) -> String {
    truncate_output(&text.replace(['\n', '\r'], " ↵ "))
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

enum SessionsState {
    Loading,
    Error(String),
    Sessions {
        sessions: Vec<SessionMetadata>,
        query: String,
        focus: usize,
    },
}

fn filtered_sessions<'a>(
    sessions: &'a [SessionMetadata],
    query: &str,
) -> impl Iterator<Item = &'a SessionMetadata> {
    let query = query.to_lowercase();
    sessions.iter().filter(move |session| {
        session.session_id.to_lowercase().contains(&query)
            || session
                .first_message
                .as_deref()
                .is_some_and(|message| message.to_lowercase().contains(&query))
            || session
                .last_message
                .as_deref()
                .is_some_and(|message| message.to_lowercase().contains(&query))
    })
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

fn draw_sessions(frame: &mut Frame<'_>, sessions: &SessionsState, area: Rect, secret: &str) {
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
        .title(" /session ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = match sessions {
        SessionsState::Loading => vec![
            Line::styled("Loading sessions…", Style::default().fg(Color::Cyan)),
            Line::raw(""),
            Line::styled("Esc  cancel", Style::default().fg(Color::DarkGray)),
        ],
        SessionsState::Error(error) => vec![
            Line::styled("Unable to list sessions", Style::default().fg(Color::Red)),
            Line::raw(""),
            Line::raw(redact_secret(error, Some(secret))),
            Line::raw(""),
            Line::styled("Enter/Esc  close", Style::default().fg(Color::DarkGray)),
        ],
        SessionsState::Sessions {
            sessions,
            query,
            focus,
        } => {
            let filtered = filtered_sessions(sessions, query).collect::<Vec<_>>();
            let focus = (*focus).min(filtered.len().saturating_sub(1));
            let list_rows = inner.height.saturating_sub(4) as usize / 2;
            let range = selection_range(filtered.len(), focus, list_rows.max(1));
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Filter  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if query.is_empty() {
                            "type to filter…".to_owned()
                        } else {
                            redact_secret(query, Some(secret))
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
                        "{} sessions{}",
                        filtered.len(),
                        if filtered.is_empty() {
                            ""
                        } else {
                            " · ↑/↓ move · Enter attach"
                        }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if filtered.is_empty() {
                lines.push(Line::styled(
                    if sessions.is_empty() {
                        "No sessions found"
                    } else {
                        "No matching sessions"
                    },
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                for index in range {
                    let session = filtered[index];
                    let selected = index == focus;
                    let style = if selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    lines.push(Line::styled(
                        format!(
                            "{} {} · {}",
                            if selected { "›" } else { " " },
                            redact_secret(&session.session_id, Some(secret)),
                            format_session_time(session.updated_at)
                        ),
                        style,
                    ));
                    let first = session.first_message.as_deref().unwrap_or("—");
                    let last = session.last_message.as_deref().unwrap_or("—");
                    lines.push(Line::styled(
                        format!(
                            "  {} → {}",
                            single_line_preview(&redact_secret(first, Some(secret))),
                            single_line_preview(&redact_secret(last, Some(secret)))
                        ),
                        style,
                    ));
                }
            }
            lines.push(Line::styled(
                "Esc  cancel",
                Style::default().fg(Color::DarkGray),
            ));
            lines
        }
    };
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(FLOATING_PANEL_BACKGROUND)),
        inner,
    );
}

fn format_session_time(updated_at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(updated_at);
    let elapsed_seconds = now.saturating_sub(updated_at) / 1000;
    match elapsed_seconds {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m ago", elapsed_seconds / 60),
        3_600..=86_399 => format!("{}h ago", elapsed_seconds / 3_600),
        _ => format!("{}d ago", elapsed_seconds / 86_400),
    }
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
    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    let buffer = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            buffer[(x, y)].set_bg(SKILL_PICKER_BACKGROUND);
        }
    }
    if inner.is_empty() {
        return;
    }

    let focus = state.skill_picker_focus.min(total - 1);
    let header = Line::styled(
        format!("[{}/{}]", focus + 1, total),
        Style::default().fg(QUEUED_MESSAGE_COLOR),
    );
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let item_rows = inner.height.saturating_sub(1) as usize;
    for (row, index) in selection_range(total, focus, item_rows).enumerate() {
        let mut style = Style::default().fg(QUEUED_MESSAGE_COLOR);
        if index == focus {
            style = style.add_modifier(Modifier::BOLD);
        }
        let skill = Line::styled(format!("/{}", matches[index]), style);
        frame.render_widget(
            Paragraph::new(skill),
            Rect::new(inner.x, inner.y + 1 + row as u16, inner.width, 1),
        );
    }
}

fn greeting_image_enabled() -> bool {
    std::env::var("LUCY_GREETING_IMAGE").as_deref() == Ok("true")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WelcomeImageLayout {
    image_area: Rect,
    intro_area: Rect,
    image_size: Size,
}

fn welcome_image_layout(area: Rect, intro_height: u16) -> Option<WelcomeImageLayout> {
    let available_height = area
        .height
        .saturating_sub(intro_height.saturating_add(WELCOME_IMAGE_GAP));
    let max_width = area.width.min(GREETING_IMAGE_SIZE.width);
    let max_height = available_height.min(GREETING_IMAGE_SIZE.height);
    let aspect_width = GREETING_IMAGE_SIZE.width / GREETING_IMAGE_SIZE.height;
    let image_height = max_height.min(max_width / aspect_width);
    let image_size = Size::new(image_height * aspect_width, image_height);
    if image_size.width < GREETING_IMAGE_MIN_SIZE.width
        || image_size.height < GREETING_IMAGE_MIN_SIZE.height
    {
        return None;
    }

    let group_height = image_size.height + WELCOME_IMAGE_GAP + intro_height;
    let group_y = area.y + area.height.saturating_sub(group_height) / 2;
    Some(WelcomeImageLayout {
        image_area: Rect::new(
            area.x + (area.width - image_size.width) / 2,
            group_y,
            image_size.width,
            image_size.height,
        ),
        intro_area: Rect::new(
            area.x,
            group_y + image_size.height + WELCOME_IMAGE_GAP,
            area.width,
            intro_height,
        ),
        image_size,
    })
}

type WelcomeImageCache = Mutex<HashMap<(u16, u16), Arc<Protocol>>>;

fn welcome_image(size: Size) -> Arc<Protocol> {
    static IMAGES: OnceLock<WelcomeImageCache> = OnceLock::new();
    let images = IMAGES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut images = images
        .lock()
        .expect("welcome image cache should not be poisoned");
    images
        .entry((size.width, size.height))
        .or_insert_with(|| {
            let image = image::load_from_memory(GREETING_IMAGE_BYTES)
                .expect("embedded greeting PNG should decode");
            let image = dim_welcome_image(image);
            Arc::new(
                Picker::halfblocks()
                    .new_protocol(image, size, Resize::Fit(None))
                    .expect("embedded greeting PNG should convert to halfblocks"),
            )
        })
        .clone()
}

fn dim_welcome_image(image: image::DynamicImage) -> image::DynamicImage {
    let mut image = image.to_rgba8();
    for pixel in image.pixels_mut() {
        for channel in pixel.0.iter_mut().take(3) {
            *channel = (u16::from(*channel) * WELCOME_IMAGE_BRIGHTNESS_PERCENT / 100) as u8;
        }
    }
    image::DynamicImage::ImageRgba8(image)
}

fn logo_lines() -> Vec<Line<'static>> {
    let max_width = LOGO_TEXT
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    LOGO_TEXT
        .lines()
        .map(|line| {
            let spans: Vec<Span> = line
                .chars()
                .enumerate()
                .map(|(index, character)| {
                    let progress = if max_width <= 1 {
                        0.0
                    } else {
                        index as f32 / (max_width - 1) as f32
                    };
                    let color = Color::Rgb(
                        interpolate_color(LOGO_START_COLOR.0, LOGO_END_COLOR.0, progress),
                        interpolate_color(LOGO_START_COLOR.1, LOGO_END_COLOR.1, progress),
                        interpolate_color(LOGO_START_COLOR.2, LOGO_END_COLOR.2, progress),
                    );
                    Span::styled(character.to_string(), Style::default().fg(color))
                })
                .collect();
            Line::from(spans)
        })
        .collect()
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
        Line::styled(WELCOME_VERSION, Style::default().fg(Color::DarkGray)),
        Line::raw(""),
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
    render_transcript_items(&state.transcript, width.max(1) as usize, state)
}

fn render_transcript_items(
    transcript: &[TranscriptItem],
    width: usize,
    state: &UiState,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut rendered_item = false;

    for (index, item) in transcript.iter().enumerate() {
        // Results are positioned on their matching call, even when the model
        // emitted several calls before execution produced any result.
        if is_result_attached_to_call(transcript, index) {
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
                let result = matching_tool_result(transcript, index, id);
                let segments = if name == "cmd" {
                    cmd_tool_segments(id, arguments, result, state)
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

/// Tool work uses its own clock instead of the main status animation.
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
    let character_position = if character_count <= 1 {
        0.0
    } else {
        character_index as f32 / (character_count - 1) as f32
    };
    let fade_start = character_position * (1.0 - TOOL_RESULT_CHARACTER_FADE_PORTION);
    let character_progress =
        ((progress - fade_start) / TOOL_RESULT_CHARACTER_FADE_PORTION).clamp(0.0, 1.0);
    let character_progress =
        character_progress * character_progress * (3.0 - 2.0 * character_progress);
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
        return TOOL_WARNING_COLOR;
    }
    if result.get("error").is_some()
        || matches!(result.get("exit_code").and_then(Value::as_i64), Some(code) if code != 0)
    {
        return TOOL_FAILURE_COLOR;
    }
    TOOL_SUCCESS_COLOR
}

fn tool_result_color_rgb(color: Color) -> (u8, u8, u8) {
    let Color::Rgb(red, green, blue) = color else {
        unreachable!("cmd result transition colours are RGB")
    };
    (red, green, blue)
}

fn cmd_result_status(result: &Value) -> (char, String, Style) {
    let target = cmd_result_target_color(result);
    if result.get("status").and_then(Value::as_str) == Some("running") {
        let id = result
            .get("background_id")
            .and_then(Value::as_str)
            .unwrap_or("background");
        return ('↗', id.to_owned(), Style::default().fg(target));
    }
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
        return format!("Context: {used}/? (?%) ??????????");
    };
    let percentage = context_percentage(state.context_tokens, window);
    format!(
        "Context: {used}/{} ({percentage}%) {}",
        format_context_tokens(window),
        context_progress_bar(state.context_tokens, window)
    )
}

fn context_progress_bar(used: usize, window: usize) -> String {
    const WIDTH: usize = 10;
    let filled = if window == 0 {
        0
    } else {
        (used as u128 * WIDTH as u128)
            .div_ceil(window as u128)
            .min(WIDTH as u128) as usize
    };
    format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled))
}

fn context_status_style(_state: &UiState) -> Style {
    Style::default().fg(CONSOLE_STATUS_COLOR)
}

fn context_percentage(used: usize, window: usize) -> usize {
    if window == 0 {
        return 0;
    }
    ((used as u128 * 100).div_ceil(window as u128)) as usize
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

fn model_status_line(state: &UiState, effort: &str, width: u16) -> Line<'static> {
    model_status_line_at(
        state,
        effort,
        state.console_animation_elapsed_at(Instant::now()),
        width,
    )
}

fn model_status_line_at(
    state: &UiState,
    effort: &str,
    elapsed: Duration,
    width: u16,
) -> Line<'static> {
    let model = redact_secret(&state.model, Some(&state.secret));
    let effort = redact_secret(effort, Some(&state.secret));
    let context = context_status_text(state);
    let context_width = UnicodeWidthStr::width(context.as_str());
    let model_style = if state.busy {
        Style::default().fg(console_accent_at(elapsed))
    } else {
        context_status_style(state)
    };
    let status_style = context_status_style(state);
    let mut spans = vec![
        Span::styled(model, model_style),
        Span::styled(format!(" · {effort}"), status_style),
    ];
    if state.busy {
        let accent = console_accent_at(elapsed);
        let (head, _) = busy_indicator_position_at(elapsed);
        spans.push(Span::raw(" "));
        for (index, character) in busy_indicator_frame_at(elapsed).chars().enumerate() {
            let distance = if character == BUSY_INDICATOR_BLOCK && index != head {
                Some(index.abs_diff(head))
            } else {
                None
            };
            let color = busy_indicator_color(accent, distance);
            spans.push(Span::styled(
                character.to_string(),
                Style::default().fg(color),
            ));
        }
    }
    let left_width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let gap = usize::from(width).saturating_sub(left_width + context_width);
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.push(Span::styled(context, status_style));
    Line::from(spans)
}

fn console_accent_cycle() -> Duration {
    CONSOLE_ACCENT_CYCLE_DURATION
}

fn console_accent_at(elapsed: Duration) -> Color {
    let cycle_progress =
        (elapsed.as_secs_f32() / console_accent_cycle().as_secs_f32()).rem_euclid(1.0);
    let progress = if cycle_progress <= 0.5 {
        cycle_progress * 2.0
    } else {
        (1.0 - cycle_progress) * 2.0
    };
    desaturate_console_accent(
        interpolate_color(CONSOLE_ACCENT_LAVENDER.0, CONSOLE_ACCENT_TEAL.0, progress),
        interpolate_color(CONSOLE_ACCENT_LAVENDER.1, CONSOLE_ACCENT_TEAL.1, progress),
        interpolate_color(CONSOLE_ACCENT_LAVENDER.2, CONSOLE_ACCENT_TEAL.2, progress),
    )
}

fn desaturate_console_accent(red: u8, green: u8, blue: u8) -> Color {
    let neutral = ((u16::from(red) + u16::from(green) + u16::from(blue)) / 3) as u8;
    Color::Rgb(
        interpolate_color(red, neutral, CONSOLE_ACCENT_DESATURATION),
        interpolate_color(green, neutral, CONSOLE_ACCENT_DESATURATION),
        interpolate_color(blue, neutral, CONSOLE_ACCENT_DESATURATION),
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
const BUSY_INDICATOR_TRACK_LENGTH: usize = 5;
const BUSY_INDICATOR_TAIL_LENGTH: usize = 2;
const BUSY_INDICATOR_WIDTH: usize = BUSY_INDICATOR_TRACK_LENGTH + BUSY_INDICATOR_TAIL_LENGTH;
const BUSY_INDICATOR_BLOCK: char = '■';
const BUSY_INDICATOR_TAIL_OPACITY: [f32; BUSY_INDICATOR_TAIL_LENGTH] = [0.55, 0.25];
const BUSY_INDICATOR_PERIOD_TICKS: u128 = (BUSY_INDICATOR_TRACK_LENGTH as u128 - 1) * 2;
// 62.5ms per cell makes the busy indicator move at 80% of its former speed.
const BUSY_INDICATOR_TICK: Duration = Duration::from_micros(62_500);
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

fn busy_indicator_position_at(elapsed: Duration) -> (usize, bool) {
    let tick = elapsed.as_micros() / BUSY_INDICATOR_TICK.as_micros();
    let phase = tick % BUSY_INDICATOR_PERIOD_TICKS;
    if phase < BUSY_INDICATOR_TRACK_LENGTH as u128 {
        (
            phase as usize,
            phase < BUSY_INDICATOR_TRACK_LENGTH as u128 - 1,
        )
    } else {
        ((BUSY_INDICATOR_PERIOD_TICKS - phase) as usize, false)
    }
}

fn busy_indicator_frame_at(elapsed: Duration) -> String {
    let (head, moving_right) = busy_indicator_position_at(elapsed);
    let mut frame = vec![' '; BUSY_INDICATOR_WIDTH];
    frame[head] = BUSY_INDICATOR_BLOCK;
    for distance in 1..=BUSY_INDICATOR_TAIL_LENGTH {
        let tail = if moving_right {
            head.checked_sub(distance)
        } else {
            head.checked_add(distance)
        };
        if let Some(tail) = tail.filter(|&index| index < BUSY_INDICATOR_WIDTH) {
            frame[tail] = BUSY_INDICATOR_BLOCK;
        }
    }
    frame.into_iter().collect()
}

/// Terminals do not support alpha in a cell foreground, so fade the tail by
/// blending the accent toward the console background color.
fn busy_indicator_color(accent: Color, distance: Option<usize>) -> Color {
    let Some(distance) = distance else {
        return accent;
    };
    let Color::Rgb(red, green, blue) = accent else {
        return accent;
    };
    let opacity = BUSY_INDICATOR_TAIL_OPACITY
        .get(distance.saturating_sub(1))
        .copied()
        .unwrap_or(0.0);
    Color::Rgb(
        interpolate_color(BUSY_INDICATOR_FADE_BASE_RGB.0, red, opacity),
        interpolate_color(BUSY_INDICATOR_FADE_BASE_RGB.1, green, opacity),
        interpolate_color(BUSY_INDICATOR_FADE_BASE_RGB.2, blue, opacity),
    )
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.busy = true;
        state.active_cancel = Some(CancellationToken::new());
        let mut writer = FailingWriter;

        release_finished_turn(&mut writer, &mut state);

        assert!(!state.busy);
        assert!(state.active_cancel.is_none());
    }

    #[test]
    fn an_idle_finish_does_not_emit_a_duplicate_notification() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let mut output = Vec::new();

        release_finished_turn(&mut output, &mut state);

        assert!(output.is_empty());
    }

    #[test]
    fn context_status_shows_used_window_and_percentage_in_uniform_gray() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_context(Some(100_000), 80_000);

        assert_eq!(
            context_status_text(&state),
            "Context: 80.0K/100.0K (80%) ████████░░"
        );
        assert_eq!(
            context_status_style(&state).fg,
            Some(Color::Rgb(144, 144, 148))
        );

        state.context_tokens = 80_001;
        assert_eq!(
            context_status_text(&state),
            "Context: 80.0K/100.0K (81%) █████████░"
        );
        assert_eq!(
            context_status_style(&state).fg,
            Some(Color::Rgb(144, 144, 148)),
            "crossing the compaction threshold does not recolor the status line"
        );
    }

    #[test]
    fn context_status_keeps_percentage_consistent_at_capacity() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_context(Some(100_000), 99_001);

        assert_eq!(
            context_status_text(&state),
            "Context: 99.0K/100.0K (100%) ██████████"
        );

        state.context_tokens = 100_000;
        assert_eq!(
            context_status_text(&state),
            "Context: 100.0K/100.0K (100%) ██████████"
        );

        state.context_tokens = 100_001;
        assert_eq!(
            context_status_text(&state),
            "Context: 100.0K/100.0K (101%) ██████████"
        );
    }

    #[test]
    fn context_status_handles_unknown_window_without_highlighting() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);

        assert_eq!(context_status_text(&state), "Context: 1/? (?%) ??????????");
        assert_eq!(
            context_status_style(&state).fg,
            Some(Color::Rgb(144, 144, 148))
        );
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
    fn tui_viewport_caps_at_one_hundred_columns_and_centers_it() {
        assert_eq!(
            tui_viewport(Rect::new(0, 0, 140, 10)),
            Rect::new(20, 0, TUI_MAX_WIDTH, 10)
        );
        assert_eq!(
            tui_viewport(Rect::new(0, 0, 103, 10)),
            Rect::new(1, 0, TUI_MAX_WIDTH, 10),
            "an odd remaining column stays on the right"
        );
    }

    #[test]
    fn bottom_console_has_external_margins_without_losing_internal_padding() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let viewport = tui_viewport(Rect::new(0, 0, 80, 14));
        let (chat, _, _, _, console, _) = ui_layout(&state, viewport);
        let content = console_content_area(console);

        assert_eq!(chat.x, console.x);
        assert_eq!(chat.width, console.width);
        assert_eq!(
            console,
            Rect::new(viewport.x + 7, 8, viewport.width - 14, 5)
        );
        assert_eq!(console.y + console.height, viewport.y + viewport.height - 1);
        assert_eq!(content.x, console.x + 2);
        assert_eq!(content.width, console.width - 4);
        assert_eq!(content.y, console.y + 1);
        assert_eq!(content.y + content.height, console.y + console.height - 1);

        for (width, margin, console_width) in [
            (1, 0, 1),
            (2, 0, 2),
            (3, 1, 1),
            (4, 1, 2),
            (5, 2, 1),
            (15, 0, 15),
        ] {
            let console = bottom_console_area(Rect::new(0, 0, width, 4), 0, 4);
            assert_eq!(console.x, margin, "width {width}");
            assert_eq!(console.width, console_width, "width {width}");
        }
    }

    #[test]
    fn inset_console_width_drives_prompt_rows_and_vertical_navigation() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.input = "x".repeat(71);
        let viewport = tui_viewport(Rect::new(0, 0, 80, 14));
        let console = ui_layout(&state, viewport).4;
        let prompt = prompt_area(console, &state);

        assert_eq!(ui_prompt_content_width(viewport), prompt.width);
        assert_eq!(prompt.width, 60);
        assert_eq!(input_visible_rows(&state, prompt.width), 2);
        assert!(move_input_cursor_vertical(
            &mut state,
            ui_prompt_content_width(viewport) as usize,
            true,
        ));
    }

    #[test]
    fn context_status_is_right_aligned_in_uniform_gray() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false)
            .with_context(Some(100), 81);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 10)).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw statusline");

        let buffer = terminal.backend().buffer();
        let status_area = ui_layout(&state, tui_viewport(Rect::new(0, 0, 80, 10))).5;
        let expected_context = "Context: 81/100 (81%) █████████░";
        let rendered = (status_area.x..status_area.x + status_area.width)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert!(rendered.ends_with(expected_context));
        assert_eq!(
            buffer[(status_area.x + status_area.width - 1, status_area.y)].symbol(),
            "░",
            "context is not pushed to the right edge"
        );
        assert!(rendered.starts_with("model · default"));
        for x in status_area.x..status_area.x + status_area.width {
            if buffer[(x, status_area.y)].symbol() != " " {
                assert_eq!(buffer[(x, status_area.y)].fg, CONSOLE_STATUS_COLOR);
            }
        }
    }

    #[test]
    fn busy_model_name_and_indicator_share_the_animated_accent_gradient() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.busy = true;
        let start = model_status_line_at(&state, "default", Duration::ZERO, 80);
        let middle = model_status_line_at(&state, "default", console_accent_cycle() / 2, 80);
        let start_accent = console_accent_at(Duration::ZERO);
        let middle_accent = console_accent_at(console_accent_cycle() / 2);

        assert_eq!(start.spans[0].style.fg, Some(start_accent));
        assert_eq!(middle.spans[0].style.fg, Some(middle_accent));
        assert_eq!(start.spans[0].content, "model");
        assert_eq!(start.spans[1].content, " · default");
        assert_eq!(start.spans[2].content, " ");
        assert_eq!(start.spans[3].content, BUSY_INDICATOR_BLOCK.to_string());
        assert_eq!(start.spans[3].style.fg, Some(start_accent));
        assert_eq!(
            start.spans.last().unwrap().style.fg,
            Some(CONSOLE_STATUS_COLOR)
        );
    }

    #[test]
    fn idle_model_status_has_no_busy_indicator() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let start = model_status_line_at(&state, "default", Duration::ZERO, 80);
        let middle = model_status_line_at(&state, "default", console_accent_cycle() / 2, 80);

        assert_eq!(start.spans[0].content, "model");
        assert_eq!(middle.spans[0].content, "model");
        assert_eq!(start.spans[0].style.fg, Some(CONSOLE_STATUS_COLOR));
        assert_eq!(middle.spans[0].style.fg, Some(CONSOLE_STATUS_COLOR));
    }

    #[test]
    fn busy_indicator_is_a_five_cell_bounce_with_a_two_cell_tail() {
        let frames = (0..=BUSY_INDICATOR_PERIOD_TICKS)
            .map(|tick| busy_indicator_frame_at(BUSY_INDICATOR_TICK * tick as u32))
            .collect::<Vec<_>>();

        assert_eq!(frames[0], "■      ");
        assert_eq!(frames[1], "■■     ");
        assert_eq!(frames[2], "■■■    ");
        assert_eq!(frames[4], "    ■■■");
        assert_eq!(frames[5], "   ■■■ ");
        assert_eq!(frames[7], " ■■■   ");
        assert_eq!(frames[8], frames[0]);
        assert!(frames
            .iter()
            .all(|frame| frame.chars().count() == BUSY_INDICATOR_WIDTH));
        assert_eq!(BUSY_INDICATOR_TRACK_LENGTH, 5);
        assert_eq!(BUSY_INDICATOR_TAIL_LENGTH, 2);
        assert_eq!(BUSY_INDICATOR_TICK, Duration::from_micros(62_500));
    }

    fn color_distance_from_indicator_base(color: Color) -> u32 {
        let Color::Rgb(red, green, blue) = color else {
            return 0;
        };
        u32::from(red.abs_diff(BUSY_INDICATOR_FADE_BASE_RGB.0))
            + u32::from(green.abs_diff(BUSY_INDICATOR_FADE_BASE_RGB.1))
            + u32::from(blue.abs_diff(BUSY_INDICATOR_FADE_BASE_RGB.2))
    }

    #[test]
    fn busy_indicator_tail_uses_same_block_with_progressively_fainter_colors() {
        let accent = Color::Rgb(180, 120, 240);
        let near = busy_indicator_color(accent, Some(1));
        let far = busy_indicator_color(accent, Some(2));

        assert_eq!(BUSY_INDICATOR_BLOCK, '■');
        assert_ne!(near, accent);
        assert_ne!(far, near);
        assert!(color_distance_from_indicator_base(near) > color_distance_from_indicator_base(far));
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
    fn console_animation_clock_runs_during_entry_and_survives_active_status_changes() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.set_status("working");
        let epoch = state.console_animation_epoch;
        assert_eq!(
            state.console_animation_elapsed_at(epoch + Duration::from_millis(200)),
            Duration::from_millis(200),
            "the console animation does not freeze during the activity ramp"
        );

        state.set_status("compacting");
        assert_eq!(state.console_animation_epoch, epoch);
        state.set_status("working");
        assert_eq!(state.console_animation_epoch, epoch);
    }

    #[test]
    fn console_accent_uses_a_fifteen_second_lavender_to_teal_round_trip() {
        assert_eq!(console_accent_cycle(), Duration::from_secs(15));
        assert_eq!(
            console_accent_at(Duration::ZERO),
            desaturate_console_accent(
                CONSOLE_ACCENT_LAVENDER.0,
                CONSOLE_ACCENT_LAVENDER.1,
                CONSOLE_ACCENT_LAVENDER.2,
            )
        );
        assert_eq!(
            console_accent_at(console_accent_cycle() / 2),
            desaturate_console_accent(
                CONSOLE_ACCENT_TEAL.0,
                CONSOLE_ACCENT_TEAL.1,
                CONSOLE_ACCENT_TEAL.2,
            )
        );
        assert_eq!(
            console_accent_at(console_accent_cycle()),
            console_accent_at(Duration::ZERO)
        );
        let midpoint = console_accent_at(console_accent_cycle() / 4);
        assert_ne!(
            midpoint,
            console_accent_at(Duration::ZERO),
            "the accent transitions continuously instead of holding at lavender"
        );
        assert_ne!(
            midpoint,
            console_accent_at(console_accent_cycle() / 2),
            "the accent transitions continuously instead of holding at teal"
        );
    }

    #[test]
    fn model_status_accent_starts_lavender_with_fifteen_percent_desaturation() {
        assert_eq!(
            console_accent_at(Duration::ZERO),
            desaturate_console_accent(
                CONSOLE_ACCENT_LAVENDER.0,
                CONSOLE_ACCENT_LAVENDER.1,
                CONSOLE_ACCENT_LAVENDER.2,
            )
        );
    }

    #[test]
    fn prompt_uses_two_cells_of_horizontal_console_padding() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.input = "1234567890123456".to_owned();
        let area = Rect::new(0, 0, 20, 6);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw padded prompt");

        let input_area = ui_layout(&state, tui_viewport(area)).4;
        let prompt = prompt_area(input_area, &state);
        assert_eq!(prompt.x, input_area.x + 2);
        assert_eq!(prompt.width, input_area.width.saturating_sub(4));
        assert_eq!(
            terminal.backend().buffer()[(input_area.x + 1, prompt.y)].symbol(),
            " ",
            "the two left padding cells remain blank"
        );
        assert_eq!(
            terminal.backend().buffer()[(input_area.x + input_area.width - 2, prompt.y)].symbol(),
            " ",
            "the two right padding cells remain blank"
        );
        terminal
            .backend_mut()
            .assert_cursor_position((input_area.x + 2, prompt.y));
    }

    #[test]
    fn prompt_width_reduction_wraps_and_saturates_at_narrow_widths() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.input = "12345".to_owned();
        state.cursor = state.input.chars().count();
        let input_area = Rect::new(3, 2, 6, 6);
        let prompt = prompt_area(input_area, &state);

        assert_eq!(prompt.width, 2);
        assert_eq!(input_visible_rows(&state, prompt.width), 3);
        assert_eq!(bottom_content_heights(&state, input_area).prompt, 3);
        assert_eq!(
            cursor_row(&state.input, state.cursor, prompt.width as usize),
            2
        );
        state.cursor = 1;
        assert!(move_input_cursor_vertical(
            &mut state,
            prompt_content_width(input_area.width) as usize,
            true,
        ));
        assert_eq!(state.cursor, 3);
        assert_eq!(prompt_content_width(0), 0);
        assert_eq!(prompt_content_width(1), 0);
        assert_eq!(prompt_content_width(2), 0);
        assert_eq!(prompt_content_width(3), 0);
        assert_eq!(prompt_content_width(4), 0);
        assert_eq!(prompt_content_width(5), 1);
    }

    #[test]
    fn ready_submission_bypasses_queue_and_is_not_added_twice_when_started() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);

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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_skill_names(vec!["release-notes".to_owned()]);
        state.queue_user("next task");
        state.input = "/".to_owned();
        state.input_changed();

        let area = Rect::new(0, 0, 80, 12);
        let (_, picker_area, _, queue_area, input_area, _) = ui_layout(&state, tui_viewport(area));
        let picker_area = picker_area.expect("skill picker area");
        let queue_area = queue_area.expect("message queue area");
        assert_eq!(picker_area.y + picker_area.height, input_area.y);
        assert_eq!(queue_area.y, input_area.y + 1);
        assert_eq!(queue_area.x, input_area.x + 2);
    }

    #[test]
    fn fresh_sessions_show_the_versioned_gradient_welcome_message() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
    fn welcome_image_brightness_is_reduced_without_changing_alpha() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([200, 100, 0, 37]),
        ));
        let dimmed = dim_welcome_image(image).to_rgba8();
        assert_eq!(dimmed.get_pixel(0, 0).0, [170, 85, 0, 37]);
    }

    #[test]
    fn spacious_welcome_uses_the_embedded_png() {
        let image = welcome_image(GREETING_IMAGE_SIZE);
        assert_eq!(image.size(), GREETING_IMAGE_SIZE);
        let layout = welcome_image_layout(Rect::new(0, 0, 100, 40), 6).expect("image fits");
        assert_eq!(layout.image_size, GREETING_IMAGE_SIZE);
        assert_eq!(layout.image_area, Rect::new(10, 6, 80, 20));
        assert_eq!(layout.intro_area.y, layout.image_area.y + 21);
    }

    #[test]
    fn cramped_welcome_falls_back_to_the_text_greeting() {
        assert_eq!(welcome_image_layout(Rect::new(0, 0, 80, 16), 6), None);
        assert_eq!(welcome_image_layout(Rect::new(0, 0, 39, 40), 6), None);
        let scaled = welcome_image_layout(Rect::new(0, 0, 60, 25), 6).expect("scaled image fits");
        assert_eq!(scaled.image_size, Size::new(60, 15));

        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let area = Rect::new(0, 0, 80, 12);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw text fallback");
        let chat_area = ui_layout(&state, tui_viewport(area)).0;
        let rows = (chat_area.y..chat_area.y + chat_area.height)
            .map(|y| {
                (chat_area.x..chat_area.x + chat_area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows.iter().any(|row| row.contains(WELCOME_MESSAGE)));
        assert!(!rows
            .iter()
            .any(|row| row.contains('▀') || row.contains('▄')));
    }

    #[test]
    fn logo_text_renders_by_default_and_greeting_image_replaces_it_when_enabled() {
        let logo = logo_lines();
        let logo_row_count = LOGO_TEXT.lines().count();
        assert_eq!(logo.len(), logo_row_count);
        // Every non-space character should carry a gradient color.
        assert!(logo.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.chars().any(|ch| ch != ' ')
                && matches!(span.style.fg, Some(Color::Rgb(..)))
        }));

        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let area = Rect::new(0, 0, 100, 50);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        let chat_area = ui_layout(&state, tui_viewport(area)).0;
        let intro_lines = welcome_lines(&state.attached_agents);
        let greeting_layout =
            welcome_image_layout(chat_area, intro_lines.len() as u16).expect("greeting fits");

        // Without the flag the logo text renders (no halfblock image cells).
        std::env::remove_var("LUCY_GREETING_IMAGE");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw logo text");
        let buffer = terminal.backend().buffer();
        let rows = (chat_area.y..chat_area.y + chat_area.height)
            .map(|y| {
                (chat_area.x..chat_area.x + chat_area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows
            .iter()
            .any(|row| row.contains(':') || row.contains('-') || row.contains('=')));
        assert!(!rows
            .iter()
            .any(|row| row.contains('▀') || row.contains('▄')));
        assert!(rows.iter().any(|row| row.contains(WELCOME_MESSAGE)));

        // With the flag set the greeting image renders instead of the logo.
        std::env::set_var("LUCY_GREETING_IMAGE", "true");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw greeting");
        let buffer = terminal.backend().buffer();
        assert_eq!(greeting_layout.image_size, GREETING_IMAGE_SIZE);
        assert!(matches!(
            buffer[(greeting_layout.image_area.x, greeting_layout.image_area.y)].symbol(),
            "▀" | "▄"
        ));
        assert!(matches!(
            buffer[(greeting_layout.image_area.x, greeting_layout.image_area.y)].fg,
            Color::Rgb(..)
        ));
        assert!(matches!(
            buffer[(greeting_layout.image_area.x, greeting_layout.image_area.y)].bg,
            Color::Rgb(..)
        ));
        let intro_rows = (greeting_layout.intro_area.y
            ..greeting_layout.intro_area.y + greeting_layout.intro_area.height)
            .map(|y| {
                (greeting_layout.intro_area.x
                    ..greeting_layout.intro_area.x + greeting_layout.intro_area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(intro_rows.iter().any(|row| row.contains(WELCOME_MESSAGE)));

        std::env::remove_var("LUCY_GREETING_IMAGE");
    }

    #[test]
    fn welcome_renders_version_below_title_with_a_blank_line_before_tagline() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let area = Rect::new(0, 0, 80, 12);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw welcome screen");

        let chat_area = ui_layout(&state, tui_viewport(area)).0;
        let buffer = terminal.backend().buffer();
        let rows = (chat_area.y..chat_area.y + chat_area.height)
            .map(|y| {
                (chat_area.x..chat_area.x + chat_area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let title_row = rows
            .iter()
            .position(|row| row.contains(WELCOME_MESSAGE))
            .expect("rendered welcome title");
        let version_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(row, rendered)| rendered.contains(WELCOME_VERSION).then_some(row))
            .collect::<Vec<_>>();

        assert_eq!(version_rows, vec![title_row + 1]);
        assert!(rows[title_row + 2].trim().is_empty());
        assert!(rows[title_row + 3].contains(WELCOME_TAGLINE));

        let version_width = WELCOME_VERSION.chars().count() as u16;
        let version_x = chat_area.x
            + rows[version_rows[0]]
                .find(WELCOME_VERSION)
                .expect("rendered welcome version") as u16;
        let version_y = chat_area.y + title_row as u16 + 1;
        assert!((version_x..version_x + version_width)
            .all(|x| buffer[(x, version_y)].fg == Color::DarkGray));
    }

    #[test]
    fn welcome_shows_the_tagline_and_attached_agents_paths() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false)
            .with_attached_agents(vec![
                "/workspace/AGENTS.md".to_owned(),
                "/workspace/app/AGENTS.md".to_owned(),
            ]);
        let lines = welcome_lines(&state.attached_agents);

        assert_eq!(lines[1].to_string(), WELCOME_VERSION);
        assert_eq!(lines[1].style.fg, Some(Color::DarkGray));
        assert!(lines[2].to_string().is_empty());
        assert_eq!(lines[3].to_string(), WELCOME_TAGLINE);
        assert_eq!(lines[3].style.fg, Some(Color::DarkGray));
        assert!(lines[4].to_string().is_empty());
        assert_eq!(lines[5].to_string(), "Attached AGENTS.md:");
        assert_eq!(lines[6].to_string(), "• /workspace/AGENTS.md");
        assert_eq!(lines[7].to_string(), "• /workspace/app/AGENTS.md");
        assert!(lines[5..]
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
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, true);
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
        let state = UiState::from_history(
            &history,
            "current-session",
            "provider-secret",
            "model",
            None,
            true,
        );
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
        let state = UiState::from_history(
            &history,
            "current-session",
            "provider-secret",
            "model",
            None,
            true,
        );
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
        let state = UiState::from_history(
            &history,
            "current-session",
            "provider-secret",
            "model",
            None,
            true,
        );
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
        let state = UiState::from_history(
            &history,
            "current-session",
            "provider-secret",
            "model",
            None,
            false,
        );
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_skill_names(vec!["release-notes".to_owned()]);
        state.add_user("/release-notes v1.2.0", "secret");
        state.mark_latest_user_skill_attached();

        let lines = transcript_lines(&state, 40);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].spans[1].content, " ");
        let cyan_text = lines[1]
            .spans
            .iter()
            .filter(|span| span.style.fg == Some(SKILL_TRIGGER_COLOR))
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
        let state = UiState::from_history(
            &history,
            "current-session",
            "provider-secret",
            "model",
            None,
            false,
        );
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
        let mut state = UiState::from_history(
            &history,
            "current-session",
            "provider-secret",
            "model",
            None,
            false,
        );
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
    fn transcript_scrollbar_appears_only_when_the_stream_is_scrolled() {
        let mut state = UiState::from_history(
            &[],
            "current-session",
            "provider-secret",
            "model",
            None,
            false,
        );
        state.welcome_visible = false;
        let area = Rect::new(0, 0, 80, 14);
        let chat_area = ui_layout(&state, tui_viewport(area)).0;
        state.transcript = (0..40)
            .map(|_| TranscriptItem::Info(format!("{}#", "x".repeat(chat_area.width as usize - 1))))
            .collect();
        state.auto_scroll = false;
        state.scroll = 3;

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw scrolled transcript");

        let message_edge_x = chat_area.x + chat_area.width - 1;
        let scrollbar_x = chat_area.x + chat_area.width;
        let buffer = terminal.backend().buffer();
        assert!(
            (chat_area.y..chat_area.y + chat_area.height)
                .any(|y| buffer[(message_edge_x, y)].symbol() == "#"),
            "the scrollbar must not overwrite transcript content at the right edge"
        );
        assert!(
            (chat_area.y..chat_area.y + chat_area.height).any(|y| {
                buffer[(scrollbar_x, y)].symbol() == TRANSCRIPT_SCROLLBAR_THUMB
                    && buffer[(scrollbar_x, y)].fg == CONSOLE_STATUS_COLOR
            }),
            "a scrolled transcript should show a scrollbar thumb"
        );
        assert!(
            (chat_area.y..chat_area.y + chat_area.height)
                .any(|y| { buffer[(scrollbar_x, y)].symbol() == TRANSCRIPT_SCROLLBAR_TRACK }),
            "a scrolled transcript should show a scrollbar track"
        );

        state.auto_scroll = true;
        state.scroll = 0;
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw following transcript");
        let buffer = terminal.backend().buffer();
        assert!((chat_area.y..chat_area.y + chat_area.height)
            .all(|y| buffer[(scrollbar_x, y)].symbol() != TRANSCRIPT_SCROLLBAR_THUMB));
    }

    #[test]
    fn tool_result_sweep_is_now_twice_as_fast() {
        assert_eq!(TOOL_RESULT_SWEEP_DURATION, Duration::from_millis(600));
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
        let mut state = UiState::from_history(
            &history,
            "current-session",
            "provider-secret",
            "model",
            None,
            false,
        );
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
        let state =
            UiState::from_history(&history, "current-session", "secret", "model", None, false);
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
        let state =
            UiState::from_history(&history, "current-session", "secret", "model", None, false);
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
        let state =
            UiState::from_history(&history, "current-session", "secret", "model", None, false);
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

        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let spinner = running_tool_status(&state);
        assert_eq!(spinner.chars().count(), 1);
        assert!(spinner
            .chars()
            .all(|spinner| TOOL_SPINNER_FRAMES.contains(&spinner)));
    }

    #[test]
    fn successful_cmd_cross_fades_to_teal_from_first_character_to_last() {
        let started_at = Instant::now();
        let character_count = 12;
        let early = started_at + TOOL_RESULT_SWEEP_DURATION / 4;
        let halfway = started_at + TOOL_RESULT_SWEEP_DURATION / 2;
        let late = started_at + TOOL_RESULT_SWEEP_DURATION * 3 / 4;

        assert_eq!(
            cmd_result_color_at(
                started_at,
                started_at,
                0,
                character_count,
                TOOL_SUCCESS_COLOR,
            ),
            PENDING_TOOL_COLOR,
        );
        assert_eq!(TOOL_SUCCESS_COLOR, Color::Rgb(0, 210, 175));

        let early_first =
            cmd_result_color_at(started_at, early, 0, character_count, TOOL_SUCCESS_COLOR);
        assert_ne!(early_first, PENDING_TOOL_COLOR);
        assert_ne!(early_first, TOOL_SUCCESS_COLOR);
        assert_eq!(
            cmd_result_color_at(started_at, early, 5, character_count, TOOL_SUCCESS_COLOR),
            PENDING_TOOL_COLOR,
            "later characters wait while the first character cross-fades"
        );

        assert_eq!(
            cmd_result_color_at(started_at, halfway, 0, character_count, TOOL_SUCCESS_COLOR),
            TOOL_SUCCESS_COLOR,
        );
        let halfway_middle =
            cmd_result_color_at(started_at, halfway, 5, character_count, TOOL_SUCCESS_COLOR);
        assert_ne!(halfway_middle, PENDING_TOOL_COLOR);
        assert_ne!(halfway_middle, TOOL_SUCCESS_COLOR);
        assert_eq!(
            cmd_result_color_at(
                started_at,
                halfway,
                character_count - 1,
                character_count,
                TOOL_SUCCESS_COLOR,
            ),
            PENDING_TOOL_COLOR,
        );

        let late_last = cmd_result_color_at(
            started_at,
            late,
            character_count - 1,
            character_count,
            TOOL_SUCCESS_COLOR,
        );
        assert_ne!(late_last, PENDING_TOOL_COLOR);
        assert_ne!(late_last, TOOL_SUCCESS_COLOR);
        assert_eq!(
            cmd_result_color_at(
                started_at,
                started_at + TOOL_RESULT_SWEEP_DURATION,
                character_count - 1,
                character_count,
                TOOL_SUCCESS_COLOR,
            ),
            TOOL_SUCCESS_COLOR,
            "the completed sweep keeps the exact teal used during the fade"
        );
    }

    #[test]
    fn cmd_result_cross_fade_has_no_abrupt_color_change_between_render_ticks() {
        let started_at = Instant::now();
        let character_count = 12;
        let render_ticks = TOOL_RESULT_SWEEP_DURATION.as_millis() / EVENT_POLL.as_millis();

        for target in [TOOL_SUCCESS_COLOR, TOOL_FAILURE_COLOR, TOOL_WARNING_COLOR] {
            for character_index in 0..character_count {
                let frames = (0..=render_ticks)
                    .map(|tick| {
                        cmd_result_color_at(
                            started_at,
                            started_at + EVENT_POLL * tick as u32,
                            character_index,
                            character_count,
                            target,
                        )
                    })
                    .collect::<Vec<_>>();

                assert!(frames
                    .iter()
                    .any(|color| { *color != PENDING_TOOL_COLOR && *color != target }));
                assert!(frames.windows(2).all(|pair| {
                    let (before_red, before_green, before_blue) = tool_result_color_rgb(pair[0]);
                    let (after_red, after_green, after_blue) = tool_result_color_rgb(pair[1]);
                    before_red.abs_diff(after_red) <= 90
                        && before_green.abs_diff(after_green) <= 90
                        && before_blue.abs_diff(after_blue) <= 90
                }));
                assert_eq!(frames.last(), Some(&target));
            }
        }
    }

    #[test]
    fn only_live_cmd_results_start_a_result_sweep() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let succeeded = serde_json::json!({"exit_code": 0});

        state.add_tool_result("historic", "cmd", succeeded.clone());
        state.add_live_tool_result("success", "cmd", succeeded);
        state.add_live_tool_result("failed", "cmd", serde_json::json!({"exit_code": 1}));

        assert!(!state.cmd_result_started_at.contains_key("historic"));
        assert!(state.cmd_result_started_at.contains_key("success"));
        assert!(state.cmd_result_started_at.contains_key("failed"));
    }

    #[test]
    fn failed_cmd_cross_fades_to_the_same_rgb_red_without_a_final_jump() {
        let started_at = Instant::now();
        let character_count = 12;
        let halfway = started_at + TOOL_RESULT_SWEEP_DURATION / 2;

        assert_eq!(
            cmd_result_color_at(
                started_at,
                started_at,
                0,
                character_count,
                TOOL_FAILURE_COLOR,
            ),
            PENDING_TOOL_COLOR,
        );
        assert_eq!(
            cmd_result_color_at(started_at, halfway, 0, character_count, TOOL_FAILURE_COLOR),
            TOOL_FAILURE_COLOR,
        );
        let intermediate =
            cmd_result_color_at(started_at, halfway, 5, character_count, TOOL_FAILURE_COLOR);
        assert_ne!(intermediate, PENDING_TOOL_COLOR);
        assert_ne!(intermediate, TOOL_FAILURE_COLOR);
        assert_eq!(
            cmd_result_color_at(
                started_at,
                halfway,
                character_count - 1,
                character_count,
                TOOL_FAILURE_COLOR,
            ),
            PENDING_TOOL_COLOR,
        );
        assert_eq!(
            cmd_result_color_at(
                started_at,
                started_at + TOOL_RESULT_SWEEP_DURATION,
                character_count - 1,
                character_count,
                TOOL_FAILURE_COLOR,
            ),
            TOOL_FAILURE_COLOR,
            "the completed failure sweep keeps the exact RGB red used during the fade"
        );
    }

    #[test]
    fn live_failed_cmd_sweep_keeps_the_final_status_text() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
            TOOL_SUCCESS_COLOR
        );
        assert_eq!(
            cmd_result_target_color(&serde_json::json!({"exit_code": 1})),
            TOOL_FAILURE_COLOR
        );
        assert_eq!(
            cmd_result_target_color(&serde_json::json!({"timed_out": true})),
            TOOL_WARNING_COLOR
        );
    }

    #[test]
    fn background_cmd_registration_shows_its_running_id() {
        let (icon, status, _) = cmd_result_status(&serde_json::json!({
            "background_id": "background-1",
            "status": "running"
        }));
        assert_eq!(icon, '↗');
        assert_eq!(status, "background-1");
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
            let state =
                UiState::from_history(&history, "current-session", "secret", "model", None, false);
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
        let state =
            UiState::from_history(&history, "current-session", "secret", "model", None, false);
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
        let state =
            UiState::from_history(&history, "current-session", "secret", "model", None, false);
        let lines = transcript_lines(&state, 200);
        assert_eq!(lines[0].to_string(), "✓ cmd  $ first");
        assert_eq!(lines[2].to_string(), "✓ cmd  $ second");
    }

    #[test]
    fn cmd_status_styles_use_success_failure_and_pending_colors() {
        assert_eq!(
            cmd_result_status(&serde_json::json!({"exit_code": 0})).2.fg,
            Some(TOOL_SUCCESS_COLOR)
        );
        assert_eq!(
            cmd_result_status(&serde_json::json!({"exit_code": 1})).2.fg,
            Some(TOOL_FAILURE_COLOR)
        );
        assert_eq!(
            cmd_tool_segments(
                "call-1",
                "{\"command\":\"pwd\"}",
                None,
                &UiState::from_history(&[], "current-session", "secret", "model", None, false)
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
        assert_eq!(SKILL_TRIGGER_COLOR, Color::Rgb(80, 255, 245));

        let lines = styled_text_lines(
            "/release-notes v1.2.0",
            trigger,
            80,
            Style::default().fg(Color::White),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "/release-notes v1.2.0");
        assert_eq!(lines[0].spans[0].content, "/release-notes");
        assert_eq!(lines[0].spans[0].style.fg, Some(SKILL_TRIGGER_COLOR));
        assert_eq!(lines[0].spans[1].content, " v1.2.0");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::White));
    }

    #[test]
    fn draw_renders_an_active_skill_trigger_in_cyan() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_skill_names(vec!["release-notes".to_owned()]);
        state.input = "/release-notes v1.2.0".to_owned();
        state.cursor = state.input.chars().count();

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(40, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw input");

        // The full-width input block keeps trigger characters bright cyan while the
        // argument that follows stays white.
        let buffer = terminal.backend().buffer();
        let (_, _, _, _, input_area, _) = ui_layout(&state, tui_viewport(Rect::new(0, 0, 40, 10)));
        let prompt_area = prompt_area(input_area, &state);
        let input_x = prompt_area.x;
        let input_y = prompt_area.y;
        assert_eq!(buffer[(input_x, input_y)].fg, SKILL_TRIGGER_COLOR);
        assert_eq!(
            buffer[(input_x + "/release-notes".chars().count() as u16, input_y)].fg,
            Color::White
        );
    }

    #[test]
    fn main_agent_status_omits_activity_animation_on_idle_and_busy_states() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_context(Some(100), 81);
        let area = Rect::new(0, 0, 80, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw ready status");
        let viewport = tui_viewport(area);
        let status_area = ui_layout(&state, viewport).5;
        let expected_context = "Context: 81/100 (81%) █████████░";
        let buffer = terminal.backend().buffer();
        let idle_row = (status_area.x..status_area.x + status_area.width)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert!(idle_row.starts_with("model · default"));
        assert!(idle_row.ends_with(expected_context));
        for x in status_area.x..status_area.x + status_area.width {
            if buffer[(x, status_area.y)].symbol() != " " {
                assert_eq!(buffer[(x, status_area.y)].fg, Color::Rgb(144, 144, 148));
            }
        }

        state.set_status("working");
        state.busy = true;
        state.activity_transition = None;
        state.console_animation_epoch = Instant::now() - console_accent_cycle() / 4;
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw working status");
        let status_area = ui_layout(&state, viewport).5;
        let buffer = terminal.backend().buffer();
        let rendered = (status_area.x..status_area.x + status_area.width)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert!(rendered.starts_with("model · default "));
        assert!(rendered.contains(BUSY_INDICATOR_BLOCK));
        assert!(rendered.ends_with(expected_context));
    }

    #[test]
    fn terminal_focus_events_control_cursor_visibility() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);

        assert!(handle_terminal_focus_event(&mut state, &Event::FocusLost));
        assert!(!state.terminal_focused);
        assert!(handle_terminal_focus_event(&mut state, &Event::FocusGained));
        assert!(state.terminal_focused);
        assert!(!handle_terminal_focus_event(
            &mut state,
            &Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        ));
        assert!(state.terminal_focused);
    }

    #[test]
    fn unfocused_busy_redraw_keeps_the_hardware_cursor_hidden() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.set_status("working");
        state.set_busy(true);
        state.terminal_focused = false;

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw busy state");

        assert!(
            !terminal.backend().cursor_visible(),
            "a busy redraw must not re-show the terminal cursor"
        );
    }

    #[test]
    fn cjk_input_keeps_the_terminal_cursor_in_the_prompt_without_resetting_activity() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
        let prompt_area = prompt_area(input_area, &state);
        assert!(terminal.backend().cursor_visible());
        terminal.backend_mut().assert_cursor_position((
            prompt_area.x + UnicodeWidthStr::width(state.input.as_str()) as u16,
            prompt_area.y,
        ));
    }

    #[test]
    fn transcript_and_console_are_separated_by_one_blank_row() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let area = Rect::new(0, 0, 80, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw separated transcript and console");

        let (transcript, _, _, _, console, _) = ui_layout(&state, tui_viewport(area));
        assert_eq!(transcript.y + transcript.height + 1, console.y);
        let gap_y = console.y - 1;
        for x in transcript.x..transcript.x + transcript.width {
            assert_eq!(terminal.backend().buffer()[(x, gap_y)].symbol(), " ");
            assert_eq!(terminal.backend().buffer()[(x, gap_y)].bg, Color::Reset);
        }
    }

    #[test]
    fn prompt_surface_has_a_subtle_dark_background_when_idle_or_busy() {
        for busy in [false, true] {
            let mut state =
                UiState::from_history(&[], "current-session", "secret", "model", None, false);
            state.input = "prompt".to_owned();
            state.cursor = state.input.chars().count();
            state.busy = busy;
            let area = Rect::new(0, 0, 80, 10);
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                    .expect("test terminal");

            terminal
                .draw(|frame| draw(frame, &state))
                .expect("draw prompt surface");

            let (_, _, _, _, input_area, _) = ui_layout(&state, tui_viewport(area));
            let buffer = terminal.backend().buffer();
            for x in 0..area.width {
                assert_eq!(buffer[(x, input_area.y - 1)].bg, Color::Reset);
            }
            for y in input_area.y..input_area.y + input_area.height {
                for x in 0..area.width {
                    let expected = if input_area.contains((x, y).into()) {
                        PROMPT_BACKGROUND
                    } else {
                        Color::Reset
                    };
                    assert_eq!(
                        buffer[(x, y)].bg,
                        expected,
                        "busy={busy}: unexpected background at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn background_indicator_fills_the_row_below_the_prompt_surface() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.background_active_count.store(2, Ordering::Relaxed);
        let area = Rect::new(0, 0, 80, 10);
        let viewport = tui_viewport(area);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw background indicator");

        let (_, _, _, _, input_area, _) = ui_layout(&state, viewport);
        let indicator_area =
            background_indicator_area(&state, input_area).expect("visible background indicator");
        assert_eq!(indicator_area.y, input_area.y + input_area.height);
        assert!(indicator_area.y + indicator_area.height <= viewport.y + viewport.height);
        let buffer = terminal.backend().buffer();
        for x in indicator_area.x..indicator_area.x + indicator_area.width {
            assert_eq!(
                buffer[(x, indicator_area.y)].bg,
                BACKGROUND_INDICATOR_BACKGROUND
            );
        }
        let expected = "Background task(s) 2 is running...";
        let rendered = (indicator_area.x..indicator_area.x + indicator_area.width)
            .map(|x| buffer[(x, indicator_area.y)].symbol())
            .collect::<String>();
        assert!(rendered.starts_with(expected));
        for x in indicator_area.x..indicator_area.x + expected.len() as u16 {
            assert_eq!(buffer[(x, indicator_area.y)].fg, BACKGROUND_INDICATOR_COLOR);
        }
    }

    #[test]
    fn background_indicator_is_hidden_when_no_background_tasks_are_active() {
        let state = UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let area = Rect::new(0, 0, 80, 10);
        let viewport = tui_viewport(area);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw without background indicator");

        let (_, _, _, _, input_area, _) = ui_layout(&state, viewport);
        assert_eq!(background_indicator_area(&state, input_area), None);
        let buffer = terminal.backend().buffer();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                assert_ne!(buffer[(x, y)].bg, BACKGROUND_INDICATOR_BACKGROUND);
            }
        }
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
            .filter(|span| span.style.fg == Some(SKILL_TRIGGER_COLOR))
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
        let prompt_area = prompt_area(input_area, &state);
        terminal
            .backend_mut()
            .assert_cursor_position((prompt_area.x, prompt_area.y + 1));
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

        let state =
            UiState::from_history(&history, "current-session", "secret", "model", None, false);
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
    fn clipped_slash_picker_uses_its_actual_item_rows_for_the_focused_item() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_skill_names(
                    ["alpha", "beta", "build", "charlie", "deploy", "doctor"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                );
        state.input = "/".to_owned();
        state.input_changed();
        state.skill_picker_focus = 5;
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(30, 5)).expect("test terminal");
        terminal
            .draw(|frame| draw_skill_picker(frame, &state, Rect::new(0, 0, 30, 5)))
            .expect("draw clipped skill picker");

        let buffer = terminal.backend().buffer();
        let item_rows = (2..4)
            .map(|y| (2..28).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(item_rows[0].starts_with("/deploy"));
        assert!(item_rows[1].starts_with("/doctor"));
        assert_eq!(buffer[(2, 3)].fg, QUEUED_MESSAGE_COLOR);
        assert!(buffer[(2, 3)].modifier.contains(Modifier::BOLD));
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
            vec!["exit", "release-notes", "session", "settings"]
        );
        assert_eq!(
            builtin_command("/settings ignored arguments"),
            Some(BuiltinCommand::Settings)
        );
        assert_eq!(builtin_command("  /exit  "), Some(BuiltinCommand::Exit));
        assert_eq!(builtin_command("/session"), Some(BuiltinCommand::Session));
        assert_eq!(builtin_command("/settings-extra"), None);
    }

    fn session(id: &str, first: Option<&str>, last: Option<&str>) -> SessionMetadata {
        SessionMetadata {
            record_type: "session_metadata",
            session_id: id.to_owned(),
            created_at: 1,
            updated_at: 2,
            first_message: first.map(str::to_owned),
            last_message: last.map(str::to_owned),
        }
    }

    #[test]
    fn session_overlay_filters_ids_and_message_previews_case_insensitively() {
        let sessions = vec![
            session("alpha-id", Some("First request"), Some("Final answer")),
            session("beta-id", Some("Deploy release"), Some("Complete")),
        ];

        assert_eq!(
            filtered_sessions(&sessions, "ALPHA")
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha-id"]
        );
        assert_eq!(
            filtered_sessions(&sessions, "REQUEST")
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha-id"]
        );
        assert_eq!(
            filtered_sessions(&sessions, "complete")
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["beta-id"]
        );
    }

    #[test]
    fn open_sessions_orders_by_updated_at_descending() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        let mut oldest = session("oldest", None, None);
        oldest.updated_at = 10;
        let mut newest = session("newest", None, None);
        newest.updated_at = 30;
        let mut middle = session("middle", None, None);
        middle.updated_at = 20;
        state.sessions = Some(SessionsState::Loading);

        state.open_sessions(Ok(vec![oldest, newest, middle]));

        let SessionsState::Sessions { sessions, .. } =
            state.sessions.as_ref().expect("session picker")
        else {
            panic!("sessions should be loaded");
        };
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newest", "middle", "oldest"]
        );
    }

    #[test]
    fn escape_closes_loaded_session_overlay() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.sessions = Some(SessionsState::Loading);
        state.open_sessions(Ok(vec![session("other-session", None, None)]));

        state.handle_sessions_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(state.sessions.is_none());
    }

    #[test]
    fn escape_during_loading_stays_closed_after_sessions_arrive() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.sessions = Some(SessionsState::Loading);

        state.handle_sessions_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(state.sessions.is_none());

        state.open_sessions(Ok(vec![session("other-session", None, None)]));
        assert!(state.sessions.is_none());
    }

    #[test]
    fn enter_on_active_session_closes_overlay_without_attaching() {
        let mut state =
            UiState::from_history(&[], "active-session", "secret", "model", None, false);
        state.sessions = Some(SessionsState::Loading);
        state.open_sessions(Ok(vec![session("active-session", None, None)]));

        assert_eq!(
            state.handle_sessions_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(state.sessions.is_none());
    }

    #[test]
    fn session_overlay_focus_clamps_and_enter_requests_attach() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.sessions = Some(SessionsState::Loading);
        state.open_sessions(Ok(vec![
            session("older", None, None),
            session("newer", None, None),
        ]));

        state.handle_sessions_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_sessions_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let SessionsState::Sessions { focus, .. } =
            state.sessions.as_ref().expect("session picker")
        else {
            panic!("sessions should be loaded");
        };
        assert_eq!(*focus, 1);

        state.handle_sessions_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        state.handle_sessions_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        let SessionsState::Sessions { focus, .. } =
            state.sessions.as_ref().expect("session picker")
        else {
            panic!("sessions should be loaded");
        };
        assert_eq!(*focus, 0);
        assert_eq!(
            state.handle_sessions_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some("older".to_owned())
        );
    }

    #[test]
    fn empty_session_overlay_has_no_attach_target() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
        state.sessions = Some(SessionsState::Loading);
        state.open_sessions(Ok(Vec::new()));

        assert_eq!(
            state.handle_sessions_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        let SessionsState::Sessions {
            sessions, focus, ..
        } = state.sessions.as_ref().expect("session picker")
        else {
            panic!("sessions should be loaded");
        };
        assert!(sessions.is_empty());
        assert_eq!(*focus, 0);

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("test terminal");
        terminal
            .draw(|frame| draw_sessions(frame, state.sessions.as_ref().unwrap(), frame.area(), ""))
            .expect("draw session picker");
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("No sessions found"));
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
        let mut state = UiState::from_history(
            &[],
            "current-session",
            "secret",
            "old",
            Some("medium"),
            false,
        );
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
        let mut state = UiState::from_history(&[], "current-session", "secret", "old", None, false);
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false);
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_skill_names(command_names(skill_names()));
        state.input = "/set".to_owned();
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
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
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
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
        assert_eq!(selection_range(20, 0, 5), 0..5);
        assert_eq!(selection_range(20, 4, 5), 0..5);
        assert_eq!(selection_range(20, 5, 5), 1..6);
        assert_eq!(selection_range(20, 19, 5), 15..20);
    }

    #[test]
    fn is_inside_tmux_detection() {
        std::env::set_var("TERM_PROGRAM", "tmux");
        assert!(is_inside_tmux());
        std::env::set_var("TERM_PROGRAM", "TMUX");
        assert!(is_inside_tmux());
        std::env::set_var("TERM_PROGRAM", "ghostty");
        assert!(!is_inside_tmux());
        std::env::remove_var("TERM_PROGRAM");
        assert!(!is_inside_tmux());
    }

    #[test]
    fn slash_picker_is_rendered_immediately_above_the_input() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
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
        // The picker shares a boundary with the prompt; no blank row separates them.
        assert_eq!(picker_area.y + picker_area.height, input_area.y);
        for (x, y) in [
            (picker_area.x, picker_area.y),
            (picker_area.x + picker_area.width - 1, picker_area.y),
            (picker_area.x, picker_area.y + picker_area.height - 1),
            (
                picker_area.x + picker_area.width - 1,
                picker_area.y + picker_area.height - 1,
            ),
        ] {
            assert_eq!(buffer[(x, y)].symbol(), " ");
            assert_eq!(buffer[(x, y)].bg, SKILL_PICKER_BACKGROUND);
        }
        assert_eq!(
            buffer[(picker_area.x + 1, picker_area.y + 1)].bg,
            SKILL_PICKER_BACKGROUND
        );
        assert_eq!(buffer[(picker_area.x + 2, picker_area.y + 1)].symbol(), "[");
        assert_eq!(
            buffer[(picker_area.x + 2, picker_area.y + 1)].fg,
            QUEUED_MESSAGE_COLOR
        );
        assert_eq!(buffer[(picker_area.x + 2, picker_area.y + 2)].symbol(), "/");
        assert_eq!(
            buffer[(picker_area.x + 2, picker_area.y + 2)].fg,
            QUEUED_MESSAGE_COLOR
        );
        assert_eq!(
            buffer[(picker_area.x + 1, picker_area.y + picker_area.height - 2)].bg,
            SKILL_PICKER_BACKGROUND
        );
        assert_eq!(buffer[(input_area.x, input_area.y)].symbol(), " ");
        assert_eq!(buffer[(input_area.x, input_area.y)].bg, PROMPT_BACKGROUND);
    }

    #[test]
    fn slash_picker_renders_count_with_bold_focus_on_the_picker_surface() {
        let mut state =
            UiState::from_history(&[], "current-session", "secret", "model", None, false)
                .with_skill_names(skill_names());
        state.input = "/".to_owned();
        state.input_changed();
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(30, 8)).expect("test terminal");
        terminal
            .draw(|frame| draw_skill_picker(frame, &state, Rect::new(0, 0, 30, 8)))
            .expect("draw skill picker");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 0)].bg, SKILL_PICKER_BACKGROUND);
        assert_eq!(buffer[(2, 1)].symbol(), "[");
        assert_eq!(buffer[(2, 1)].fg, QUEUED_MESSAGE_COLOR);
        assert_eq!(buffer[(2, 2)].symbol(), "/");
        assert_eq!(buffer[(2, 2)].fg, QUEUED_MESSAGE_COLOR);
        assert!(buffer[(2, 2)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(2, 3)].symbol(), "/");
        assert_eq!(buffer[(2, 3)].fg, QUEUED_MESSAGE_COLOR);
        assert!(!buffer[(2, 3)].modifier.contains(Modifier::BOLD));
    }
}

#[cfg(test)]
mod tmux_keyboard_tests {
    use super::*;

    #[test]
    fn is_inside_tmux_detection() {
        std::env::set_var("TERM_PROGRAM", "tmux");
        assert!(is_inside_tmux());
        std::env::set_var("TERM_PROGRAM", "TMUX");
        assert!(is_inside_tmux());
        std::env::set_var("TERM_PROGRAM", "ghostty");
        assert!(!is_inside_tmux());
        std::env::remove_var("TERM_PROGRAM");
        assert!(!is_inside_tmux());
    }
}
