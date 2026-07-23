use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
const TUI_GLOW_BACKGROUND_RGB: (u8, u8, u8) = (16, 18, 22);
const TUI_GLOW_BACKGROUND: Color = Color::Rgb(
    TUI_GLOW_BACKGROUND_RGB.0,
    TUI_GLOW_BACKGROUND_RGB.1,
    TUI_GLOW_BACKGROUND_RGB.2,
);
const CONSOLE_BACKGROUND_RGB: (u8, u8, u8) = (42, 42, 46);
const CONSOLE_BACKGROUND: Color = Color::Rgb(
    CONSOLE_BACKGROUND_RGB.0,
    CONSOLE_BACKGROUND_RGB.1,
    CONSOLE_BACKGROUND_RGB.2,
);
const CONSOLE_STATUS_COLOR: Color = Color::Rgb(144, 144, 148);
const CONSOLE_ACCENT_LAVENDER: (u8, u8, u8) = (145, 70, 220);
const CONSOLE_ACCENT_TEAL: (u8, u8, u8) = (0, 180, 180);
const CONSOLE_ACCENT_CYCLE_DURATION: Duration = Duration::from_secs(15);
const CONSOLE_ACCENT_DESATURATION: f32 = 0.15;
const CONSOLE_GLASS_DESATURATION: f32 = 0.65;
const CONSOLE_GLASS_TINT: f32 = 0.24;
const CONSOLE_GLASS_WHITE_TINT: f32 = 0.03;
const CONSOLE_GLASS_GLOW_THROUGH: f32 = 0.26;
const CONSOLE_REFLECTION_TINT: f32 = 0.12;
const CONSOLE_REFLECTION_WHITE_TINT: f32 = 0.20;
const CONSOLE_REFLECTION_GLYPH: &str = "▁";
const GLOW_HEIGHT: u16 = 12;
const GLOW_HORIZONTAL_SPREAD: u16 = 24;
const GLOW_INTENSITY: f32 = 0.70;
const GLOW_DESATURATION: f32 = 0.10;
const CONSOLE_BOUNDARY_CYCLE: Duration = Duration::from_millis(7000);
const CONSOLE_REACH_MIN: f32 = 0.28;
const CONSOLE_REACH_MAX: f32 = 0.38;
const CONSOLE_VISIBILITY_TRANSITION: Duration = Duration::from_millis(600);
const SKILL_TRIGGER_COLOR: Color = Color::Rgb(80, 255, 245);
const PENDING_TOOL_COLOR_RGB: (u8, u8, u8) = (255, 165, 0);
const PENDING_TOOL_COLOR: Color = Color::Rgb(
    PENDING_TOOL_COLOR_RGB.0,
    PENDING_TOOL_COLOR_RGB.1,
    PENDING_TOOL_COLOR_RGB.2,
);
/// A completed `cmd` call first retains its pending orange, then sweeps to the
/// final result colour from the left edge of the compact tool line.
const TOOL_RESULT_SWEEP_DURATION: Duration = Duration::from_millis(1200);
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
const SUBAGENT_TITLE_COLOR: Color = Color::Rgb(165, 35, 135);
const SKILL_PICKER_MAX_ROWS: usize = 5;
const SUBAGENT_TASK_PREVIEW_CHARS: usize = 25;
const SUBAGENT_STREAM_PREVIEW_HEIGHT: u16 = 15;
const SUBAGENT_STREAM_MAX_CHARS: usize = 12 * 1024;
const SUBAGENT_NOTICE_DURATION: Duration = Duration::from_millis(600);
const SUBAGENT_NOTICE_FLASH_INTERVAL: Duration = Duration::from_millis(100);
const BUILTIN_COMMANDS: [&str; 2] = ["settings", "exit"];
const SUBAGENT_OVERLAY_BACKGROUND: Color = FLOATING_PANEL_BACKGROUND;
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
    let subagent_activity_rx = harness.take_subagent_activity_receiver();

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
        &subagent_activity_rx,
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
        if let Err(error) = harness.collect_completed_subagents(&mut sink) {
            let message = redact_secret(&error, Some(&harness.provider.api_key()));
            let _ = sink.emit_event(&ProtocolEvent::Error { message });
        }
        let request = match requests.recv_timeout(EVENT_POLL) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                while let Some(activity) = harness.next_subagent_activity() {
                    let _ = messages.send(WorkerMessage::SubagentActivity(activity));
                }
                if let Err(error) = harness.collect_completed_subagents(&mut sink) {
                    let message = redact_secret(&error, Some(&harness.provider.api_key()));
                    let _ = sink.emit_event(&ProtocolEvent::Error { message });
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
                    let message = redact_secret(&error, Some(&harness.provider.api_key()));
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
    subagent_activities: &Receiver<SubagentActivity>,
) -> Result<(), String> {
    let mut quitting = false;
    loop {
        loop {
            match messages.try_recv() {
                Ok(WorkerMessage::Event(event)) => state.apply_event(event),
                Ok(WorkerMessage::SubagentActivity(activity)) => {
                    state.apply_subagent_activity(activity);
                }
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

        while let Ok(activity) = subagent_activities.try_recv() {
            state.apply_subagent_activity(activity);
        }

        // Ratatui flushes the buffer diff (which issues MoveTo for every
        // changed cell) before it hides or shows the cursor. If the hardware
        // cursor is visible during that flush it briefly appears at each
        // changed cell — most noticeable across the animated glow region in
        // busy state. Hide it first so the flush phase never shows it; Ratatui
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
                    if !move_up_from_input_or_subagent(state, input_width) {
                        let max_scroll = max_scroll_for_area(state, size);
                        scroll_up(state, max_scroll);
                    }
                }
                KeyCode::Down => {
                    if state.subagent_focus.is_some() {
                        let _ = state.move_subagent_focus(true);
                    } else {
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
    User(String),
    Assistant(String),
    Reasoning {
        complete: bool,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentToolActionKind {
    Check,
    Wait,
    Send,
    Cancel,
}

#[derive(Debug, Clone)]
struct SubagentToolAction {
    task_id: String,
    kind: SubagentToolActionKind,
}

#[derive(Debug, Clone, Copy)]
enum SubagentListNotice {
    Flash { started_at: Instant, until: Instant },
    Waiting,
    Cancelling,
}

/// A bounded interpolation between the resting bars and a live pulse frame.
/// Keeping the source frame lets a completed turn settle instead of snapping
/// straight from its last pulse height to the resting indicator.
#[derive(Debug, Clone, Copy)]
struct ConsoleVisibilityTransition {
    started_at: Instant,
    from: f32,
    to: f32,
}

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
    terminal_focused: bool,
    active_cancel: Option<CancellationToken>,
    scroll: u16,
    auto_scroll: bool,
    tool_animation_epoch: Instant,
    console_animation_epoch: Instant,
    console_visibility_transition: Option<ConsoleVisibilityTransition>,
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
    terminal_subagents: HashSet<String>,
    background_result_tasks: HashMap<String, String>,
    pending_subagent_activities: HashMap<String, Vec<SubagentActivity>>,
    subagent_tool_actions: HashMap<String, SubagentToolAction>,
    subagent_list_notices: HashMap<String, SubagentListNotice>,
    cancelling_subagents: HashSet<String>,
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
            terminal_focused: true,
            active_cancel: None,
            scroll: 0,
            auto_scroll: true,
            tool_animation_epoch: Instant::now(),
            console_animation_epoch: Instant::now(),
            console_visibility_transition: None,
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
            terminal_subagents: HashSet::new(),
            background_result_tasks: HashMap::new(),
            pending_subagent_activities: HashMap::new(),
            subagent_tool_actions: HashMap::new(),
            subagent_list_notices: HashMap::new(),
            cancelling_subagents: HashSet::new(),
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

    fn set_busy(&mut self, busy: bool) {
        self.set_busy_at(busy, Instant::now());
    }

    fn set_busy_at(&mut self, busy: bool, now: Instant) {
        if self.busy == busy {
            return;
        }
        let from = self.console_visibility_at(now);
        if busy && from <= f32::EPSILON {
            self.console_animation_epoch = now;
        }
        self.busy = busy;
        self.console_visibility_transition = Some(ConsoleVisibilityTransition {
            started_at: now,
            from,
            to: if busy { 1.0 } else { 0.0 },
        });
    }

    fn console_visibility_at(&self, now: Instant) -> f32 {
        let Some(transition) = self.console_visibility_transition else {
            return if self.busy { 1.0 } else { 0.0 };
        };
        let progress = now
            .saturating_duration_since(transition.started_at)
            .as_secs_f32()
            / CONSOLE_VISIBILITY_TRANSITION.as_secs_f32();
        if progress >= 1.0 {
            return transition.to;
        }
        let progress = progress.clamp(0.0, 1.0);
        let eased = progress * progress * (3.0 - 2.0 * progress);
        transition.from + (transition.to - transition.from) * eased
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
            SessionHistoryRecord::BackgroundResultPending(pending) => {
                self.background_result_tasks
                    .insert(pending.completion_id.clone(), pending.task_id.clone());
                self.complete_subagent(&pending.task_id, pending.result.clone());
                self.transcript.push(TranscriptItem::SubagentLifecycle {
                    completion_id: pending.completion_id.clone(),
                    task_id: pending.task_id.clone(),
                    status: format!("{:?}", pending.status).to_lowercase(),
                    delivered: false,
                });
            }
            SessionHistoryRecord::BackgroundResultDelivered(delivered) => {
                let task_id = self
                    .background_result_tasks
                    .get(&delivered.completion_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_owned());
                self.transcript.push(TranscriptItem::SubagentLifecycle {
                    completion_id: delivered.completion_id.clone(),
                    task_id,
                    status: String::new(),
                    delivered: true,
                });
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

    fn record_tool_call(&mut self, call: &crate::model::ChatToolCall, live: bool) {
        self.clear_thinking();
        if is_subagent_tool(&call.name) {
            if call.name == "spawn_subagent" {
                self.register_subagent_call(&call.id, &call.arguments);
            } else if live {
                self.begin_subagent_tool_action(&call.id, &call.name, &call.arguments);
            }
            return;
        }
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
        if is_subagent_tool(name) {
            if name == "spawn_subagent" {
                self.update_subagent_queued(id, &result);
            } else if animate {
                self.finish_subagent_tool_action(id, &result);
            }
            if subagent_tool_result_is_error(&result) {
                self.transcript.push(TranscriptItem::Error(format!(
                    "subagent {name}: {}",
                    redact_secret(&subagent_tool_error_message(&result), Some(&self.secret))
                )));
            }
            return;
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

    fn begin_subagent_tool_action(&mut self, call_id: &str, name: &str, arguments: &str) {
        let Some(kind) = subagent_tool_action_kind(name) else {
            return;
        };
        let Some(task_id) = subagent_tool_task_id(arguments) else {
            return;
        };
        self.subagent_tool_actions.insert(
            call_id.to_owned(),
            SubagentToolAction {
                task_id: task_id.clone(),
                kind,
            },
        );
        if self.running_subagent_by_id(&task_id).is_none() {
            return;
        }
        if kind == SubagentToolActionKind::Cancel {
            self.cancelling_subagents.insert(task_id.clone());
        }
        let now = Instant::now();
        let notice = match kind {
            SubagentToolActionKind::Check | SubagentToolActionKind::Send => {
                SubagentListNotice::Flash {
                    started_at: now,
                    until: now + SUBAGENT_NOTICE_DURATION,
                }
            }
            SubagentToolActionKind::Wait => SubagentListNotice::Waiting,
            SubagentToolActionKind::Cancel => SubagentListNotice::Cancelling,
        };
        self.subagent_list_notices.insert(task_id, notice);
    }

    fn finish_subagent_tool_action(&mut self, call_id: &str, result: &Value) {
        let Some(action) = self.subagent_tool_actions.remove(call_id) else {
            return;
        };
        if self.running_subagent_by_id(&action.task_id).is_none()
            || subagent_tool_result_is_error(result)
        {
            self.subagent_list_notices.remove(&action.task_id);
            if action.kind == SubagentToolActionKind::Cancel {
                self.cancelling_subagents.remove(&action.task_id);
            }
            return;
        }
        match action.kind {
            SubagentToolActionKind::Check | SubagentToolActionKind::Send => {
                let now = Instant::now();
                self.subagent_list_notices.insert(
                    action.task_id,
                    SubagentListNotice::Flash {
                        started_at: now,
                        until: now + SUBAGENT_NOTICE_DURATION,
                    },
                );
            }
            SubagentToolActionKind::Wait => {
                self.subagent_list_notices.remove(&action.task_id);
            }
            SubagentToolActionKind::Cancel => {
                self.cancelling_subagents.insert(action.task_id);
            }
        }
    }

    fn running_subagent_by_id(&self, task_id: &str) -> Option<&SubagentTask> {
        self.subagents.iter().find(|task| {
            task.status == SubagentStatus::Running && task.task_id.as_deref() == Some(task_id)
        })
    }

    fn subagent_list_notice_at(&self, task_id: &str, now: Instant) -> Option<SubagentListNotice> {
        if self.cancelling_subagents.contains(task_id) {
            return Some(SubagentListNotice::Cancelling);
        }
        match self.subagent_list_notices.get(task_id).copied() {
            Some(SubagentListNotice::Flash { until, .. }) if now >= until => None,
            notice => notice,
        }
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
        let model = Some(self.model.clone());
        let effort = self.effort.clone();
        self.subagents.push(SubagentTask {
            call_id: call_id.to_owned(),
            task_id: None,
            task: task.clone(),
            model,
            effort,
            status: SubagentStatus::Queued,
            result: None,
            creation_completed: false,
            stream: vec![SubagentStreamItem::User(task.clone())],
            stream_chars: task.chars().count(),
        });
    }

    fn update_subagent_queued(&mut self, call_id: &str, result: &Value) {
        let task_id = {
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
            task.task_id.clone()
        };

        if let Some(task_id) = task_id {
            if let Some(activities) = self.pending_subagent_activities.remove(&task_id) {
                for activity in activities {
                    self.apply_subagent_activity(activity);
                }
            }
        }
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
            if subagent_completion_status(&result) == "completed" {
                self.completed_subagent_calls.insert(call_id);
            } else {
                self.failed_subagent_calls.insert(call_id);
            }
            self.subagents.remove(index);
            self.subagent_focus = match self.subagent_focus {
                None => None,
                Some(_) if self.subagents.is_empty() => None,
                Some(focus) if focus > index => Some(focus - 1),
                Some(focus) if focus == index => Some(focus.min(self.subagents.len() - 1)),
                Some(focus) => Some(focus),
            };
        }

        self.pending_subagent_activities.remove(task_id);
        self.subagent_list_notices.remove(task_id);
        self.cancelling_subagents.remove(task_id);
        self.terminal_subagents.insert(task_id.to_owned());
    }

    fn apply_subagent_activity(&mut self, activity: SubagentActivity) {
        let task_id = match &activity {
            SubagentActivity::Event { task_id, .. }
            | SubagentActivity::ReasoningStarted { task_id }
            | SubagentActivity::ReasoningCompleted { task_id } => Some(task_id.clone()),
        };
        if let Some(task_id) = task_id {
            let registered = self
                .subagents
                .iter()
                .any(|task| task.task_id.as_deref() == Some(task_id.as_str()));
            if !registered {
                if !self.terminal_subagents.contains(&task_id) {
                    self.pending_subagent_activities
                        .entry(task_id)
                        .or_default()
                        .push(activity);
                }
                return;
            }
        }

        match activity {
            SubagentActivity::ReasoningStarted { task_id } => {
                let already_reasoning = self
                    .subagents
                    .iter()
                    .find(|task| task.task_id.as_deref() == Some(task_id.as_str()))
                    .is_some_and(|task| {
                        matches!(
                            task.stream.last(),
                            Some(SubagentStreamItem::Reasoning { complete: false })
                        )
                    });
                if !already_reasoning {
                    self.append_subagent_stream_item(
                        task_id,
                        SubagentStreamItem::Reasoning { complete: false },
                        0,
                    );
                }
            }
            SubagentActivity::ReasoningCompleted { task_id } => {
                if let Some(task) = self
                    .subagents
                    .iter_mut()
                    .find(|task| task.task_id.as_deref() == Some(task_id.as_str()))
                {
                    if let Some(SubagentStreamItem::Reasoning { complete }) = task.stream.last_mut()
                    {
                        *complete = true;
                    }
                }
            }
            SubagentActivity::Event { task_id, event } => {
                let (item, chars) = match event {
                    ProtocolEvent::AssistantDelta { text } => {
                        let text = redact_secret(&text, Some(&self.secret));
                        let chars = text.chars().count();
                        (SubagentStreamItem::Assistant(text), chars)
                    }
                    ProtocolEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        let arguments = redact_secret(&arguments, Some(&self.secret));
                        let chars = arguments.chars().count() + name.len() + id.len();
                        (
                            SubagentStreamItem::ToolCall {
                                id,
                                name,
                                arguments,
                            },
                            chars,
                        )
                    }
                    ProtocolEvent::ToolResult { id, name, result } => {
                        let encoded = result.to_string();
                        let chars = encoded.chars().count() + name.len() + id.len();
                        (SubagentStreamItem::ToolResult { id, name, result }, chars)
                    }
                    ProtocolEvent::Session { .. }
                    | ProtocolEvent::BackgroundResultPending { .. }
                    | ProtocolEvent::BackgroundResultDelivered { .. }
                    | ProtocolEvent::TurnEnd
                    | ProtocolEvent::TurnInterrupted { .. }
                    | ProtocolEvent::Error { .. } => return,
                };
                self.append_subagent_stream_item(task_id, item, chars);
            }
        }
    }

    fn append_subagent_stream_item(
        &mut self,
        task_id: String,
        item: SubagentStreamItem,
        chars: usize,
    ) {
        let Some(task) = self
            .subagents
            .iter_mut()
            .find(|task| task.task_id.as_deref() == Some(task_id.as_str()))
        else {
            return;
        };
        // Provider text arrives in deltas. Keep the same assistant message
        // together in the preview instead of producing one row per chunk.
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

    fn running_subagent_indices(&self) -> Vec<usize> {
        self.subagents
            .iter()
            .enumerate()
            .filter_map(|(index, task)| (task.status == SubagentStatus::Running).then_some(index))
            .collect()
    }

    fn focus_subagent_list_from_input(&mut self) -> bool {
        let Some(index) = self.running_subagent_indices().first().copied() else {
            return false;
        };
        self.subagent_focus = Some(index);
        true
    }

    fn move_subagent_focus(&mut self, down: bool) -> bool {
        let Some(focus) = self.subagent_focus else {
            return false;
        };
        let running = self.running_subagent_indices();
        let Some(position) = running.iter().position(|index| *index == focus) else {
            self.subagent_focus = None;
            return true;
        };
        self.subagent_focus = if down {
            running.get(position + 1).copied()
        } else {
            position
                .checked_sub(1)
                .and_then(|previous| running.get(previous).copied())
        };
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
            } => self.add_live_tool_call(&crate::model::ChatToolCall {
                id,
                name,
                arguments,
            }),
            ProtocolEvent::ToolResult { id, name, result } => {
                self.add_live_tool_result(&id, &name, result)
            }
            ProtocolEvent::BackgroundResultPending {
                completion_id,
                task_id,
                status,
                result,
                ..
            } => {
                self.background_result_tasks
                    .insert(completion_id.clone(), task_id.clone());
                self.complete_subagent(&task_id, result);
                self.transcript.push(TranscriptItem::SubagentLifecycle {
                    completion_id,
                    task_id,
                    status,
                    delivered: false,
                });
            }
            ProtocolEvent::BackgroundResultDelivered {
                completion_id,
                task_id,
                ..
            } => {
                self.terminal_subagents.insert(task_id.clone());
                self.background_result_tasks
                    .insert(completion_id.clone(), task_id.clone());
                self.transcript.push(TranscriptItem::SubagentLifecycle {
                    completion_id,
                    task_id,
                    status: String::new(),
                    delivered: true,
                });
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
    if task.stream_chars <= SUBAGENT_STREAM_MAX_CHARS {
        return;
    }

    // Reserve one character for the marker so the retained stream visibly
    // distinguishes omitted history from a complete worker message.
    let mut chars_to_drop = task
        .stream_chars
        .saturating_sub(SUBAGENT_STREAM_MAX_CHARS)
        .saturating_add(1);
    while chars_to_drop > 0 && !task.stream.is_empty() {
        let item = task.stream.remove(0);
        let item_chars = subagent_stream_item_chars(&item);
        task.stream_chars = task.stream_chars.saturating_sub(item_chars);
        if item_chars <= chars_to_drop {
            chars_to_drop -= item_chars;
            continue;
        }

        match item {
            SubagentStreamItem::User(text) => {
                let text = truncate_subagent_stream_tail(
                    &text,
                    item_chars.saturating_sub(chars_to_drop).saturating_add(1),
                );
                task.stream_chars = task.stream_chars.saturating_add(text.chars().count());
                task.stream.insert(0, SubagentStreamItem::User(text));
            }
            SubagentStreamItem::Assistant(text) => {
                let text = truncate_subagent_stream_tail(
                    &text,
                    item_chars.saturating_sub(chars_to_drop).saturating_add(1),
                );
                task.stream_chars = task.stream_chars.saturating_add(text.chars().count());
                task.stream.insert(0, SubagentStreamItem::Assistant(text));
            }
            // Never turn structured tool data into assistant text: doing so
            // bypasses the shared tool renderer and exposes raw JSON in the
            // worker overlay when its bounded history is trimmed.
            SubagentStreamItem::Reasoning { .. }
            | SubagentStreamItem::ToolCall { .. }
            | SubagentStreamItem::ToolResult { .. } => {
                task.stream
                    .insert(0, SubagentStreamItem::Assistant("…".to_owned()));
                task.stream_chars = task.stream_chars.saturating_add(1);
            }
        }
        return;
    }

    if !task.stream.is_empty() {
        task.stream
            .insert(0, SubagentStreamItem::Assistant("…".to_owned()));
        task.stream_chars = task.stream_chars.saturating_add(1);
    }
}

fn subagent_stream_item_chars(item: &SubagentStreamItem) -> usize {
    match item {
        SubagentStreamItem::User(text) | SubagentStreamItem::Assistant(text) => {
            text.chars().count()
        }
        SubagentStreamItem::Reasoning { .. } => 0,
        SubagentStreamItem::ToolCall {
            id,
            name,
            arguments,
        } => id.len() + name.len() + arguments.chars().count(),
        SubagentStreamItem::ToolResult { id, name, result } => {
            id.len() + name.len() + result.to_string().chars().count()
        }
    }
}

fn truncate_subagent_stream_tail(text: &str, limit: usize) -> String {
    let chars = text.chars().count();
    if chars <= limit {
        return text.to_owned();
    }
    if limit == 0 {
        return String::new();
    }
    let tail = text
        .chars()
        .skip(chars.saturating_sub(limit.saturating_sub(1)))
        .collect::<String>();
    format!("…{tail}")
}

fn subagent_completion_status(result: &Value) -> &'static str {
    if result.get("interrupted").is_some() {
        "interrupted"
    } else if result.get("cancelled").is_some() {
        "canceled"
    } else if result.get("error").is_some() {
        "failed"
    } else {
        "completed"
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
    SubagentLifecycle {
        completion_id: String,
        task_id: String,
        status: String,
        delivered: bool,
    },
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

fn ui_layout(
    state: &UiState,
    area: Rect,
) -> (Rect, Option<Rect>, Option<Rect>, Option<Rect>, Rect, Rect) {
    let prompt_rows = input_visible_rows(state, ui_prompt_content_width(area));
    let list_height = subagent_list_height(state);
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
    let usable_height = area.height.saturating_sub(bottom_margin);
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
    let stream_area = subagent_stream_overlay_area(state, input_area, area.y);
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

/// Keep the console inset without allowing margins to consume all available
/// width. A narrow terminal sheds margin cells before it sheds the console.
fn bottom_console_area(area: Rect, y: u16, height: u16) -> Rect {
    let horizontal_margin = area.width.saturating_sub(1) / 2;
    let horizontal_margin = horizontal_margin.min(2);
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

    let requested_list = subagent_list_height(state);
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

fn subagent_list_area(state: &UiState, input_area: Rect) -> Option<Rect> {
    let inner = console_content_area(input_area);
    let content = bottom_content_heights(state, input_area);
    (content.list > 0).then(|| {
        Rect::new(
            inner.x,
            inner.y
                + content.queue
                + content.queue_separator
                + content.prompt
                + content.list_separator,
            inner.width,
            content.list,
        )
    })
}

#[cfg(test)]
fn console_spacer_rows(
    state: &UiState,
    input_area: Rect,
) -> (Option<u16>, Option<u16>, Option<u16>) {
    let inner = console_content_area(input_area);
    let content = bottom_content_heights(state, input_area);
    let queue_prompt = (content.queue_separator > 0).then_some(inner.y + content.queue);
    let list_prompt = (content.list_separator > 0)
        .then_some(inner.y + content.queue + content.queue_separator + content.prompt);
    let prompt_status = (content.status_separator > 0)
        .then_some(inner.y + inner.height.saturating_sub(content.status + 1));
    (queue_prompt, list_prompt, prompt_status)
}

fn subagent_list_height(state: &UiState) -> u16 {
    let running = state
        .subagents
        .iter()
        .filter(|task| task.status == SubagentStatus::Running)
        .count()
        .min(u16::MAX as usize - 1) as u16;
    u16::from(running > 0) + running
}

/// Focused worker output uses the same transient slot as the slash picker:
/// immediately above the input, without changing the transcript viewport.
fn subagent_stream_overlay_area(state: &UiState, input_area: Rect, top: u16) -> Option<Rect> {
    let focus = state.subagent_focus?;
    let task = state.subagents.get(focus)?;
    if task.status != SubagentStatus::Running {
        return None;
    }
    let available = input_area.y.saturating_sub(top);
    if available == 0 {
        return None;
    }
    let height = SUBAGENT_STREAM_PREVIEW_HEIGHT.min(available);
    Some(Rect::new(
        input_area.x,
        input_area.y - height,
        input_area.width,
        height,
    ))
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

fn move_up_from_input_or_subagent(state: &mut UiState, width: usize) -> bool {
    if state.subagent_focus.is_some() {
        return state.move_subagent_focus(false);
    }
    state.move_skill_picker(false) || move_input_cursor_vertical(state, width, false)
}

fn move_down_from_input(state: &mut UiState, width: usize) -> bool {
    let width = width.max(1);
    let rows = input_visual_rows(&state.input, width);
    let on_last_row = input_cursor_row(&state.input, state.cursor, width) + 1 == rows.len();
    if on_last_row && state.focus_subagent_list_from_input() {
        return true;
    }
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
    let (chat_chunk, picker_area, overlay_area, queue_area, input_chunk, status_area) =
        ui_layout(state, area);

    // The queue, running workers, prompt, and status line share one background
    // surface. Transient picker and worker-stream surfaces remain above it.
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
        let transcript = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(transcript, visible_chat_area);
    }

    let activity_now = Instant::now();
    let activity_elapsed = state.console_animation_elapsed_at(activity_now);
    let console_visibility = state.console_visibility_at(activity_now);
    apply_tui_glow(
        frame,
        full_area,
        input_chunk,
        activity_elapsed,
        console_visibility,
    );
    // Keep the reflection in the existing transcript gap. On constrained
    // layouts the row above the console belongs to the transcript instead.
    if chat_chunk.y.saturating_add(chat_chunk.height) < input_chunk.y {
        apply_console_top_reflection(frame, input_chunk, activity_elapsed, console_visibility);
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
    if let Some(list_area) = subagent_list_area(state, input_chunk) {
        draw_subagent_list(frame, state, list_area);
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
        Paragraph::new(model_status_line(state, effort)),
        status_area,
    );

    apply_console_background(frame, input_chunk, activity_elapsed, console_visibility);

    // The task overlay is a floating surface: draw it after the transcript,
    // picker, input, and status layers so none can cut through its left edge
    // or upper-left corner on a constrained terminal layout.
    if let Some(overlay_area) = overlay_area {
        draw_subagent_stream_overlay(frame, state, overlay_area);
    }
    if let Some(settings) = &state.settings {
        draw_settings(frame, settings, area);
    }

    // A frame cursor makes Ratatui issue `Show` after every redraw. Only set
    // one while focused, so background glow redraws cannot re-show it.
    if state.terminal_focused && state.settings.is_none() && !prompt_area.is_empty() && visible > 0
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

// Hashes vary HSV saturation at a fixed 315° magenta hue.
const SUBAGENT_ID_COLORS: [Color; 8] = [
    Color::Rgb(220, 36, 174),
    Color::Rgb(220, 64, 181),
    Color::Rgb(220, 92, 188),
    Color::Rgb(220, 120, 195),
    Color::Rgb(220, 148, 202),
    Color::Rgb(220, 176, 209),
    Color::Rgb(220, 204, 216),
    Color::Rgb(220, 212, 218),
];

fn is_subagent_tool(name: &str) -> bool {
    matches!(
        name,
        "spawn_subagent" | "check_subagent" | "wait_subagent" | "send_subagent" | "cancel_subagent"
    )
}

fn subagent_tool_action_kind(name: &str) -> Option<SubagentToolActionKind> {
    match name {
        "check_subagent" => Some(SubagentToolActionKind::Check),
        "wait_subagent" => Some(SubagentToolActionKind::Wait),
        "send_subagent" => Some(SubagentToolActionKind::Send),
        "cancel_subagent" => Some(SubagentToolActionKind::Cancel),
        _ => None,
    }
}

fn subagent_tool_task_id(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|task_id| !task_id.trim().is_empty())
}

fn subagent_tool_result_is_error(result: &Value) -> bool {
    result.get("error").is_some()
        || matches!(
            result.get("status").and_then(Value::as_str),
            Some("unknown" | "failed" | "parent_canceled")
        )
}

fn subagent_tool_error_message(result: &Value) -> String {
    result
        .get("error")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            result
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "failed".to_owned())
}

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
    let chrome = Style::default().fg(SUBAGENT_TITLE_COLOR);
    frame.render_widget(
        Paragraph::new(Line::styled("Subagents", chrome)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let running = state.running_subagent_indices();
    let item_height = area.height.saturating_sub(1) as usize;
    let range = state
        .subagent_focus
        .and_then(|focus| running.iter().position(|index| *index == focus))
        .map(|focus| selection_range(running.len(), focus, item_height))
        .unwrap_or(0..running.len().min(item_height));
    for (row, position) in range.enumerate() {
        let index = running[position];
        let task = &state.subagents[index];
        let selected = state.subagent_focus == Some(index);
        let id = task.task_id.as_deref().unwrap_or(&task.call_id);
        let notice = state.subagent_list_notice_at(id, Instant::now());
        let mut style = Style::default().fg(subagent_id_color(id));
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        let preview = truncate_chars(
            &task.task.replace(['\n', '\r'], " ↵ "),
            SUBAGENT_TASK_PREVIEW_CHARS,
        );
        let line = match notice {
            Some(SubagentListNotice::Waiting) => {
                format!("Waiting for {id} {} · {preview}", tool_spinner_frame(state))
            }
            Some(SubagentListNotice::Cancelling) => {
                format!("Cancelling {id} {} · {preview}", tool_spinner_frame(state))
            }
            Some(SubagentListNotice::Flash { started_at, .. }) => {
                if (Instant::now()
                    .saturating_duration_since(started_at)
                    .as_millis()
                    / SUBAGENT_NOTICE_FLASH_INTERVAL.as_millis())
                .is_multiple_of(2)
                {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                format!("{id} · {preview}")
            }
            None => format!("{id} · {preview}"),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("│ ", chrome),
                Span::styled(line, style),
            ])),
            Rect::new(area.x, area.y + row as u16 + 1, area.width, 1),
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
    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    frame.render_widget(Clear, area);
    let buffer = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            buffer[(x, y)].set_bg(SUBAGENT_OVERLAY_BACKGROUND);
        }
    }
    if inner.is_empty() {
        return;
    }

    let lines = latest_subagent_stream_lines(task, inner.width.max(1) as usize, state);
    let start = lines.len().saturating_sub(inner.height as usize);
    frame.render_widget(Paragraph::new(lines[start..].to_vec()), inner);
}

fn latest_subagent_stream_lines(
    task: &SubagentTask,
    width: usize,
    state: &UiState,
) -> Vec<Line<'static>> {
    subagent_stream_lines(task, width, state)
}

fn subagent_stream_lines(task: &SubagentTask, width: usize, state: &UiState) -> Vec<Line<'static>> {
    if task.stream.is_empty() {
        return vec![Line::raw("waiting for worker output")];
    }
    let stream = task
        .stream
        .iter()
        .map(|item| match item {
            SubagentStreamItem::User(text) => TranscriptItem::User {
                text: text.clone(),
                skill_instruction_attached: false,
            },
            SubagentStreamItem::Assistant(text) => TranscriptItem::Assistant(text.clone()),
            SubagentStreamItem::Reasoning { complete } => TranscriptItem::Reasoning {
                complete: *complete,
            },
            SubagentStreamItem::ToolCall {
                id,
                name,
                arguments,
            } => TranscriptItem::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            },
            SubagentStreamItem::ToolResult { id, name, result } => TranscriptItem::ToolResult {
                id: id.clone(),
                name: name.clone(),
                result: result.clone(),
            },
        })
        .collect::<Vec<_>>();
    // Worker content uses the same transcript formatting and suppression rules
    // as the main stream; only the overlay viewport follows its own tail.
    render_transcript_items(&stream, width.max(1), state, true)
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
    render_transcript_items(&state.transcript, width.max(1) as usize, state, true)
}

fn render_transcript_items(
    transcript: &[TranscriptItem],
    width: usize,
    state: &UiState,
    suppress_subagent_tools: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut rendered_item = false;

    for (index, item) in transcript.iter().enumerate() {
        // Results are positioned on their matching call, even when the model
        // emitted several calls before execution produced any result.
        if is_result_attached_to_call(transcript, index) {
            continue;
        }
        if suppress_subagent_tools
            && matches!(
                item,
                TranscriptItem::ToolCall { name, .. } | TranscriptItem::ToolResult { name, .. }
                    if is_subagent_tool(name)
            )
        {
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
                if !suppress_subagent_tools || !is_subagent_tool(name) {
                    let segments = if name == "cmd" {
                        cmd_tool_segments(id, arguments, result, state)
                    } else {
                        generic_tool_segments(name, arguments, result, state)
                    };
                    push_spans_wrapped(&mut lines, &segments, width);
                }
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
            TranscriptItem::SubagentLifecycle {
                completion_id,
                task_id,
                status,
                delivered,
            } => {
                let (marker, text, style) = if *delivered {
                    (
                        "✓",
                        format!("✓ subagent  {task_id}  · result delivered ({completion_id})"),
                        tool_result_style(),
                    )
                } else {
                    let marker = if status == "completed" { "·" } else { "!" };
                    let style = if status == "completed" {
                        info_style()
                    } else {
                        error_style()
                    };
                    (
                        marker,
                        format!("{marker} subagent  {task_id}  → {status} · result pending ({completion_id})"),
                        style,
                    )
                };
                let _ = marker;
                push_wrapped(&mut lines, &text, width, style);
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

fn model_status_line(state: &UiState, effort: &str) -> Line<'static> {
    let model = redact_secret(&state.model, Some(&state.secret));
    let effort = redact_secret(effort, Some(&state.secret));
    Line::styled(
        format!("{model} · {effort} | {}", context_status_text(state)),
        context_status_style(state),
    )
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
        // ANSI RGB equivalent while blending into and out of the accent cycle.
        Color::Cyan => (0, 255, 255),
        _ => unreachable!("activity transition colours are cyan or RGB"),
    }
}

fn console_reach_at(elapsed: Duration) -> f32 {
    let phase = elapsed.as_secs_f32() / CONSOLE_BOUNDARY_CYCLE.as_secs_f32();
    let midpoint = (CONSOLE_REACH_MIN + CONSOLE_REACH_MAX) / 2.0;
    let amplitude = (CONSOLE_REACH_MAX - CONSOLE_REACH_MIN) / 2.0;
    midpoint + amplitude * (phase * std::f32::consts::TAU).sin()
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

/// Return a smooth half-elliptical falloff beneath the console.
///
/// The TUI's bottom edge cuts the ellipse on its horizontal centreline, so the
/// visible upper half grows wider toward the bottom. The glow is never taller
/// than `GLOW_HEIGHT` terminal rows and reaches farther horizontally than it
/// does vertically.
fn glow_coverage_at(column: u16, row: u16, canvas: Rect, console_area: Rect) -> f32 {
    let canvas_bottom = canvas.y.saturating_add(canvas.height);
    if canvas.is_empty() || row < canvas.y || row >= canvas_bottom {
        return 0.0;
    }

    let left = console_area.x.max(canvas.x);
    let right = console_area
        .x
        .saturating_add(console_area.width)
        .min(canvas.x.saturating_add(canvas.width));
    if left >= right {
        return 0.0;
    }

    let x = canvas.x.saturating_add(column);
    let center_x = (left as f32 + right.saturating_sub(1) as f32) / 2.0;
    let horizontal_radius = (right - left) as f32 / 2.0 + GLOW_HORIZONTAL_SPREAD as f32;
    let horizontal_distance = (x as f32 - center_x).abs() / horizontal_radius;
    // Sample the lower edge of each cell: the bottom edge of the TUI is the
    // ellipse's centreline while its visible half remains within the canvas.
    let vertical_distance =
        (row.saturating_add(1) as f32 - canvas_bottom as f32).abs() / GLOW_HEIGHT as f32;
    let distance = horizontal_distance.hypot(vertical_distance);
    let falloff = (1.0 - distance).clamp(0.0, 1.0);
    falloff * falloff * (3.0 - 2.0 * falloff)
}

fn glow_accent_at(elapsed: Duration) -> Color {
    glow_accent_with_desaturation_at(elapsed, GLOW_DESATURATION)
}

fn glow_accent_with_desaturation_at(elapsed: Duration, desaturation: f32) -> Color {
    let (red, green, blue) = activity_rgb(console_accent_at(elapsed));
    let neutral = ((u16::from(red) + u16::from(green) + u16::from(blue)) / 3) as u8;
    Color::Rgb(
        interpolate_color(red, neutral, desaturation),
        interpolate_color(green, neutral, desaturation),
        interpolate_color(blue, neutral, desaturation),
    )
}

fn glow_color_at(
    elapsed: Duration,
    column: u16,
    _width: u16,
    row: u16,
    canvas: Rect,
    console_area: Rect,
    visibility: f32,
) -> Option<Color> {
    let visibility = visibility.clamp(0.0, 1.0);
    let bottom = canvas.y.saturating_add(canvas.height);
    if visibility <= 0.0 || row < canvas.y || row >= bottom {
        return None;
    }

    let coverage = glow_coverage_at(column, row, canvas, console_area);
    if coverage <= 0.0 {
        return None;
    }

    let intensity =
        (console_reach_at(elapsed) / CONSOLE_REACH_MAX) * GLOW_INTENSITY * coverage * visibility;
    Some(blend_rgb(
        TUI_GLOW_BACKGROUND,
        glow_accent_at(elapsed),
        intensity,
    ))
}

fn apply_tui_glow(
    frame: &mut Frame<'_>,
    canvas: Rect,
    console_area: Rect,
    elapsed: Duration,
    visibility: f32,
) {
    let left = console_area
        .x
        .saturating_sub(GLOW_HORIZONTAL_SPREAD)
        .max(canvas.x);
    let right = console_area
        .x
        .saturating_add(console_area.width)
        .saturating_add(GLOW_HORIZONTAL_SPREAD)
        .min(canvas.x.saturating_add(canvas.width));
    let bottom = canvas.y.saturating_add(canvas.height);
    let top = bottom.saturating_sub(GLOW_HEIGHT).max(canvas.y);
    let buffer = frame.buffer_mut();
    for y in top..bottom {
        for x in left..right {
            if let Some(color) = glow_color_at(
                elapsed,
                x.saturating_sub(canvas.x),
                canvas.width,
                y,
                canvas,
                console_area,
                visibility,
            ) {
                buffer[(x, y)].set_bg(color);
            }
        }
    }
}

fn console_glass_color_at(elapsed: Duration, glow: Color, visibility: f32) -> Color {
    let (red, green, blue) = activity_rgb(console_accent_at(elapsed));
    let neutral = ((u16::from(red) + u16::from(green) + u16::from(blue)) / 3) as u8;
    let glass_accent = Color::Rgb(
        interpolate_color(red, neutral, CONSOLE_GLASS_DESATURATION),
        interpolate_color(green, neutral, CONSOLE_GLASS_DESATURATION),
        interpolate_color(blue, neutral, CONSOLE_GLASS_DESATURATION),
    );
    let visibility = visibility.clamp(0.0, 1.0);
    let tint = blend_rgb(
        CONSOLE_BACKGROUND,
        glass_accent,
        CONSOLE_GLASS_TINT * visibility,
    );
    let white_tinted = blend_rgb(
        tint,
        Color::Rgb(255, 255, 255),
        CONSOLE_GLASS_WHITE_TINT * visibility,
    );
    blend_rgb(white_tinted, glow, CONSOLE_GLASS_GLOW_THROUGH * visibility)
}

/// Return the faint accent colour used by the external glass reflection.
fn console_top_reflection_color_at(elapsed: Duration, background: Color, visibility: f32) -> Color {
    let visibility = visibility.clamp(0.0, 1.0);
    let reflected = blend_rgb(
        background,
        glow_accent_at(elapsed),
        CONSOLE_REFLECTION_TINT * visibility,
    );
    blend_rgb(
        reflected,
        Color::Rgb(255, 255, 255),
        CONSOLE_REFLECTION_WHITE_TINT * visibility,
    )
}

/// Draw a one-eighth-cell reflection on the bottom edge of the row immediately
/// above the console. This leaves the console's own background uniform while
/// keeping the effect thin in terminals that render block glyphs.
fn apply_console_top_reflection(
    frame: &mut Frame<'_>,
    area: Rect,
    elapsed: Duration,
    visibility: f32,
) {
    if visibility <= 0.0 || area.y == 0 {
        return;
    }

    let y = area.y - 1;
    let buffer = frame.buffer_mut();
    for x in area.x..area.x.saturating_add(area.width) {
        let background = match buffer[(x, y)].bg {
            Color::Rgb(_, _, _) => buffer[(x, y)].bg,
            _ => TUI_GLOW_BACKGROUND,
        };
        buffer[(x, y)].set_symbol(CONSOLE_REFLECTION_GLYPH).set_fg(
            console_top_reflection_color_at(elapsed, background, visibility),
        );
    }
}

/// Composite the dark, low-saturation glass only over the console rectangle.
fn apply_console_background(frame: &mut Frame<'_>, area: Rect, elapsed: Duration, visibility: f32) {
    let buffer = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            let glow = match buffer[(x, y)].bg {
                Color::Rgb(_, _, _) => buffer[(x, y)].bg,
                _ => TUI_GLOW_BACKGROUND,
            };
            buffer[(x, y)].set_bg(console_glass_color_at(elapsed, glow, visibility));
        }
    }
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

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

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
    fn context_status_shows_used_window_and_percentage_in_uniform_gray() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
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
    fn context_status_keeps_percentage_and_bar_consistent_at_capacity() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
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
        let state = UiState::from_history(&[], "secret", "model", None, false);

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
        let state = UiState::from_history(&[], "secret", "model", None, false);
        let viewport = tui_viewport(Rect::new(0, 0, 80, 14));
        let (chat, _, _, _, console, _) = ui_layout(&state, viewport);
        let content = console_content_area(console);

        assert_eq!(chat.x, console.x);
        assert_eq!(chat.width, console.width);
        assert_eq!(console, Rect::new(viewport.x + 2, 8, viewport.width - 4, 5));
        assert_eq!(console.y + console.height, viewport.y + viewport.height - 1);
        assert_eq!(content.x, console.x + 2);
        assert_eq!(content.width, console.width - 4);
        assert_eq!(content.y, console.y + 1);
        assert_eq!(content.y + content.height, console.y + console.height - 1);

        for (width, margin, console_width) in
            [(1, 0, 1), (2, 0, 2), (3, 1, 1), (4, 1, 2), (5, 2, 1)]
        {
            let console = bottom_console_area(Rect::new(0, 0, width, 4), 0, 4);
            assert_eq!(console.x, margin, "width {width}");
            assert_eq!(console.width, console_width, "width {width}");
        }
    }

    #[test]
    fn inset_console_width_drives_prompt_rows_and_vertical_navigation() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.input = "x".repeat(71);
        let viewport = tui_viewport(Rect::new(0, 0, 80, 14));
        let console = ui_layout(&state, viewport).4;
        let prompt = prompt_area(console, &state);

        assert_eq!(ui_prompt_content_width(viewport), prompt.width);
        assert_eq!(prompt.width, 70);
        assert_eq!(input_visible_rows(&state, prompt.width), 2);
        assert!(move_input_cursor_vertical(
            &mut state,
            ui_prompt_content_width(viewport) as usize,
            true,
        ));
    }

    #[test]
    fn context_immediately_follows_the_left_status_flow_in_uniform_gray() {
        let state =
            UiState::from_history(&[], "secret", "model", None, false).with_context(Some(100), 81);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 10)).expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw statusline");

        let buffer = terminal.backend().buffer();
        let status_area = ui_layout(&state, tui_viewport(Rect::new(0, 0, 80, 10))).5;
        let expected = "model · default | Context: 81/100 (81%) █████████░";
        let rendered = (status_area.x..status_area.x + expected.chars().count() as u16)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert_eq!(rendered, expected);
        assert_eq!(
            buffer[(
                status_area.x + expected.chars().count() as u16,
                status_area.y
            )]
                .symbol(),
            " ",
            "context is not pushed to the right edge"
        );
        for x in status_area.x..status_area.x + expected.chars().count() as u16 {
            assert_eq!(buffer[(x, status_area.y)].fg, CONSOLE_STATUS_COLOR);
        }
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
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
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

    fn color_distance(from: Color, to: Color) -> u16 {
        let (from_red, from_green, from_blue) = activity_rgb(from);
        let (to_red, to_green, to_blue) = activity_rgb(to);
        u16::from(from_red.abs_diff(to_red))
            + u16::from(from_green.abs_diff(to_green))
            + u16::from(from_blue.abs_diff(to_blue))
    }

    fn color_saturation(color: Color) -> u8 {
        let (red, green, blue) = activity_rgb(color);
        red.max(green).max(blue) - red.min(green).min(blue)
    }

    fn color_luminance(color: Color) -> u32 {
        let (red, green, blue) = activity_rgb(color);
        299 * u32::from(red) + 587 * u32::from(green) + 114 * u32::from(blue)
    }

    #[test]
    fn glow_coverage_is_a_bottom_anchored_half_ellipse_with_a_twelve_row_cap() {
        let canvas = Rect::new(0, 0, 160, 40);
        let console = Rect::new(40, 28, 80, 7);
        let bottom = canvas.y + canvas.height;
        let center_x = console.x + console.width / 2 - 1;
        let outer_x = center_x + 35;
        let active_rows = (canvas.y..bottom)
            .filter(|&row| glow_coverage_at(center_x, row, canvas, console) > 0.0)
            .collect::<Vec<_>>();

        assert_eq!(GLOW_HEIGHT, 12);
        assert_eq!(GLOW_HORIZONTAL_SPREAD, 24);
        assert_eq!(
            active_rows,
            (bottom - GLOW_HEIGHT..bottom).collect::<Vec<_>>()
        );
        assert_eq!(
            glow_coverage_at(center_x, bottom - GLOW_HEIGHT - 1, canvas, console),
            0.0,
            "the glow never exceeds its twelve-row cap"
        );
        assert_eq!(
            glow_coverage_at(center_x, bottom - 1, canvas, console),
            glow_coverage_at(center_x + 1, bottom - 1, canvas, console),
            "the bottom edge intersects the ellipse at its horizontal centreline"
        );
        assert_eq!(
            glow_coverage_at(outer_x, bottom - GLOW_HEIGHT, canvas, console),
            0.0,
            "the top of the half ellipse stays narrow"
        );
        assert!(
            glow_coverage_at(outer_x, bottom - 1, canvas, console) > 0.0,
            "the exposed glow grows wider toward the TUI bottom"
        );
        assert!(
            glow_coverage_at(console.x + console.width + 23, bottom - 1, canvas, console) > 0.0,
            "the glow extends twenty-four cells beyond each console edge"
        );
    }

    #[test]
    fn wider_console_has_a_wider_elliptical_side_falloff() {
        let canvas = Rect::new(0, 0, 200, 20);
        let wide_console = Rect::new(50, 12, 100, 7);
        let narrow_console = Rect::new(50, 12, 4, 7);
        let bottom_row = canvas.y + canvas.height - 1;
        let offset_from_center = 60;
        let wide_sample = wide_console.x + wide_console.width / 2 + offset_from_center;
        let narrow_sample = narrow_console.x + narrow_console.width / 2 + offset_from_center;

        let wide = glow_coverage_at(wide_sample, bottom_row, canvas, wide_console);
        let narrow = glow_coverage_at(narrow_sample, bottom_row, canvas, narrow_console);

        assert!(wide > 0.0);
        assert_eq!(narrow, 0.0);
        assert!(
            wide > narrow,
            "the horizontal radius follows the console width to form an ellipse rather than fixed endpoint circles"
        );
    }

    #[test]
    fn exposed_bloom_is_not_tinted_as_console_glass() {
        let canvas = Rect::new(0, 0, 80, 20);
        let console = Rect::new(20, 12, 40, 7);
        let elapsed = CONSOLE_BOUNDARY_CYCLE / 4;
        let source_row = canvas.y + canvas.height - 1;
        let exposed = (console.x - 1, source_row);
        let inside = (console.x, console.y + console.height - 1);
        let exposed_glow = glow_color_at(
            elapsed,
            exposed.0,
            canvas.width,
            exposed.1,
            canvas,
            console,
            1.0,
        )
        .expect("endpoint bloom");
        let inside_glow = glow_color_at(
            elapsed,
            inside.0,
            canvas.width,
            inside.1,
            canvas,
            console,
            1.0,
        )
        .expect("source segment glow");
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(
            canvas.width,
            canvas.height,
        ))
        .expect("test terminal");

        terminal
            .draw(|frame| {
                apply_tui_glow(frame, canvas, console, elapsed, 1.0);
                apply_console_background(frame, console, elapsed, 1.0);
                let buffer = frame.buffer_mut();
                assert_eq!(buffer[exposed].bg, exposed_glow);
                assert_eq!(
                    buffer[inside].bg,
                    console_glass_color_at(elapsed, inside_glow, 1.0)
                );
            })
            .expect("render glow and glass");
    }

    #[test]
    fn idle_canvas_has_no_glow() {
        let canvas = Rect::new(0, 0, 80, 12);
        let console = Rect::new(2, 6, 76, 5);
        for row in canvas.y..canvas.y + canvas.height {
            for column in 0..canvas.width {
                assert_eq!(
                    glow_color_at(
                        CONSOLE_BOUNDARY_CYCLE / 4,
                        column,
                        canvas.width,
                        row,
                        canvas,
                        console,
                        0.0,
                    ),
                    None,
                    "idle glow never paints the canvas"
                );
            }
        }
    }

    #[test]
    fn console_visibility_transition_is_smooth_and_reversible() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.set_busy(true);
        let entering = state
            .console_visibility_transition
            .expect("entry transition");
        assert_eq!(state.console_visibility_at(entering.started_at), 0.0);
        let entry_middle =
            state.console_visibility_at(entering.started_at + CONSOLE_VISIBILITY_TRANSITION / 2);
        assert!((entry_middle - 0.5).abs() < 0.000_1);
        assert_eq!(
            state.console_visibility_at(entering.started_at + CONSOLE_VISIBILITY_TRANSITION),
            1.0
        );

        state.console_visibility_transition = None;
        state.set_busy(false);
        let exiting = state
            .console_visibility_transition
            .expect("exit transition");
        assert_eq!(state.console_visibility_at(exiting.started_at), 1.0);
        let exit_middle =
            state.console_visibility_at(exiting.started_at + CONSOLE_VISIBILITY_TRANSITION / 2);
        assert!((exit_middle - 0.5).abs() < 0.000_1);
        assert_eq!(
            state.console_visibility_at(exiting.started_at + CONSOLE_VISIBILITY_TRANSITION),
            0.0
        );
    }

    #[test]
    fn rapid_console_reentry_preserves_glow_phase() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        let canvas = Rect::new(0, 0, 80, 12);
        let console = Rect::new(2, 6, 76, 5);
        let start = Instant::now();
        state.set_busy_at(true, start);
        let epoch = state.console_animation_epoch;
        state.set_busy_at(false, start + CONSOLE_VISIBILITY_TRANSITION);
        let reversal_at = start + CONSOLE_VISIBILITY_TRANSITION * 3 / 2;
        let visibility_before = state.console_visibility_at(reversal_at);
        let elapsed_before = state.console_animation_elapsed_at(reversal_at);
        let color_before = glow_color_at(
            elapsed_before,
            40,
            canvas.width,
            canvas.y + canvas.height - 1,
            canvas,
            console,
            visibility_before,
        );

        state.set_busy_at(true, reversal_at);

        assert_eq!(state.console_animation_epoch, epoch);
        assert!((state.console_visibility_at(reversal_at) - visibility_before).abs() < 0.000_1);
        assert_eq!(
            glow_color_at(
                state.console_animation_elapsed_at(reversal_at),
                40,
                canvas.width,
                canvas.y + canvas.height - 1,
                canvas,
                console,
                state.console_visibility_at(reversal_at),
            ),
            color_before,
            "reversing an exit keeps the current glow phase"
        );
    }

    #[test]
    fn glow_reach_still_controls_light_intensity() {
        let long = CONSOLE_BOUNDARY_CYCLE / 4;
        let short = CONSOLE_BOUNDARY_CYCLE * 3 / 4;
        let canvas = Rect::new(0, 0, 80, 12);
        let console = Rect::new(2, 6, 76, 5);
        let row = canvas.y + canvas.height - 1;
        let long_glow =
            glow_color_at(long, 40, canvas.width, row, canvas, console, 1.0).expect("visible glow");
        let short_glow = glow_color_at(short, 40, canvas.width, row, canvas, console, 1.0)
            .expect("visible glow");

        assert!((console_reach_at(long) - CONSOLE_REACH_MAX).abs() < 0.000_1);
        assert!((console_reach_at(short) - CONSOLE_REACH_MIN).abs() < 0.000_1);
        assert!(
            color_distance(TUI_GLOW_BACKGROUND, long_glow)
                > color_distance(TUI_GLOW_BACKGROUND, short_glow)
        );
    }

    #[test]
    fn maximum_glow_is_brighter_and_more_saturated_than_the_previous_tuning() {
        let canvas = Rect::new(0, 0, 160, 20);
        let console = Rect::new(40, 12, 80, 7);
        let column = console.x + console.width / 2;
        let row = canvas.y + canvas.height - 1;
        let elapsed = Duration::ZERO;
        let coverage = glow_coverage_at(column, row, canvas, console);
        assert_eq!(GLOW_INTENSITY, 0.70);
        let current = glow_color_at(elapsed, column, canvas.width, row, canvas, console, 1.0)
            .expect("bottom-centre glow");
        let previous = blend_rgb(
            TUI_GLOW_BACKGROUND,
            glow_accent_with_desaturation_at(elapsed, 0.16),
            (console_reach_at(elapsed) / CONSOLE_REACH_MAX) * 0.62 * coverage,
        );

        assert!(
            color_luminance(current) > color_luminance(previous),
            "the adjusted maximum glow is brighter than the previous maximum"
        );
        assert!(
            color_saturation(current) > color_saturation(previous),
            "the adjusted maximum glow is more saturated than the previous maximum"
        );
    }

    #[test]
    fn glow_tuning_is_brighter_and_more_saturated_than_before() {
        let canvas = Rect::new(0, 0, 160, 20);
        let console = Rect::new(40, 12, 80, 7);
        let column = console.x + console.width / 2;
        let row = canvas.y + canvas.height - 1;
        let coverage = glow_coverage_at(column, row, canvas, console);

        assert_eq!(GLOW_INTENSITY, 0.70);
        assert_eq!(GLOW_DESATURATION, 0.10);
        for (phase, elapsed) in [
            Duration::ZERO,
            console_accent_cycle() / 4,
            console_accent_cycle() / 2,
            console_accent_cycle() * 3 / 4,
        ]
        .into_iter()
        .enumerate()
        {
            let current_accent = glow_accent_at(elapsed);
            let previous_accent = glow_accent_with_desaturation_at(elapsed, 0.16);
            assert!(
                color_saturation(current_accent) > color_saturation(previous_accent),
                "phase {phase}: reducing glow desaturation raises saturation"
            );
            let current = glow_color_at(elapsed, column, canvas.width, row, canvas, console, 1.0)
                .expect("bottom-centre glow");
            let previous = blend_rgb(
                TUI_GLOW_BACKGROUND,
                glow_accent_with_desaturation_at(elapsed, 0.16),
                (console_reach_at(elapsed) / CONSOLE_REACH_MAX) * 0.62 * coverage,
            );

            assert!(
                color_luminance(current) > color_luminance(previous),
                "phase {phase}: the rendered glow is brighter than the prior tuning"
            );
            assert!(
                color_saturation(current) > color_saturation(previous),
                "phase {phase}: the rendered glow is more saturated than the prior tuning"
            );
        }
    }

    #[test]
    fn console_is_dark_lower_saturation_glass_over_the_glow() {
        let elapsed = CONSOLE_BOUNDARY_CYCLE / 4;
        let canvas = Rect::new(0, 0, 80, 12);
        let console = Rect::new(2, 6, 76, 5);
        let glow = glow_color_at(
            elapsed,
            40,
            canvas.width,
            canvas.y + canvas.height - 1,
            canvas,
            console,
            1.0,
        )
        .expect("visible glow");
        let glass = console_glass_color_at(elapsed, glow, 1.0);
        let solid_glass = console_glass_color_at(elapsed, CONSOLE_BACKGROUND, 1.0);

        assert_eq!(
            console_glass_color_at(elapsed, glow, 0.0),
            CONSOLE_BACKGROUND
        );
        assert_ne!(glass, CONSOLE_BACKGROUND);
        let (glass_red, glass_green, glass_blue) = activity_rgb(glass);
        let (glow_red, glow_green, glow_blue) = activity_rgb(glow);
        assert!(
            u16::from(glass_red) + u16::from(glass_green) + u16::from(glass_blue)
                < u16::from(glow_red) + u16::from(glow_green) + u16::from(glow_blue),
            "console glass is darker than the exposed glow"
        );
        assert!(
            color_distance(CONSOLE_BACKGROUND, glass) < color_distance(TUI_GLOW_BACKGROUND, glow),
            "glass tint is subtler than the exposed glow"
        );
        assert!(
            color_distance(CONSOLE_BACKGROUND, glass)
                > color_distance(CONSOLE_BACKGROUND, solid_glass),
            "the glow visibly carries through the stronger glass tint"
        );
        assert!(
            color_saturation(glass) < color_saturation(glow),
            "glass tint is less saturated than the exposed glow"
        );
    }

    #[test]
    fn console_top_reflection_is_a_thin_active_line_in_the_external_gap() {
        assert_eq!(CONSOLE_REFLECTION_WHITE_TINT, 0.20);
        let elapsed = Duration::ZERO;
        let canvas = Rect::new(0, 0, 20, 8);
        let console = Rect::new(2, 3, 16, 4);
        let reflection_y = console.y - 1;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(
            canvas.width,
            canvas.height,
        ))
        .expect("test terminal");
        terminal
            .draw(|frame| {
                apply_tui_glow(frame, canvas, console, elapsed, 1.0);
                apply_console_top_reflection(frame, console, elapsed, 1.0);
                apply_console_background(frame, console, elapsed, 1.0);
            })
            .expect("render external console reflection");

        let reflection_glow = glow_color_at(
            elapsed,
            console.x,
            canvas.width,
            reflection_y,
            canvas,
            console,
            1.0,
        )
        .expect("reflection row glow");
        let accent_only_reflection = blend_rgb(
            reflection_glow,
            glow_accent_at(elapsed),
            CONSOLE_REFLECTION_TINT,
        );
        let previous_reflection =
            blend_rgb(accent_only_reflection, Color::Rgb(255, 255, 255), 0.10);
        let reflection = console_top_reflection_color_at(elapsed, reflection_glow, 1.0);
        let previous_luminance_lift =
            color_luminance(previous_reflection) - color_luminance(accent_only_reflection);
        let luminance_lift = color_luminance(reflection) - color_luminance(accent_only_reflection);
        assert!(
            luminance_lift * 100 >= previous_luminance_lift * 195
                && luminance_lift * 100 <= previous_luminance_lift * 210,
            "doubling the white tint doubles its reflection luminance contribution"
        );
        let console_glow = glow_color_at(
            elapsed,
            console.x,
            canvas.width,
            console.y,
            canvas,
            console,
            1.0,
        )
        .expect("console glow");
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(console.x, reflection_y)].symbol(),
            CONSOLE_REFLECTION_GLYPH,
            "the reflection is drawn in the row above the console"
        );
        assert_eq!(
            buffer[(console.x, reflection_y)].fg,
            console_top_reflection_color_at(elapsed, reflection_glow, 1.0),
            "the thin reflection follows the glow accent"
        );
        assert_eq!(
            buffer[(console.x, reflection_y - 1)].symbol(),
            " ",
            "only the closest external row is changed"
        );
        assert_eq!(
            buffer[(console.x, console.y)].bg,
            console_glass_color_at(elapsed, console_glow, 1.0),
            "the console top row remains normal glass"
        );

        let mut idle_terminal = Terminal::new(ratatui::backend::TestBackend::new(
            canvas.width,
            canvas.height,
        ))
        .expect("idle test terminal");
        idle_terminal
            .draw(|frame| {
                apply_tui_glow(frame, canvas, console, elapsed, 0.0);
                apply_console_top_reflection(frame, console, elapsed, 0.0);
                apply_console_background(frame, console, elapsed, 0.0);
            })
            .expect("render idle console");
        assert_eq!(
            idle_terminal.backend().buffer()[(console.x, reflection_y)].symbol(),
            " ",
            "idle consoles have no external reflection"
        );

        let state = UiState::from_history(&[], "secret", "model", None, false);
        let viewport = tui_viewport(Rect::new(0, 0, 80, 10));
        let (chat, _, _, _, input_area, _) = ui_layout(&state, viewport);
        assert_eq!(
            chat.y + chat.height + 1,
            input_area.y,
            "the reflection uses the existing transcript gap rather than adding a row"
        );
    }

    #[test]
    fn console_reflection_skips_transcript_when_no_gap_fits() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.welcome_visible = false;
        state
            .transcript
            .push(TranscriptItem::Info("protected transcript".to_owned()));
        state.busy = true;
        state.activity_transition = None;
        state.console_animation_epoch = Instant::now() - CONSOLE_BOUNDARY_CYCLE / 4;
        let area = Rect::new(0, 0, 60, 7);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw constrained active console");

        let (chat, _, _, _, console, _) = ui_layout(&state, tui_viewport(area));
        assert_eq!(chat.y + chat.height, console.y, "there is no separator row");
        assert_eq!(
            terminal.backend().buffer()[(chat.x, chat.y)].symbol(),
            "p",
            "the active reflection does not overwrite the adjacent transcript"
        );
    }

    #[test]
    fn console_reflection_stays_behind_an_active_skill_picker() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["agent-browser".to_owned()]);
        state.input = "/".to_owned();
        state.input_changed();
        state.busy = true;
        state.activity_transition = None;
        state.console_animation_epoch = Instant::now() - CONSOLE_BOUNDARY_CYCLE / 4;
        let area = Rect::new(0, 0, 40, 12);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw active picker");

        let (_, picker, _, _, input, _) = ui_layout(&state, tui_viewport(area));
        let picker = picker.expect("visible picker");
        assert_eq!(picker.y + picker.height, input.y);
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(picker.x, picker.y + picker.height - 1)].bg,
            SKILL_PICKER_BACKGROUND,
            "the picker covers the reflection row"
        );
        assert_ne!(
            buffer[(picker.x, picker.y + picker.height - 1)].symbol(),
            CONSOLE_REFLECTION_GLYPH,
            "the reflection glyph does not show through the picker"
        );
    }

    #[test]
    fn console_glass_white_film_brightens_only_visible_glass() {
        let elapsed = Duration::ZERO;
        let glow = Color::Rgb(80, 82, 86);
        let (red, green, blue) = activity_rgb(console_accent_at(elapsed));
        let neutral = ((u16::from(red) + u16::from(green) + u16::from(blue)) / 3) as u8;
        let glass_accent = Color::Rgb(
            interpolate_color(red, neutral, CONSOLE_GLASS_DESATURATION),
            interpolate_color(green, neutral, CONSOLE_GLASS_DESATURATION),
            interpolate_color(blue, neutral, CONSOLE_GLASS_DESATURATION),
        );
        let accent_tinted = blend_rgb(CONSOLE_BACKGROUND, glass_accent, CONSOLE_GLASS_TINT);
        let without_white_film = blend_rgb(accent_tinted, glow, CONSOLE_GLASS_GLOW_THROUGH);
        let with_white_film = console_glass_color_at(elapsed, glow, 1.0);
        let expected = blend_rgb(
            blend_rgb(
                accent_tinted,
                Color::Rgb(255, 255, 255),
                CONSOLE_GLASS_WHITE_TINT,
            ),
            glow,
            CONSOLE_GLASS_GLOW_THROUGH,
        );

        assert_eq!(CONSOLE_GLASS_WHITE_TINT, 0.03);
        assert_eq!(
            console_glass_color_at(elapsed, glow, 0.0),
            CONSOLE_BACKGROUND,
            "idle glass uses the configured console background"
        );
        assert_eq!(
            with_white_film, expected,
            "the white film is composited before glow-through"
        );
        let (with_white_red, with_white_green, with_white_blue) = activity_rgb(with_white_film);
        let (without_white_red, without_white_green, without_white_blue) =
            activity_rgb(without_white_film);
        assert!(
            u16::from(with_white_red) + u16::from(with_white_green) + u16::from(with_white_blue)
                > u16::from(without_white_red)
                    + u16::from(without_white_green)
                    + u16::from(without_white_blue),
            "the active glass has a visible white film"
        );
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
            "the glow transitions continuously instead of holding at lavender"
        );
        assert_ne!(
            midpoint,
            console_accent_at(console_accent_cycle() / 2),
            "the glow transitions continuously instead of holding at teal"
        );
    }

    #[test]
    fn tui_glow_background_is_ghostty_base_101216() {
        assert_eq!(TUI_GLOW_BACKGROUND_RGB, (16, 18, 22));
        assert_eq!(TUI_GLOW_BACKGROUND, Color::Rgb(16, 18, 22));
    }

    #[test]
    fn console_palette_starts_lavender_with_fifteen_percent_desaturation() {
        assert_eq!(CONSOLE_BACKGROUND_RGB, (42, 42, 46));
        assert_eq!(CONSOLE_BACKGROUND, Color::Rgb(42, 42, 46));
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
    fn floating_panel_backgrounds_are_neutral_gray_and_darker_than_the_console() {
        assert_eq!(FLOATING_PANEL_BACKGROUND, Color::Rgb(28, 28, 30));
        assert_eq!(SKILL_PICKER_BACKGROUND, FLOATING_PANEL_BACKGROUND);
        assert_eq!(SUBAGENT_OVERLAY_BACKGROUND, FLOATING_PANEL_BACKGROUND);
    }

    #[test]
    fn console_accent_transition_changes_all_tui_glow_in_sync() {
        let elapsed = console_accent_cycle() / 4;
        let canvas = Rect::new(0, 0, 80, 20);
        let console = Rect::new(20, 12, 40, 7);
        let row = canvas.y + canvas.height - 1;
        let left = glow_color_at(elapsed, console.x, canvas.width, row, canvas, console, 1.0)
            .expect("left-side glow");
        let right = glow_color_at(
            elapsed,
            console.x + console.width - 1,
            canvas.width,
            row,
            canvas,
            console,
            1.0,
        )
        .expect("right-side glow");
        let intensity = (console_reach_at(elapsed) / CONSOLE_REACH_MAX)
            * GLOW_INTENSITY
            * glow_coverage_at(console.x, row, canvas, console);

        assert_eq!(
            blend_rgb(TUI_GLOW_BACKGROUND, glow_accent_at(elapsed), intensity),
            left,
            "the left edge uses the shared accent phase"
        );
        assert_eq!(left, right, "the glow has no horizontal color phase");

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(
            canvas.width,
            canvas.height,
        ))
        .expect("test terminal");
        terminal
            .draw(|frame| {
                apply_tui_glow(frame, canvas, console, elapsed, 1.0);
                apply_console_top_reflection(frame, console, elapsed, 1.0);
                apply_console_background(frame, console, elapsed, 1.0);
            })
            .expect("render synchronized TUI glow");
        let buffer = terminal.backend().buffer();
        let left_x = console.x;
        let right_x = console.x + console.width - 1;
        assert_eq!(
            buffer[(left_x, console.y)].bg,
            buffer[(right_x, console.y)].bg,
            "the glass uses the shared accent phase"
        );
        assert_eq!(
            buffer[(left_x, console.y - 1)].fg,
            buffer[(right_x, console.y - 1)].fg,
            "the reflection uses the shared accent phase"
        );
    }

    #[test]
    fn borderless_console_contains_queue_running_subagent_prompt_and_statusline() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.queue_user("first task");
        state.queue_user("second task");
        state.subagents.push(SubagentTask {
            call_id: "call-worker".to_owned(),
            task_id: Some("subagent-1".to_owned()),
            task: "Inspect the command UI".to_owned(),
            model: Some("worker-model".to_owned()),
            effort: Some("high".to_owned()),
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: Vec::new(),
            stream_chars: 0,
        });
        state.input = "prompt text".to_owned();
        state.cursor = state.input.chars().count();
        let area = Rect::new(0, 0, 80, 14);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw bottom console");
        let (_, _, _, queue_area, input_area, status_area) = ui_layout(&state, tui_viewport(area));
        let queue_area = queue_area.expect("message queue area");
        let list_area = subagent_list_area(&state, input_area).expect("subagent list area");
        let prompt_area = prompt_area(input_area, &state);
        assert_eq!(
            queue_area.height, 3,
            "the queue header precedes each message"
        );
        assert_eq!(
            list_area.height, 2,
            "the subagent header precedes each worker"
        );
        assert_eq!(queue_area.y, input_area.y + 1);
        let (queue_spacer, list_spacer, status_spacer) = console_spacer_rows(&state, input_area);
        let queue_spacer = queue_spacer.expect("queue/prompt spacer");
        let list_spacer = list_spacer.expect("prompt/list spacer");
        let status_spacer = status_spacer.expect("list/status spacer");
        assert_eq!(queue_area.y + queue_area.height, queue_spacer);
        assert_eq!(prompt_area.y, queue_spacer + 1);
        assert_eq!(prompt_area.y + prompt_area.height, list_spacer);
        assert_eq!(list_area.y, list_spacer + 1);
        assert_eq!(list_area.y + list_area.height, status_spacer);
        assert_eq!(status_area.y, status_spacer + 1);
        assert_eq!(status_area.y + 1, input_area.y + input_area.height - 1);
        for area in [queue_area, list_area, status_area] {
            assert_eq!(area.x, input_area.x + 2);
            assert_eq!(area.width, input_area.width.saturating_sub(4));
        }
        assert_eq!(prompt_area.x, input_area.x + 2);
        assert_eq!(prompt_area.width, input_area.width.saturating_sub(4));

        let buffer = terminal.backend().buffer();
        let border_glyphs = ["┌", "┐", "└", "┘", "├", "┤", "─"];
        for y in input_area.y..input_area.y + input_area.height {
            for x in input_area.x..input_area.x + input_area.width {
                assert!(
                    !border_glyphs.contains(&buffer[(x, y)].symbol()),
                    "console contains a border glyph at ({x}, {y})"
                );
                assert_eq!(buffer[(x, y)].bg, CONSOLE_BACKGROUND);
            }
        }

        for y in [
            input_area.y,
            queue_spacer,
            list_spacer,
            status_spacer,
            input_area.y + input_area.height - 1,
        ] {
            for x in input_area.x..input_area.x + input_area.width {
                assert_eq!(buffer[(x, y)].symbol(), " ");
            }
        }
        for (x, y) in [
            (input_area.x, input_area.y),
            (input_area.x + input_area.width - 1, input_area.y),
            (input_area.x, input_area.y + input_area.height - 1),
            (
                input_area.x + input_area.width - 1,
                input_area.y + input_area.height - 1,
            ),
        ] {
            assert_eq!(buffer[(x, y)].symbol(), " ");
            assert_eq!(buffer[(x, y)].bg, CONSOLE_BACKGROUND);
        }

        let queued_rows = (queue_area.y..queue_area.y + queue_area.height)
            .map(|y| {
                (queue_area.x..queue_area.x + queue_area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(queued_rows[0].starts_with("Queued"));
        assert!(queued_rows[1].contains("│ 1) first task"));
        assert!(queued_rows[2].contains("│ 2) second task"));
        assert!(!queued_rows[1].contains("Queued 1"));
        assert!(!queued_rows[2].contains("Queued 2"));
        assert_eq!(
            buffer[(queue_area.x, queue_area.y)].fg,
            SECTION_CHROME_COLOR
        );
        assert_eq!(
            buffer[(queue_area.x, queue_area.y + 1)].fg,
            SECTION_CHROME_COLOR
        );
        assert_eq!(
            buffer[(queue_area.x + 2, queue_area.y + 1)].fg,
            QUEUED_MESSAGE_COLOR
        );
        assert_eq!(buffer[(list_area.x, list_area.y)].fg, SUBAGENT_TITLE_COLOR);
        assert_eq!(
            buffer[(list_area.x, list_area.y + 1)].fg,
            SUBAGENT_TITLE_COLOR
        );
        assert_eq!(
            buffer[(list_area.x + 2, list_area.y + 1)].fg,
            subagent_id_color("subagent-1")
        );
        assert_eq!(
            buffer[(status_area.x, status_area.y)].fg,
            CONSOLE_STATUS_COLOR
        );
        let status_row = (status_area.x..status_area.x + status_area.width)
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert!(status_row.starts_with("model · default | Context: "));
        terminal.backend_mut().assert_cursor_position((
            prompt_area.x + UnicodeWidthStr::width(state.input.as_str()) as u16,
            prompt_area.y,
        ));
    }

    #[test]
    fn prompt_uses_two_cells_of_horizontal_console_padding() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
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
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
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
    fn constrained_console_keeps_internal_rects_inside_without_borders() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.queue_user("first task");
        state.queue_user("second task");
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

        for height in 3..=9 {
            let area = Rect::new(0, 0, 60, height);
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                    .expect("test terminal");
            terminal
                .draw(|frame| draw(frame, &state))
                .expect("draw constrained console");

            let (_, _, _, queue, console, status) = ui_layout(&state, tui_viewport(area));
            let content = console_content_area(console);
            let prompt = prompt_area(console, &state);
            let list = subagent_list_area(&state, console);
            for child in queue.into_iter().chain(list).chain([prompt, status]) {
                assert!(
                    child.x >= content.x,
                    "height {height}: {child:?} starts left of {content:?}"
                );
                assert!(
                    child.y >= content.y,
                    "height {height}: {child:?} starts above {content:?}"
                );
                assert!(
                    child.x + child.width <= content.x + content.width,
                    "height {height}: {child:?} ends right of {content:?}"
                );
                assert!(
                    child.y + child.height <= content.y + content.height,
                    "height {height}: {child:?} ends below {content:?}"
                );
            }

            let buffer = terminal.backend().buffer();
            let border_glyphs = ["┌", "┐", "└", "┘", "├", "┤", "─", "│"];
            for y in console.y..console.y + console.height {
                assert!(!border_glyphs.contains(&buffer[(console.x, y)].symbol()));
                assert!(
                    !border_glyphs.contains(&buffer[(console.x + console.width - 1, y)].symbol())
                );
                let expected_background = CONSOLE_BACKGROUND;
                assert_eq!(buffer[(console.x, y)].bg, expected_background);
                assert_eq!(
                    buffer[(console.x + console.width - 1, y)].bg,
                    expected_background
                );
            }
            assert_eq!(
                status.y + status.height,
                console.y + console.height.saturating_sub(1)
            );
            let spacers = console_spacer_rows(&state, console);
            assert_eq!(spacers.0.is_some(), queue.is_some());
            assert_eq!(spacers.1.is_some(), list.is_some());
            for y in spacers.0.into_iter().chain(spacers.1).chain(spacers.2) {
                for x in console.x..console.x + console.width {
                    assert_eq!(buffer[(x, y)].symbol(), " ");
                }
            }
        }
    }

    #[test]
    fn constrained_console_hides_sections_that_cannot_fit_a_header_and_entry() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.queue_user("queued");
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

        let cramped = bottom_content_heights(&state, Rect::new(0, 0, 80, 7));
        assert_eq!(cramped.queue, 0);
        assert_eq!(cramped.queue_separator, 0);
        assert_eq!(cramped.list, 0);
        assert_eq!(cramped.list_separator, 0);

        let queue_only = bottom_content_heights(&state, Rect::new(0, 0, 80, 8));
        assert_eq!(queue_only.queue, 2);
        assert_eq!(queue_only.queue_separator, 1);
        assert_eq!(queue_only.list, 0);
    }

    #[test]
    fn constrained_multiline_prompt_scrolls_to_its_last_row_and_keeps_cursor_inside() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.queue_user("first task");
        state.queue_user("second task");
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
        state.input = "first\nsecond\nthird".to_owned();
        state.cursor = state.input.chars().count();
        let area = Rect::new(0, 0, 40, 6);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw constrained multiline prompt");

        let outer = ui_layout(&state, tui_viewport(area)).4;
        let prompt = prompt_area(outer, &state);
        assert_eq!(prompt.height, 2);
        let rendered = (prompt.y..prompt.y + prompt.height)
            .map(|y| {
                (prompt.x..prompt.x + prompt.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rendered[0].starts_with("second"));
        assert!(rendered[1].starts_with("third"));
        terminal.backend_mut().assert_cursor_position((
            prompt.x + UnicodeWidthStr::width("third") as u16,
            prompt.y + prompt.height - 1,
        ));
    }

    #[test]
    fn constrained_picker_and_worker_overlay_remain_above_console() {
        let mut picker_state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["release-notes".to_owned()]);
        picker_state.input = "/".to_owned();
        picker_state.input_changed();
        for height in [3, 5] {
            let area = Rect::new(0, 0, 60, height);
            let (_, picker, _, _, outer, _) = ui_layout(&picker_state, tui_viewport(area));
            if let Some(picker) = picker {
                assert!(picker.y >= area.y);
                assert!(picker.y + picker.height <= outer.y);
            }
        }

        let mut worker_state = UiState::from_history(&[], "secret", "model", None, false);
        worker_state.subagents.push(SubagentTask {
            call_id: "call-worker".to_owned(),
            task_id: Some("subagent-1".to_owned()),
            task: "Inspect".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: vec![SubagentStreamItem::Assistant("worker output".to_owned())],
            stream_chars: 13,
        });
        assert!(worker_state.focus_subagent_list_from_input());
        for height in [3, 5] {
            let area = Rect::new(0, 0, 60, height);
            let (_, _, overlay, _, outer, _) = ui_layout(&worker_state, tui_viewport(area));
            if let Some(overlay) = overlay {
                assert!(overlay.y >= area.y);
                assert!(overlay.y + overlay.height <= outer.y);
            }

            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                    .expect("test terminal");
            terminal
                .draw(|frame| draw(frame, &worker_state))
                .expect("draw constrained worker overlay");
            assert_eq!(
                terminal.backend().buffer()[(outer.x, outer.y)].symbol(),
                " ",
                "height {height}"
            );
            assert_eq!(
                terminal.backend().buffer()[(outer.x, outer.y)].bg,
                CONSOLE_BACKGROUND,
                "height {height}"
            );
        }
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
        assert_eq!(picker_area.y + picker_area.height, input_area.y);
        assert_eq!(queue_area.y, input_area.y + 1);
        assert_eq!(queue_area.x, input_area.x + 2);
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

        let state = UiState::from_history(&[], "secret", "model", None, false);
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

        let state = UiState::from_history(&[], "secret", "model", None, false);
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
        let state = UiState::from_history(&[], "secret", "model", None, false);
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
        let version_x = chat_area.x + (chat_area.width - version_width) / 2;
        let version_y = chat_area.y + title_row as u16 + 1;
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
                    before_red.abs_diff(after_red) <= 45
                        && before_green.abs_diff(after_green) <= 45
                        && before_blue.abs_diff(after_blue) <= 45
                }));
                assert_eq!(frames.last(), Some(&target));
            }
        }
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
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
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
    fn main_agent_status_omits_activity_animation_on_idle_and_busy_glass() {
        let mut state =
            UiState::from_history(&[], "secret", "model", None, false).with_context(Some(100), 81);
        let area = Rect::new(0, 0, 80, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw ready status");
        let viewport = tui_viewport(area);
        let status_area = ui_layout(&state, viewport).5;
        let idle = "model · default | Context: 81/100 (81%) █████████░";
        let buffer = terminal.backend().buffer();
        let idle_columns = status_area.x..status_area.x + idle.chars().count() as u16;
        let idle_row = idle_columns
            .clone()
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert_eq!(idle_row, idle);
        for x in idle_columns {
            assert_eq!(buffer[(x, status_area.y)].fg, Color::Rgb(144, 144, 148));
        }

        state.set_status("working");
        state.busy = true;
        state.activity_transition = None;
        state.console_animation_epoch = Instant::now() - CONSOLE_BOUNDARY_CYCLE / 4;
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw working status");
        let status_area = ui_layout(&state, viewport).5;
        let expected = "model · default | Context: 81/100 (81%) █████████░";
        let buffer = terminal.backend().buffer();
        let status_columns = status_area.x..status_area.x + expected.chars().count() as u16;
        let rendered = status_columns
            .clone()
            .map(|x| buffer[(x, status_area.y)].symbol())
            .collect::<String>();
        assert_eq!(rendered, expected);
        assert!(
            status_columns
                .clone()
                .any(|x| buffer[(x, status_area.y)].bg != CONSOLE_BACKGROUND),
            "the busy status line renders over bright glass"
        );
        for x in status_columns {
            assert_eq!(buffer[(x, status_area.y)].fg, Color::Rgb(144, 144, 148));
        }
    }

    #[test]
    fn terminal_focus_events_control_cursor_visibility() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);

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
    fn unfocused_busy_glow_keeps_the_hardware_cursor_hidden() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.set_status("working");
        state.set_busy(true);
        state.terminal_focused = false;
        state.console_animation_epoch = Instant::now() - CONSOLE_BOUNDARY_CYCLE / 4;

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 10)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw busy glow");

        assert!(
            !terminal.backend().cursor_visible(),
            "the glow redraw must not re-show the terminal cursor"
        );
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
        let prompt_area = prompt_area(input_area, &state);
        assert!(terminal.backend().cursor_visible());
        terminal.backend_mut().assert_cursor_position((
            prompt_area.x + UnicodeWidthStr::width(state.input.as_str()) as u16,
            prompt_area.y,
        ));
    }

    #[test]
    fn transcript_and_console_are_separated_by_one_blank_row() {
        let state = UiState::from_history(&[], "secret", "model", None, false);
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
    fn idle_console_has_external_gutters_uniform_background_and_no_borders() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.input = "prompt".to_owned();
        state.cursor = state.input.chars().count();
        let area = Rect::new(0, 0, 80, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw idle console");

        let input_area = ui_layout(&state, tui_viewport(area)).4;
        let prompt_area = prompt_area(input_area, &state);
        let buffer = terminal.backend().buffer();
        let bottom_y = area.y + area.height - 1;
        assert_eq!(buffer[(area.x, bottom_y)].bg, Color::Reset);
        assert_eq!(buffer[(area.x + area.width - 1, bottom_y)].bg, Color::Reset);
        for y in input_area.y..input_area.y + input_area.height {
            assert_eq!(buffer[(0, y)].bg, Color::Reset);
            assert_eq!(buffer[(79, y)].bg, Color::Reset);
            for x in input_area.x..input_area.x + input_area.width {
                assert_eq!(buffer[(x, y)].bg, CONSOLE_BACKGROUND);
            }
        }
        assert_eq!(buffer[(input_area.x, input_area.y)].symbol(), " ");
        assert_eq!(
            buffer[(input_area.x + input_area.width - 1, input_area.y)].symbol(),
            " "
        );
        assert_eq!(
            buffer[(input_area.x, input_area.y + input_area.height - 1)].symbol(),
            " "
        );
        assert_eq!(
            buffer[(
                input_area.x + input_area.width - 1,
                input_area.y + input_area.height - 1
            )]
                .symbol(),
            " "
        );
        assert_eq!(buffer[(prompt_area.x, prompt_area.y)].symbol(), "p");
        assert_eq!(buffer[(prompt_area.x, prompt_area.y)].fg, Color::White);
        terminal.backend_mut().assert_cursor_position((
            prompt_area.x + UnicodeWidthStr::width(state.input.as_str()) as u16,
            prompt_area.y,
        ));
    }

    #[test]
    fn busy_console_keeps_glass_inside_the_bottom_half_ellipse() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.busy = true;
        state.set_status("working");
        state.activity_transition = None;
        state.console_animation_epoch = Instant::now() - CONSOLE_BOUNDARY_CYCLE / 4;
        let area = Rect::new(0, 0, 200, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw busy console");

        let viewport = tui_viewport(area);
        let (chat_area, _, _, _, input_area, _) = ui_layout(&state, viewport);
        let reflection_y = input_area.y - 1;
        let glow_floor_y = area.y + area.height - 1;
        let edge_y = input_area.y + input_area.height - 1;
        let floor = input_area.x + input_area.width / 2;
        let left_edge = input_area.x;
        let left_outer = left_edge - 1;
        let right_edge = input_area.x + input_area.width - 1;
        let right_outer = right_edge + 1;
        let widening_sample = input_area.x - 22;
        let buffer = terminal.backend().buffer();
        assert_eq!(chat_area.y + chat_area.height + 1, input_area.y);
        assert_eq!(
            buffer[(floor, reflection_y)].symbol(),
            CONSOLE_REFLECTION_GLYPH,
            "the active console reflects along the bottom of the existing gap row"
        );
        assert_ne!(buffer[(floor, reflection_y)].fg, Color::Reset);
        assert_ne!(buffer[(floor, glow_floor_y)].bg, Color::Reset);
        assert_ne!(buffer[(left_outer, edge_y)].bg, Color::Reset);
        assert_ne!(buffer[(right_outer, edge_y)].bg, Color::Reset);
        let left_extent = left_edge - GLOW_HORIZONTAL_SPREAD;
        let right_extent = right_edge + GLOW_HORIZONTAL_SPREAD;
        assert_ne!(buffer[(left_extent, glow_floor_y)].bg, Color::Reset);
        assert_ne!(buffer[(right_extent, glow_floor_y)].bg, Color::Reset);
        assert_eq!(buffer[(left_extent - 1, glow_floor_y)].bg, Color::Reset);
        assert_eq!(buffer[(right_extent + 1, glow_floor_y)].bg, Color::Reset);
        assert_eq!(buffer[(widening_sample, input_area.y)].bg, Color::Reset);
        assert_ne!(buffer[(widening_sample, glow_floor_y)].bg, Color::Reset);
        assert_eq!(buffer[(area.x, glow_floor_y)].bg, Color::Reset);
        for y in input_area.y..input_area.y + input_area.height {
            for x in input_area.x..input_area.x + input_area.width {
                assert_ne!(buffer[(x, y)].bg, Color::Reset);
            }
        }
        for y in input_area.y..input_area.y + input_area.height {
            for x in input_area.x..input_area.x + input_area.width {
                assert_ne!(buffer[(x, y)].bg, Color::Reset);
                assert!(
                    color_distance(CONSOLE_BACKGROUND, buffer[(x, y)].bg)
                        < color_distance(TUI_GLOW_BACKGROUND, buffer[(floor, glow_floor_y)].bg)
                );
            }
        }
    }

    #[test]
    fn narrow_busy_console_keeps_both_endpoint_blooms_exposed() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.busy = true;
        state.set_status("working");
        state.activity_transition = None;
        state.console_animation_epoch = Instant::now() - CONSOLE_BOUNDARY_CYCLE / 4;
        let area = Rect::new(0, 0, 10, 10);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw narrow TUI");

        let input = ui_layout(&state, tui_viewport(area)).4;
        assert_eq!(input.width, 4, "test the minimum inset console width");
        let left_edge = input.x;
        let right_edge = input.x + input.width - 1;
        let buffer = terminal.backend().buffer();
        for y in input.y..input.y + input.height {
            for (edge, outer) in [(left_edge, left_edge - 1), (right_edge, right_edge + 1)] {
                assert_ne!(
                    buffer[(outer, y)].bg,
                    Color::Reset,
                    "the outer glow cell is present at ({outer}, {y})"
                );
                assert_ne!(
                    buffer[(edge, y)].bg,
                    buffer[(outer, y)].bg,
                    "narrow console glass does not spill into the exposed bloom at ({edge}, {y})"
                );
            }
        }
    }

    #[test]
    fn busy_queue_and_subagent_rows_preserve_the_console_glow_surface() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.busy = true;
        state.console_animation_epoch = Instant::now() - CONSOLE_BOUNDARY_CYCLE / 4;
        state.queue_user("send later");
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
        let area = Rect::new(0, 0, 80, 18);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                .expect("test terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw busy TUI");

        let viewport = tui_viewport(area);
        let (_, _, _, queue, input, _) = ui_layout(&state, viewport);
        let list = subagent_list_area(&state, input).expect("subagent list");
        let elapsed = state.console_animation_elapsed_at(Instant::now());
        let visibility = state.console_visibility_at(Instant::now());
        let buffer = terminal.backend().buffer();
        for section in [queue.expect("message queue"), list] {
            let x = section.x + section.width / 2;
            let y = section.y;
            let glow = glow_color_at(elapsed, x, area.width, y, area, input, visibility)
                .unwrap_or(TUI_GLOW_BACKGROUND);
            let expected = console_glass_color_at(elapsed, glow, visibility);
            assert!(
                color_distance(buffer[(x, y)].bg, expected) <= 3,
                "section ({x}, {y}) must retain the same glass surface as the console"
            );
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
                            "task": "Inspect the command UI"
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
        let mut state =
            UiState::from_history(&history, "secret", "worker-model", Some("high"), false);
        assert_eq!(state.subagents.len(), 1);
        let task = &state.subagents[0];
        assert_eq!(task.task, "Inspect the command UI");
        assert_eq!(task.task_id.as_deref(), Some("subagent-1"));
        assert_eq!(task.model.as_deref(), Some("worker-model"));
        assert_eq!(task.effort.as_deref(), Some("high"));
        assert_eq!(task.status, SubagentStatus::Running);
        assert!(state.transcript.is_empty());

        state.apply_event(ProtocolEvent::BackgroundResultPending {
            completion_id: "completion-1".to_owned(),
            task_id: "subagent-1".to_owned(),
            child_session_id: "child-1".to_owned(),
            status: "completed".to_owned(),
            result: serde_json::json!({"model":"worker-model","output":"finished"}),
            completed_at: 1,
        });
        state.apply_event(ProtocolEvent::BackgroundResultDelivered {
            completion_id: "completion-1".to_owned(),
            task_id: "subagent-1".to_owned(),
            logical_turn_id: "turn-1".to_owned(),
            delivery: "synthetic".to_owned(),
        });
        assert!(
            state.subagents.is_empty(),
            "completed workers are removed from the live background-task list"
        );
        let rendered = transcript_lines(&state, 100)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| {
            line.contains("subagent-1")
                && line.contains("completed")
                && line.contains("result pending")
        }));

        assert!(rendered
            .iter()
            .any(|line| { line.contains("subagent-1") && line.contains("result delivered") }));
        assert_eq!(
            state
                .transcript
                .iter()
                .filter(|item| matches!(item, TranscriptItem::SubagentLifecycle { .. }))
                .count(),
            2,
            "pending and delivered are separate transcript transitions"
        );
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
            SessionHistoryRecord::BackgroundResultPending(
                crate::session::BackgroundResultPending {
                    timestamp: 3,
                    completion_id: "completion-1".to_owned(),
                    task_id: "subagent-1".to_owned(),
                    child_session_id: "child-1".to_owned(),
                    task: "Inspect".to_owned(),
                    status: crate::session::ChildSessionStatus::Completed,
                    result: serde_json::json!({"output":"resumed result"}),
                    completed_at: 3,
                },
            ),
        ];
        let state = UiState::from_history(&history, "secret", "model", None, true);
        assert!(
            state.subagents.is_empty(),
            "a completed worker is not restored into the live background-task list"
        );
        assert!(!state.transcript.iter().any(|item| {
            matches!(item, TranscriptItem::User { text, .. } if text.contains("Background subagent"))
        }));
        assert!(state.transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::SubagentLifecycle { task_id, status, .. }
                    if task_id == "subagent-1" && status == "completed"
            )
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
        assert_eq!(prompt.y, input.y + 1);
        let (queue_spacer, list_spacer, status_spacer) = console_spacer_rows(&state, input);
        assert_eq!(queue_spacer, None);
        assert_eq!(list.height, 2);
        assert_eq!(list_spacer, Some(prompt.y + prompt.height));
        assert_eq!(list.y, list_spacer.expect("list spacer") + 1);
        assert_eq!(status_spacer, Some(list.y + list.height));
        assert_eq!(status.y, status_spacer.expect("status spacer") + 1);
        assert_eq!(status.y + 2, input.y + input.height);

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
        assert!(screen.contains("Subagents"));
        assert!(screen.contains("│ subagent-1"));
        assert!(!screen.contains("worker-model"));
        assert!(!screen.contains("high"));
        assert!(screen.contains("Inspect the command UI"));
    }

    #[test]
    fn queued_and_terminal_subagents_do_not_appear_in_the_running_list() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.subagents.push(SubagentTask {
            call_id: "call-queued".to_owned(),
            task_id: None,
            task: "Queued".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Queued,
            result: None,
            creation_completed: false,
            stream: Vec::new(),
            stream_chars: 0,
        });
        state.subagents.push(SubagentTask {
            call_id: "call-failed".to_owned(),
            task_id: None,
            task: "Failed".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Failed,
            result: None,
            creation_completed: false,
            stream: Vec::new(),
            stream_chars: 0,
        });
        assert_eq!(subagent_list_height(&state), 0);
        assert!(subagent_list_area(&state, Rect::new(0, 0, 80, 10)).is_none());
        assert!(!state.focus_subagent_list_from_input());
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
        assert!(SUBAGENT_ID_COLORS.iter().all(|color| {
            matches!(color, Color::Rgb(220, green, blue)
                if u16::from(*green) < u16::from(*blue)
                    && u16::from(*blue) < 220
                    && 4 * (u16::from(*blue) - u16::from(*green))
                        == 3 * (220 - u16::from(*green)))
        }));
        assert!(SUBAGENT_ID_COLORS
            .windows(2)
            .all(|colors| colors[0] != colors[1]));

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
            .draw(|frame| draw_subagent_list(frame, &state, Rect::new(0, 0, 80, 2)))
            .expect("draw subagent list");
        assert_eq!(terminal.backend().buffer()[(0, 0)].fg, SUBAGENT_TITLE_COLOR);
        assert_eq!(terminal.backend().buffer()[(0, 1)].fg, SUBAGENT_TITLE_COLOR);
        assert_eq!(
            terminal.backend().buffer()[(2, 1)].fg,
            subagent_id_color("subagent-1")
        );
        assert_eq!(terminal.backend().buffer()[(0, 1)].bg, Color::Reset);
    }

    #[test]
    fn clipped_subagent_list_keeps_the_focused_running_worker_visible() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        for (index, status) in [
            SubagentStatus::Queued,
            SubagentStatus::Running,
            SubagentStatus::Running,
            SubagentStatus::Failed,
            SubagentStatus::Running,
        ]
        .into_iter()
        .enumerate()
        {
            state.subagents.push(SubagentTask {
                call_id: format!("call-{index}"),
                task_id: Some(format!("subagent-{index}")),
                task: format!("task-{index}"),
                model: None,
                effort: None,
                status,
                result: None,
                creation_completed: status == SubagentStatus::Running,
                stream: vec![SubagentStreamItem::Assistant(format!("output-{index}"))],
                stream_chars: 8,
            });
        }
        state.subagent_focus = Some(4);

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(60, 4)).expect("test terminal");
        terminal
            .draw(|frame| draw_subagent_list(frame, &state, Rect::new(0, 0, 60, 2)))
            .expect("draw clipped subagent list");

        let buffer = terminal.backend().buffer();
        let rows = (0..2)
            .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(rows[0].contains("Subagents"));
        assert!(rows[1].contains("subagent-4"));
        assert!(buffer[(2, 1)].modifier.contains(Modifier::BOLD));

        terminal
            .draw(|frame| draw_subagent_stream_overlay(frame, &state, Rect::new(0, 0, 60, 3)))
            .expect("draw focused worker overlay");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("output-4"));
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
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: "worker output".to_owned(),
            },
        });
        assert!(state.focus_subagent_list_from_input());
        let area = tui_viewport(Rect::new(0, 0, 80, 30));
        let (_, picker, stream, _, input, _) = ui_layout(&state, area);
        let stream = stream.expect("stream overlay");
        assert_eq!(stream.height, SUBAGENT_STREAM_PREVIEW_HEIGHT);
        assert_eq!(stream.y + stream.height, input.y);
        assert!(
            picker.is_none(),
            "focused worker replaces the skill overlay slot"
        );

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 30)).expect("test terminal");
        terminal.draw(|frame| draw(frame, &state)).expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("worker output"));
        let buffer = terminal.backend().buffer();
        for y in stream.y..stream.y + stream.height {
            for x in [
                stream.x,
                stream.x + 1,
                stream.x + stream.width - 2,
                stream.x + stream.width - 1,
            ] {
                assert_eq!(buffer[(x, y)].symbol(), " ");
                assert_eq!(buffer[(x, y)].bg, SUBAGENT_OVERLAY_BACKGROUND);
            }
        }
        for x in stream.x..stream.x + stream.width {
            assert_eq!(buffer[(x, stream.y)].symbol(), " ");
            assert_eq!(buffer[(x, stream.y + stream.height - 1)].symbol(), " ");
        }
        assert_eq!(buffer[(stream.x + 2, stream.y + 1)].symbol(), "w");
        assert_eq!(buffer[(stream.x + 2, stream.y + 1)].fg, Color::Reset);

        let narrow_input = Rect::new(0, 4, 80, 2);
        assert_eq!(
            subagent_stream_overlay_area(&state, narrow_input, 0),
            Some(Rect::new(0, 0, 80, 4)),
            "only a terminal without 15 rows of space may shrink the preview"
        );
    }

    #[test]
    fn subagent_preview_reuses_normalized_events_and_keeps_the_message_stream() {
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

        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: "first message ".to_owned(),
            },
        });
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: "continued message".to_owned(),
            },
        });
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::ToolCall {
                id: "call-cmd".to_owned(),
                name: "cmd".to_owned(),
                arguments: serde_json::json!({"command":"pwd"}).to_string(),
            },
        });
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::ToolResult {
                id: "call-cmd".to_owned(),
                name: "cmd".to_owned(),
                result: serde_json::json!({"stdout":"command output","stderr":""}),
            },
        });

        let task = &state.subagents[0];
        let lines = subagent_stream_lines(task, 80, &state);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("first message continued message"));
        assert!(rendered.contains("cmd  $ pwd"));
        let expected = vec![
            TranscriptItem::Assistant("first message continued message".to_owned()),
            TranscriptItem::ToolCall {
                id: "call-cmd".to_owned(),
                name: "cmd".to_owned(),
                arguments: serde_json::json!({"command":"pwd"}).to_string(),
            },
            TranscriptItem::ToolResult {
                id: "call-cmd".to_owned(),
                name: "cmd".to_owned(),
                result: serde_json::json!({"stdout":"command output","stderr":""}),
            },
        ];
        assert_eq!(
            lines,
            render_transcript_items(&expected, 80, &state, true),
            "worker events use the same transcript-item renderer as the main stream"
        );
        assert_eq!(
            task.stream
                .iter()
                .filter(|item| matches!(item, SubagentStreamItem::Assistant(_)))
                .count(),
            1,
            "assistant deltas remain one message in the preview"
        );
    }

    #[test]
    fn oversized_subagent_assistant_stream_keeps_the_latest_tail() {
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
            stream: vec![SubagentStreamItem::User("Inspect".to_owned())],
            stream_chars: "Inspect".chars().count(),
        });
        let tail = "latest assistant output";
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: format!("{}{}", "x".repeat(SUBAGENT_STREAM_MAX_CHARS), tail),
            },
        });

        let task = &state.subagents[0];
        assert_eq!(task.stream_chars, SUBAGENT_STREAM_MAX_CHARS);
        assert!(matches!(
            task.stream.as_slice(),
            [SubagentStreamItem::Assistant(text)] if text.starts_with('…') && text.ends_with(tail)
        ));
        let rendered = subagent_stream_lines(task, 80, &state)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(tail));
    }

    #[test]
    fn subagent_stream_trimming_keeps_the_tail_across_items() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        let earlier = "a".repeat(6_000);
        let latest = "b".repeat(7_000);
        state.subagents.push(SubagentTask {
            call_id: "call-worker".to_owned(),
            task_id: Some("subagent-1".to_owned()),
            task: "Inspect".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: vec![SubagentStreamItem::User(earlier)],
            stream_chars: 6_000,
        });
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: latest.clone(),
            },
        });

        let task = &state.subagents[0];
        assert_eq!(task.stream_chars, SUBAGENT_STREAM_MAX_CHARS);
        assert!(matches!(
            task.stream.as_slice(),
            [SubagentStreamItem::User(prefix), SubagentStreamItem::Assistant(text)]
                if prefix.starts_with('…') && prefix.ends_with('a') && text == &latest
        ));
    }

    #[test]
    fn subagent_stream_trimming_marks_a_wholly_evicted_item() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        let latest = "b".repeat(SUBAGENT_STREAM_MAX_CHARS - 1);
        state.subagents.push(SubagentTask {
            call_id: "call-worker".to_owned(),
            task_id: Some("subagent-1".to_owned()),
            task: "Inspect".to_owned(),
            model: None,
            effort: None,
            status: SubagentStatus::Running,
            result: None,
            creation_completed: true,
            stream: vec![SubagentStreamItem::User("a".repeat(8))],
            stream_chars: 8,
        });
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: latest.clone(),
            },
        });

        let task = &state.subagents[0];
        assert_eq!(task.stream_chars, SUBAGENT_STREAM_MAX_CHARS);
        assert!(matches!(
            task.stream.as_slice(),
            [SubagentStreamItem::Assistant(marker), SubagentStreamItem::Assistant(text)]
                if marker == "…" && text == &latest
        ));
    }

    #[test]
    fn oversized_subagent_tool_result_keeps_structured_truncation_marker() {
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
            stream: vec![SubagentStreamItem::User("Inspect".to_owned())],
            stream_chars: "Inspect".chars().count(),
        });
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::ToolResult {
                id: "call-cmd".to_owned(),
                name: "cmd".to_owned(),
                result: serde_json::json!({
                    "stdout": "x".repeat(SUBAGENT_STREAM_MAX_CHARS),
                    "zz_raw_marker": "must stay structured"
                }),
            },
        });

        let task = &state.subagents[0];
        assert_eq!(task.stream_chars, 1);
        assert!(matches!(
            task.stream.as_slice(),
            [SubagentStreamItem::Assistant(text)] if text == "…"
        ));
        let rendered = subagent_stream_lines(task, 80, &state)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rendered, "…");
        assert!(!rendered.contains("zz_raw_marker"));
    }

    #[test]
    fn oversized_subagent_tool_call_keeps_structured_truncation_marker() {
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
            stream: vec![SubagentStreamItem::User("Inspect".to_owned())],
            stream_chars: "Inspect".chars().count(),
        });
        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::ToolCall {
                id: "call-cmd".to_owned(),
                name: "cmd".to_owned(),
                arguments: format!(
                    r#"{{"command":"{}","zz_raw_marker":"must stay structured"}}"#,
                    "x".repeat(SUBAGENT_STREAM_MAX_CHARS)
                ),
            },
        });

        let task = &state.subagents[0];
        assert_eq!(task.stream_chars, 1);
        assert!(matches!(
            task.stream.as_slice(),
            [SubagentStreamItem::Assistant(text)] if text == "…"
        ));
        let rendered = subagent_stream_lines(task, 80, &state)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rendered, "…");
        assert!(!rendered.contains("zz_raw_marker"));
    }

    #[test]
    fn empty_subagent_stream_shows_a_waiting_placeholder() {
        let state = UiState::from_history(&[], "secret", "model", None, false);
        let task = SubagentTask {
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
        };

        assert_eq!(
            subagent_stream_lines(&task, 80, &state)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            ["waiting for worker output"]
        );
    }

    #[test]
    fn subagent_preview_replays_activity_that_arrives_before_spawn_acknowledgement() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.add_tool_call(&crate::model::ChatToolCall {
            id: "call-worker".to_owned(),
            name: "spawn_subagent".to_owned(),
            arguments: serde_json::json!({"task":"Inspect"}).to_string(),
        });

        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: "early worker output".to_owned(),
            },
        });
        assert_eq!(state.pending_subagent_activities.len(), 1);

        state.add_live_tool_result(
            "call-worker",
            "spawn_subagent",
            serde_json::json!({"task_id":"subagent-1","status":"queued"}),
        );
        assert!(state.pending_subagent_activities.is_empty());
        let visible = subagent_stream_lines(&state.subagents[0], 80, &state)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("early worker output"));
    }

    #[test]
    fn subagent_preview_stays_pinned_to_the_latest_stream_lines() {
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

        for index in 0..10 {
            state.apply_subagent_activity(SubagentActivity::Event {
                task_id: "subagent-1".to_owned(),
                event: ProtocolEvent::ToolResult {
                    id: format!("call-{index}"),
                    name: "cmd".to_owned(),
                    result: serde_json::json!({"stdout": format!("output-{index}")}),
                },
            });
        }

        let visible = latest_subagent_stream_lines(&state.subagents[0], 80, &state);
        assert!(visible
            .iter()
            .any(|line| line_text(line).contains("output-0")));
        assert!(visible
            .last()
            .is_some_and(|line| line_text(line).contains("output-9")));

        state.apply_subagent_activity(SubagentActivity::Event {
            task_id: "subagent-1".to_owned(),
            event: ProtocolEvent::AssistantDelta {
                text: "newest live message".to_owned(),
            },
        });
        let visible = latest_subagent_stream_lines(&state.subagents[0], 80, &state);
        assert!(visible
            .last()
            .is_some_and(|line| line_text(line).contains("newest live message")));

        state.subagent_focus = Some(0);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 15)).expect("test terminal");
        terminal
            .draw(|frame| draw_subagent_stream_overlay(frame, &state, Rect::new(0, 0, 80, 15)))
            .expect("draw clipped worker overlay");
        let buffer = terminal.backend().buffer();
        let inner_rows = (1..14)
            .map(|y| (2..78).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(inner_rows
            .iter()
            .any(|row| row.contains("newest live message")));
        assert!(!inner_rows.iter().any(|row| row.contains("output-0")));
    }

    #[test]
    fn subagent_preview_shows_reasoning_state_and_initial_task_message() {
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
            stream: vec![SubagentStreamItem::User("Inspect".to_owned())],
            stream_chars: 7,
        });

        state.apply_subagent_activity(SubagentActivity::ReasoningStarted {
            task_id: "subagent-1".to_owned(),
        });
        let before = subagent_stream_lines(&state.subagents[0], 80, &state)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(before.contains("Inspect"));
        assert!(before.contains("Reasoning"));

        state.apply_subagent_activity(SubagentActivity::ReasoningCompleted {
            task_id: "subagent-1".to_owned(),
        });
        let after = subagent_stream_lines(&state.subagents[0], 80, &state)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(after.contains("Reasoning Complete"));
    }

    #[test]
    fn down_from_last_input_row_prioritizes_subagent_list_over_skill_picker() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
            .with_skill_names(vec!["settings".to_owned()]);
        state.input = "/".to_owned();
        state.input_changed();
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

        assert!(state.skill_picker_visible());
        assert!(move_down_from_input(&mut state, 20));
        assert_eq!(state.subagent_focus, Some(0));
        assert_eq!(state.skill_picker_focus, 0);
        assert!(subagent_stream_overlay_area(&state, Rect::new(0, 4, 80, 2), 0).is_some());
        assert!(move_up_from_input_or_subagent(&mut state, 20));
        assert_eq!(state.subagent_focus, None);
        assert_eq!(state.skill_picker_focus, 0);
    }

    #[test]
    fn subagent_focus_moves_from_prompt_to_list_on_down_and_returns_on_up() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.input = "prompt".to_owned();
        state.cursor = state.input.chars().count();
        for (call_id, task_id) in [("call-one", "one"), ("call-two", "two")] {
            state.subagents.push(SubagentTask {
                call_id: call_id.to_owned(),
                task_id: Some(task_id.to_owned()),
                task: task_id.to_owned(),
                model: None,
                effort: None,
                status: SubagentStatus::Running,
                result: None,
                creation_completed: true,
                stream: Vec::new(),
                stream_chars: 0,
            });
        }

        assert_eq!(input_cursor_row(&state.input, state.cursor, 20), 0);
        assert!(state.focus_subagent_list_from_input());
        assert_eq!(state.subagent_focus, Some(0));
        assert!(state.move_subagent_focus(false));
        assert_eq!(
            state.subagent_focus, None,
            "Up from the first row returns to the prompt"
        );

        assert!(state.focus_subagent_list_from_input());
        assert!(state.move_subagent_focus(true));
        assert_eq!(
            state.subagent_focus,
            Some(1),
            "Down advances through the list"
        );
        assert!(state.move_subagent_focus(false));
        assert_eq!(state.subagent_focus, Some(0));
    }

    #[test]
    fn subagent_lifecycle_tool_cards_are_suppressed_from_the_transcript() {
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
                timestamp: 4,
                message: ChatMessage::tool(
                    "call-check".to_owned(),
                    "check_subagent".to_owned(),
                    serde_json::json!({"task_id":"subagent-1","status":"running"}).to_string(),
                ),
            },
        ];
        let state = UiState::from_history(&history, "secret", "model", None, false);
        assert_eq!(state.subagents.len(), 1);
        assert!(state.transcript.is_empty());
        assert_eq!(transcript_lines(&state, 100)[0].to_string(), "");
    }

    #[test]
    fn suppressed_lifecycle_tools_do_not_leave_transcript_spacing() {
        for name in [
            "spawn_subagent",
            "check_subagent",
            "wait_subagent",
            "send_subagent",
            "cancel_subagent",
        ] {
            let state = UiState {
                transcript: vec![
                    TranscriptItem::Assistant("before".to_owned()),
                    TranscriptItem::ToolCall {
                        id: "call".to_owned(),
                        name: name.to_owned(),
                        arguments: "{}".to_owned(),
                    },
                    TranscriptItem::ToolResult {
                        id: "call".to_owned(),
                        name: name.to_owned(),
                        result: serde_json::json!({"status":"running"}),
                    },
                    TranscriptItem::Assistant("after".to_owned()),
                ],
                ..UiState::from_history(&[], "secret", "model", None, false)
            };
            let lines = transcript_lines(&state, 80)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>();
            assert_eq!(lines, ["before", "", "after"], "{name}");
        }
    }

    #[test]
    fn subagent_lifecycle_actions_annotate_the_running_list() {
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

        let call = |id: &str, name: &str| crate::model::ChatToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: serde_json::json!({"task_id":"subagent-1"}).to_string(),
        };
        state.add_live_tool_call(&call("check", "check_subagent"));
        assert!(matches!(
            state.subagent_list_notice_at("subagent-1", Instant::now()),
            Some(SubagentListNotice::Flash { .. })
        ));
        state.add_live_tool_result(
            "check",
            "check_subagent",
            serde_json::json!({"task_id":"subagent-1","status":"running"}),
        );

        state.add_live_tool_call(&call("wait", "wait_subagent"));
        assert!(matches!(
            state.subagent_list_notice_at("subagent-1", Instant::now()),
            Some(SubagentListNotice::Waiting)
        ));
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 2)).expect("test terminal");
        terminal
            .draw(|frame| draw_subagent_list(frame, &state, Rect::new(0, 0, 80, 2)))
            .expect("draw waiting worker");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Waiting for subagent-1"));
        state.add_live_tool_result(
            "wait",
            "wait_subagent",
            serde_json::json!({"task_id":"subagent-1","status":"waiting","timed_out":true}),
        );
        assert!(state
            .subagent_list_notice_at("subagent-1", Instant::now())
            .is_none());

        state.add_live_tool_call(&call("send", "send_subagent"));
        assert!(matches!(
            state.subagent_list_notice_at("subagent-1", Instant::now()),
            Some(SubagentListNotice::Flash { .. })
        ));
        state.add_live_tool_result(
            "send",
            "send_subagent",
            serde_json::json!({"task_id":"subagent-1","status":"queued"}),
        );
        state.add_live_tool_call(&call("cancel", "cancel_subagent"));
        state.add_live_tool_result(
            "cancel",
            "cancel_subagent",
            serde_json::json!({"task_id":"subagent-1","status":"cancellation_requested"}),
        );
        assert!(matches!(
            state.subagent_list_notice_at("subagent-1", Instant::now()),
            Some(SubagentListNotice::Cancelling)
        ));
    }

    #[test]
    fn cancelling_notice_survives_other_lifecycle_results_until_terminal() {
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
        for (id, name) in [("check", "check_subagent"), ("cancel", "cancel_subagent")] {
            state.add_live_tool_call(&crate::model::ChatToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: serde_json::json!({"task_id":"subagent-1"}).to_string(),
            });
        }
        state.add_live_tool_result(
            "check",
            "check_subagent",
            serde_json::json!({"task_id":"subagent-1","status":"failed"}),
        );
        assert!(matches!(
            state.subagent_list_notice_at("subagent-1", Instant::now()),
            Some(SubagentListNotice::Cancelling)
        ));
        state.add_live_tool_result(
            "cancel",
            "cancel_subagent",
            serde_json::json!({"task_id":"subagent-1","status":"cancellation_requested"}),
        );
        assert!(matches!(
            state.subagent_list_notice_at("subagent-1", Instant::now()),
            Some(SubagentListNotice::Cancelling)
        ));

        state.complete_subagent("subagent-1", serde_json::json!({"cancelled":true}));
        assert!(state
            .subagent_list_notice_at("subagent-1", Instant::now())
            .is_none());
    }

    #[test]
    fn failed_or_unknown_subagent_actions_remain_transcript_errors() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false);
        state.add_live_tool_call(&crate::model::ChatToolCall {
            id: "check-unknown".to_owned(),
            name: "check_subagent".to_owned(),
            arguments: serde_json::json!({"task_id":"unknown"}).to_string(),
        });
        let result = serde_json::json!({"task_id":"unknown","status":"unknown"});
        assert!(subagent_tool_result_is_error(&result));
        state.add_live_tool_result("check-unknown", "check_subagent", result);
        assert!(
            matches!(
                state.transcript.as_slice(),
                [TranscriptItem::Error(message)] if message.contains("unknown")
            ),
            "{:?}",
            state.transcript
        );
    }

    #[test]
    fn clipped_slash_picker_uses_its_actual_item_rows_for_the_focused_item() {
        let mut state = UiState::from_history(&[], "secret", "model", None, false)
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
        assert_eq!(selection_range(20, 0, 5), 0..5);
        assert_eq!(selection_range(20, 4, 5), 0..5);
        assert_eq!(selection_range(20, 5, 5), 1..6);
        assert_eq!(selection_range(20, 19, 5), 15..20);
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
        assert_eq!(buffer[(input_area.x, input_area.y)].bg, CONSOLE_BACKGROUND);
    }

    #[test]
    fn slash_picker_renders_count_with_bold_focus_on_the_picker_surface() {
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
