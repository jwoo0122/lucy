use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config::{Config, DEFAULT_API_KEY_ENV};
use crate::context::{resolve_boot_context_with_api_key_env, InstructionSource, SkillEntry};
use crate::model::{estimate_context_tokens, estimate_message_tokens, ChatMessage, ChatToolCall};
use crate::protocol::{EventSink, ProtocolEvent, ProtocolWriter};
use crate::provider::{Provider, ProviderStreamEvent, ProviderTurn};
use crate::redaction::{
    conflicts_with_protected_literal, conflicts_with_tui_literal, is_structural_key, redact_secret,
    redaction_marker,
};
use crate::session::Session;

#[derive(Debug)]
struct CliOptions {
    session: Option<String>,
    list_sessions: bool,
    jsonl: bool,
    tui: bool,
    version: bool,
}

#[derive(Debug, Deserialize)]
struct InputRecord {
    #[serde(rename = "type")]
    record_type: String,
    text: Option<String>,
}

const MAX_CONCURRENT_SUBAGENTS: usize = 4;
const MAX_SUBAGENT_TASK_BYTES: usize = 64 * 1024;
static NEXT_SUBAGENT_ID: AtomicU64 = AtomicU64::new(1);
const USER_CANCEL_REASON: &str = "user_cancelled";
const PROVIDER_PHASE: &str = "provider_stream";
const COMMAND_PHASE: &str = "cmd";
const AUTO_COMPACTION_THRESHOLD_PERCENT: usize = 95;
const COMPACTION_KEEP_RECENT_TOKENS: usize = 20_000;
const COMPACTION_SYSTEM_PROMPT: &str = "You are compacting a coding-agent conversation. Produce a concise, factual continuation summary. Preserve the user's goals, explicit decisions, constraints, files and code changes, commands and results, current implementation state, unresolved work, and exact identifiers that future turns need. Do not invent facts. Return only the summary text; do not call tools.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendMode {
    Jsonl,
    Tui,
}

pub fn run_cli<R, W, E>(args: &[String], input: R, output: W, diagnostics: E) -> i32
where
    R: BufRead + Send + 'static,
    W: Write,
    E: Write,
{
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            let mut diagnostics = diagnostics;
            write_diagnostic(&mut diagnostics, &error);
            return 2;
        }
    };
    if options.version {
        if let Err(error) = write_version(output) {
            let mut diagnostics = diagnostics;
            write_diagnostic(
                &mut diagnostics,
                &format!("unable to write version: {error}"),
            );
            return 1;
        }
        return 0;
    }

    let home = match home_directory() {
        Ok(home) => home,
        Err(error) => {
            let mut diagnostics = diagnostics;
            write_diagnostic(&mut diagnostics, &error);
            return 1;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_error) => {
            let mut diagnostics = diagnostics;
            write_diagnostic(&mut diagnostics, "unable to resolve cwd");
            return 1;
        }
    };
    run_cli_at_home_with_terminals(
        args,
        input,
        output,
        diagnostics,
        &home,
        &cwd,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    )
}

pub fn run_cli_at_home<R, W, E>(
    args: &[String],
    input: R,
    output: W,
    diagnostics: E,
    home: &Path,
    cwd: &Path,
) -> i32
where
    R: BufRead + Send + 'static,
    W: Write,
    E: Write,
{
    // The generic test/library entry point has no terminal handles. The real
    // binary uses run_cli, which supplies the actual stdio terminal state.
    run_cli_at_home_with_terminals(args, input, output, diagnostics, home, cwd, false, false)
}

#[allow(clippy::too_many_arguments)]
fn run_cli_at_home_with_terminals<R, W, E>(
    args: &[String],
    input: R,
    output: W,
    mut diagnostics: E,
    home: &Path,
    cwd: &Path,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> i32
where
    R: BufRead + Send + 'static,
    W: Write,
    E: Write,
{
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            let mut diagnostics = diagnostics;
            write_diagnostic(&mut diagnostics, &error);
            return 2;
        }
    };
    if options.version {
        if let Err(error) = write_version(output) {
            write_diagnostic(
                &mut diagnostics,
                &format!("unable to write version: {error}"),
            );
            return 1;
        }
        return 0;
    }
    let mode = match resolve_mode(args, stdin_is_tty, stdout_is_tty) {
        Ok(mode) => mode,
        Err(error) => {
            write_diagnostic(&mut diagnostics, &error);
            return 2;
        }
    };

    if options.list_sessions {
        let mut protocol = ProtocolWriter::new(output);
        if let Err(error) = Config::ensure_exists(home) {
            write_diagnostic(&mut diagnostics, &error.to_string());
            return 1;
        }
        return match Session::list(home) {
            Ok(sessions) => {
                for session in sessions {
                    if let Err(error) = protocol.emit_serializable(&session) {
                        write_diagnostic(
                            &mut diagnostics,
                            &format!("unable to write session metadata: {error}"),
                        );
                        return 1;
                    }
                }
                0
            }
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                1
            }
        };
    }

    let (session, provider, resumed, attached_agents) = if let Some(id) = options.session.as_deref()
    {
        let mut session = match Session::resume(home, id) {
            Ok(session) => session,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                return 1;
            }
        };
        let config = match Config::load_or_create(home) {
            Ok(config) => config,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                return 1;
            }
        };
        let selected = match config.resolved_llm() {
            Ok(settings) => settings,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    configured_api_key(&config).as_deref(),
                );
                return 1;
            }
        };
        session.llm.model = selected.model;
        session.llm.effort = selected.effort;
        let provider = match Provider::new(&session.llm) {
            Ok(provider) => provider,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                return 1;
            }
        };
        if let Err(error) =
            session.append_provider_settings(session.llm.model.clone(), session.llm.effort.clone())
        {
            write_diagnostic_safe(
                &mut diagnostics,
                &error.to_string(),
                Some(provider.api_key()),
            );
            return 1;
        }
        if mode == FrontendMode::Tui && conflicts_with_tui_literal(provider.api_key()) {
            write_diagnostic_safe(
                &mut diagnostics,
                "API key conflicts with terminal UI literals",
                Some(provider.api_key()),
            );
            return 1;
        }
        (session, provider, true, Vec::new())
    } else {
        let config = match Config::load_or_create(home) {
            Ok(config) => config,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                return 1;
            }
        };
        let configured_secret = configured_api_key(&config);
        let api_key_env = configured_api_key_env(&config);
        let llm = match config.resolved_llm() {
            Ok(llm) => llm,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    configured_secret.as_deref(),
                );
                return 1;
            }
        };
        let provider = match Provider::new(&llm) {
            Ok(provider) => provider,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    configured_secret.as_deref(),
                );
                return 1;
            }
        };
        if mode == FrontendMode::Tui && conflicts_with_tui_literal(provider.api_key()) {
            write_diagnostic_safe(
                &mut diagnostics,
                "API key conflicts with terminal UI literals",
                Some(provider.api_key()),
            );
            return 1;
        }
        let safe_cwd = match std::fs::canonicalize(cwd) {
            Ok(cwd) if !cwd.display().to_string().contains(provider.api_key()) => cwd,
            Ok(_) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    "session header rejected",
                    Some(provider.api_key()),
                );
                return 1;
            }
            Err(_) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    "unable to resolve session cwd",
                    Some(provider.api_key()),
                );
                return 1;
            }
        };
        let context = match resolve_boot_context_with_api_key_env(
            home,
            &safe_cwd,
            &config.system_prompt,
            api_key_env.as_deref(),
        ) {
            Ok(context) => context,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    configured_secret.as_deref(),
                );
                return 1;
            }
        };
        let boot_system_prompt = redact_secret(&context.system_prompt, Some(provider.api_key()));
        let attached_agents = attached_agents(context.instruction_files, provider.api_key());
        let skills = redact_skills(context.skills, provider.api_key());
        let session = match Session::create_with_skills_and_secret(
            home,
            &safe_cwd,
            boot_system_prompt,
            llm,
            skills,
            Some(provider.api_key()),
        ) {
            Ok(session) => session,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    Some(provider.api_key()),
                );
                return 1;
            }
        };
        (session, provider, false, attached_agents)
    };

    let harness = Harness {
        home: home.to_path_buf(),
        session,
        provider,
        context_window: None,
        attached_agents,
        subagents: Arc::new(Mutex::new(HashMap::new())),
        completed_subagents: mpsc::channel(),
    };
    if mode == FrontendMode::Tui {
        return match crate::tui::run(harness, resumed, output) {
            Ok(()) => 0,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error);
                1
            }
        };
    }

    let mut protocol = ProtocolWriter::new(output);
    let mut harness = harness;
    if let Err(error) = protocol.session(&harness.session.id, resumed) {
        write_diagnostic_safe(
            &mut diagnostics,
            &format!("unable to write session event: {error}"),
            Some(harness.provider.api_key()),
        );
        return 1;
    }

    let (input_tx, input_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in input.lines() {
            if input_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut input_closed = false;
    while !input_closed || harness.has_running_subagents() {
        if let Some(notification) = harness.next_subagent_notification() {
            if let Err(error) = harness.handle_message(&notification, &mut protocol, None) {
                let error = redact_secret(&error, Some(harness.provider.api_key()));
                let _ = protocol.error(&error);
            }
            continue;
        }
        let line = match input_rx.recv_timeout(std::time::Duration::from_millis(25)) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &format!("unable to read stdin: {error}"),
                    Some(harness.provider.api_key()),
                );
                return 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                input_closed = true;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let text = match parse_input_message(&line) {
            Ok(text) => text,
            Err(error) => {
                let error = redact_secret(&error, Some(harness.provider.api_key()));
                if let Err(write_error) = protocol.error(&error) {
                    write_diagnostic_safe(
                        &mut diagnostics,
                        &format!("unable to write protocol error: {write_error}"),
                        Some(harness.provider.api_key()),
                    );
                    return 1;
                }
                continue;
            }
        };
        if let Err(error) = harness.handle_message(&text, &mut protocol, None) {
            let error = redact_secret(&error, Some(harness.provider.api_key()));
            if let Err(write_error) = protocol.error(&error) {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &format!("unable to write protocol error: {write_error}"),
                    Some(harness.provider.api_key()),
                );
                return 1;
            }
        }
    }
    0
}

pub fn resolve_mode(
    args: &[String],
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> Result<FrontendMode, String> {
    let options = parse_args(args)?;
    if options.list_sessions {
        if options.tui {
            return Err("--tui cannot be combined with --list-sessions".to_owned());
        }
        return Ok(FrontendMode::Jsonl);
    }
    if options.tui && !(stdin_is_tty && stdout_is_tty) {
        return Err("--tui requires a terminal on stdin and stdout".to_owned());
    }
    if options.tui {
        Ok(FrontendMode::Tui)
    } else if options.jsonl || !(stdin_is_tty && stdout_is_tty) {
        Ok(FrontendMode::Jsonl)
    } else {
        Ok(FrontendMode::Tui)
    }
}

pub(crate) struct Harness {
    pub(crate) home: PathBuf,
    pub(crate) session: Session,
    pub(crate) provider: Provider,
    /// Model context metadata resolved by the interactive frontend; `None`
    /// keeps compaction disabled when an OpenAI-compatible provider exposes no
    /// context-window metadata.
    pub(crate) context_window: Option<usize>,
    /// AGENTS.md sources selected for this newly created session's boot context.
    /// The TUI uses these only while its first-boot welcome is visible.
    pub(crate) attached_agents: Vec<String>,
    subagents: Arc<Mutex<HashMap<String, SubagentState>>>,
    completed_subagents: (
        mpsc::Sender<SubagentCompletion>,
        mpsc::Receiver<SubagentCompletion>,
    ),
}

#[derive(Clone)]
enum SubagentState {
    Running,
    Completed(Value),
}

struct SubagentCompletion {
    task_id: String,
    result: Value,
}

fn should_compact_context(context_tokens: usize, context_window: usize) -> bool {
    context_window > 0
        && context_tokens as u128 * 100
            >= context_window as u128 * AUTO_COMPACTION_THRESHOLD_PERCENT as u128
}

fn find_compaction_boundary(
    messages: &[ChatMessage],
    previous_boundary: Option<usize>,
) -> Option<usize> {
    let user_starts = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == "user").then_some(index))
        .collect::<Vec<_>>();
    let mut start = *user_starts.last()?;
    let end = messages.len();
    let mut kept_tokens = messages[start..end]
        .iter()
        .map(estimate_message_tokens)
        .sum::<usize>();

    while kept_tokens < COMPACTION_KEEP_RECENT_TOKENS {
        let Some(previous_start) = user_starts
            .iter()
            .copied()
            .rev()
            .find(|candidate| *candidate < start)
        else {
            break;
        };
        start = previous_start;
        kept_tokens = messages[start..end]
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>();
    }

    (start > 0 && previous_boundary.is_none_or(|previous| start > previous)).then_some(start)
}

impl Harness {
    pub(crate) fn next_subagent_notification(&mut self) -> Option<String> {
        let completion = self.completed_subagents.1.try_recv().ok()?;
        Some(format!(
            "Background subagent {} completed. Deliver this result to the user and continue the task: {}",
            completion.task_id,
            serde_json::to_string(&completion.result).unwrap_or_else(|_| "{\"error\":\"unable to encode subagent result\"}".to_owned())
        ))
    }

    fn has_running_subagents(&self) -> bool {
        self.subagents.lock().is_ok_and(|states| {
            states
                .values()
                .any(|state| matches!(state, SubagentState::Running))
        })
    }

    fn spawn_subagent(&self, task: String, model: Option<String>, effort: Option<String>) -> Value {
        let mut subagents = match self.subagents.lock() {
            Ok(subagents) => subagents,
            Err(_) => return serde_json::json!({"error": "subagent registry unavailable"}),
        };
        let running = subagents
            .values()
            .filter(|state| matches!(state, SubagentState::Running))
            .count();
        if running >= MAX_CONCURRENT_SUBAGENTS {
            return serde_json::json!({"error": format!("subagent concurrency limit is {MAX_CONCURRENT_SUBAGENTS}")});
        }
        let task_id = format!(
            "subagent-{}",
            NEXT_SUBAGENT_ID.fetch_add(1, Ordering::Relaxed)
        );
        subagents.insert(task_id.clone(), SubagentState::Running);
        drop(subagents);
        let settings = self.session.llm.clone();
        let boot = self.session.boot_system_prompt.clone();
        let cwd = self.session.cwd.clone();
        let secret = self.provider.api_key().to_owned();
        let states = Arc::clone(&self.subagents);
        let completed = self.completed_subagents.0.clone();
        let completion_id = task_id.clone();
        std::thread::spawn(move || {
            let result = redact_json_value(
                run_subagent(settings, boot, cwd, task, model, effort, None),
                &secret,
            );
            if let Ok(mut states) = states.lock() {
                states.insert(
                    completion_id.clone(),
                    SubagentState::Completed(result.clone()),
                );
            }
            let _ = completed.send(SubagentCompletion {
                task_id: completion_id,
                result,
            });
        });
        serde_json::json!({"task_id": task_id, "status": "queued"})
    }

    fn subagent_status(&self, arguments: &str) -> Value {
        let task_id = match serde_json::from_str::<Value>(arguments)
            .ok()
            .and_then(|value| {
                value
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }) {
            Some(task_id) => task_id,
            None => return serde_json::json!({"error": "check_subagent requires a task_id string"}),
        };
        match self
            .subagents
            .lock()
            .ok()
            .and_then(|states| states.get(&task_id).cloned())
        {
            Some(SubagentState::Running) => {
                serde_json::json!({"task_id": task_id, "status": "running"})
            }
            Some(SubagentState::Completed(result)) => {
                serde_json::json!({"task_id": task_id, "status": "completed", "result": result})
            }
            None => serde_json::json!({"task_id": task_id, "status": "unknown"}),
        }
    }

    pub(crate) fn apply_settings(
        &mut self,
        home: &Path,
        model: String,
        effort: Option<String>,
    ) -> Result<(), String> {
        let config = Config::load_or_create(home).map_err(|error| error.to_string())?;
        let mut settings = config.resolved_llm().map_err(|error| error.to_string())?;
        settings.model = model.trim().to_owned();
        settings.effort = effort
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        // Endpoint and credential remain the session's established provider boundary.
        settings.base_url = self.session.llm.base_url.clone();
        settings.api_key_env = self.session.llm.api_key_env.clone();
        let provider = Provider::new(&settings).map_err(|error| error.to_string())?;
        // Validate the candidate before changing the user-owned source of truth.
        Config::save_selection(home, &settings.model, settings.effort.as_deref())
            .map_err(|error| error.to_string())?;
        self.session
            .append_provider_settings(settings.model.clone(), settings.effort.clone())
            .map_err(|error| error.to_string())?;
        self.session.llm = settings;
        self.provider = provider;
        self.context_window = self.provider.context_window();
        Ok(())
    }

    fn should_compact(&self, messages: &[ChatMessage]) -> bool {
        self.context_window
            .is_some_and(|window| should_compact_context(estimate_context_tokens(messages), window))
    }

    fn compaction_boundary(&self) -> Option<usize> {
        let latest_boundary = self
            .session
            .history
            .iter()
            .rev()
            .find_map(|record| match record {
                crate::session::SessionHistoryRecord::Compaction(compaction) => {
                    Some(compaction.first_kept_message)
                }
                _ => None,
            });
        find_compaction_boundary(&self.session.messages, latest_boundary)
    }

    fn compact_context<S: EventSink>(
        &mut self,
        sink: &mut S,
        cancellation: Option<&crate::cancellation::CancellationToken>,
        tokens_before: usize,
    ) -> Result<(), String> {
        let Some(boundary) = self.compaction_boundary() else {
            return Err("context cannot be compacted without an earlier complete turn".to_owned());
        };
        let Some(cancellation) = cancellation else {
            return Err("context compaction requires a cancellable turn".to_owned());
        };
        sink.compaction_started()
            .map_err(|error| format!("unable to emit compaction state: {error}"))?;
        let context_messages = self.session.provider_messages();
        let mut summary_messages = Vec::with_capacity(context_messages.len() + 1);
        summary_messages.push(ChatMessage::system(self.session.boot_system_prompt.clone()));
        summary_messages.push(ChatMessage::system(COMPACTION_SYSTEM_PROMPT.to_owned()));
        summary_messages.extend(context_messages.into_iter().skip(1));
        let summary = match self.provider.summarize(&summary_messages, cancellation) {
            Ok(summary) => redact_secret(&summary, Some(self.provider.api_key())),
            Err(error) if cancellation.is_cancelled() || error.is_cancelled() => {
                return self.interrupt(sink, PROVIDER_PHASE, "", &[], Vec::new());
            }
            Err(error) => return Err(format!("unable to compact context: {error}")),
        };
        self.session
            .append_compaction(summary, boundary, tokens_before)
            .map_err(|error| format!("unable to persist context compaction: {error}"))?;
        let tokens_after = estimate_context_tokens(&self.session.provider_messages());
        sink.compaction_finished(tokens_before, tokens_after)
            .map_err(|error| format!("unable to emit compaction state: {error}"))?;
        Ok(())
    }

    pub(crate) fn handle_message<S: EventSink>(
        &mut self,
        text: &str,
        sink: &mut S,
        cancellation: Option<&crate::cancellation::CancellationToken>,
    ) -> Result<(), String> {
        let secret = self.provider.api_key().to_owned();
        let expanded = expand_skill_invocation(text, &self.session.skills)?;
        let user_message = ChatMessage::user(redact_secret(&expanded.text, Some(&secret)));
        if let Err(error) = self.session.append_message(user_message) {
            if cancellation.is_some_and(|token| token.is_cancelled()) {
                let interruption = self.interrupt(sink, PROVIDER_PHASE, "", &[], Vec::new());
                return interruption
                    .map_err(|interrupt_error| format!("{error}; {interrupt_error}"));
            }
            return Err(error.to_string());
        }
        if let Some(name) = expanded.attached_skill.as_deref() {
            sink.skill_instruction_attached(name)
                .map_err(|error| format!("unable to emit skill attachment state: {error}"))?;
        }

        let mut compacted_for_turn = false;
        loop {
            if cancellation.is_some_and(|token| token.is_cancelled()) {
                return self.interrupt(sink, PROVIDER_PHASE, "", &[], Vec::new());
            }
            let mut messages = self.session.provider_messages();
            let tokens_before = estimate_context_tokens(&messages);
            if !compacted_for_turn && self.should_compact(&messages) {
                self.compact_context(sink, cancellation, tokens_before)?;
                compacted_for_turn = true;
                messages = self.session.provider_messages();
            }
            sink.context_usage(estimate_context_tokens(&messages))
                .map_err(|error| format!("unable to emit context usage: {error}"))?;
            let mut raw_content = String::new();
            let mut redactor = SecretRedactor::new(&secret);
            let mut reasoning_active = false;
            let stream_result = {
                let mut on_event = |event: ProviderStreamEvent| -> io::Result<()> {
                    match event {
                        ProviderStreamEvent::ReasoningStarted => {
                            if !reasoning_active {
                                reasoning_active = true;
                                sink.reasoning_started()?;
                            }
                            Ok(())
                        }
                        ProviderStreamEvent::Text(delta) => {
                            if reasoning_active {
                                reasoning_active = false;
                                sink.reasoning_completed()?;
                            }
                            raw_content.push_str(&delta);
                            redactor.push(&delta, |safe_delta| {
                                sink.emit_event(&ProtocolEvent::AssistantDelta {
                                    text: safe_delta.to_owned(),
                                })
                            })
                        }
                    }
                };
                match cancellation {
                    Some(token) => self
                        .provider
                        .stream_chat_cancellable_with_options_and_events(
                            &messages,
                            &mut on_event,
                            token,
                            true,
                            true,
                        ),
                    None => self.provider.stream_chat(&messages, &mut |delta| {
                        raw_content.push_str(delta);
                        redactor.push(delta, |safe_delta| {
                            sink.emit_event(&ProtocolEvent::AssistantDelta {
                                text: safe_delta.to_owned(),
                            })
                        })
                    }),
                }
            };
            redactor
                .finish(|safe_delta| {
                    sink.emit_event(&ProtocolEvent::AssistantDelta {
                        text: safe_delta.to_owned(),
                    })
                })
                .map_err(|error| format!("unable to write assistant delta: {error}"))?;
            let turn = match stream_result {
                Ok(turn) => {
                    if reasoning_active {
                        sink.reasoning_completed()
                            .map_err(|error| format!("unable to emit reasoning state: {error}"))?;
                    }
                    turn
                }
                Err(error)
                    if cancellation.is_some_and(|token| token.is_cancelled())
                        || error.is_cancelled() =>
                {
                    if reasoning_active {
                        sink.reasoning_completed()
                            .map_err(|error| format!("unable to emit reasoning state: {error}"))?;
                    }
                    let partial = error.partial_turn().cloned().unwrap_or(ProviderTurn {
                        content: raw_content,
                        tool_calls: Vec::new(),
                        reasoning_details: Vec::new(),
                    });
                    return self.interrupt(
                        sink,
                        PROVIDER_PHASE,
                        &partial.content,
                        &partial.tool_calls,
                        Vec::new(),
                    );
                }
                Err(error) => {
                    if reasoning_active {
                        sink.reasoning_completed()
                            .map_err(|error| format!("unable to emit reasoning state: {error}"))?;
                    }
                    return Err(error.to_string());
                }
            };
            let canceled_after_stream = cancellation.is_some_and(|token| token.is_cancelled());

            if turn.tool_calls.iter().any(|call| {
                call.name != "cmd" && call.name != "spawn_subagent" && call.name != "check_subagent"
            }) {
                if canceled_after_stream {
                    return self.interrupt(sink, PROVIDER_PHASE, &turn.content, &[], Vec::new());
                }
                return Err("provider requested an unsupported tool".to_owned());
            }
            let safe_tool_calls = turn
                .tool_calls
                .iter()
                .map(|call| safe_tool_call(call, &secret))
                .collect::<Vec<_>>();
            let assistant_content = redact_secret(&turn.content, Some(&secret));
            let safe_reasoning_details = redact_reasoning_details(&turn.reasoning_details, &secret);
            let mut assistant =
                ChatMessage::assistant(assistant_content.clone(), safe_tool_calls.clone());
            assistant.reasoning_details = safe_reasoning_details;
            if let Err(error) = self.session.append_message(assistant) {
                if cancellation.is_some_and(|token| token.is_cancelled()) {
                    let interruption = self.interrupt(
                        sink,
                        PROVIDER_PHASE,
                        &assistant_content,
                        &turn.tool_calls,
                        Vec::new(),
                    );
                    return interruption
                        .map_err(|interrupt_error| format!("{error}; {interrupt_error}"));
                }
                return Err(error.to_string());
            }

            if safe_tool_calls.is_empty() {
                if canceled_after_stream || cancellation.is_some_and(|token| !token.try_complete())
                {
                    return self.interrupt(sink, PROVIDER_PHASE, "", &[], Vec::new());
                }
                sink.context_usage(estimate_context_tokens(&self.session.provider_messages()))
                    .map_err(|error| format!("unable to emit context usage: {error}"))?;
                sink.emit_event(&ProtocolEvent::TurnEnd)
                    .map_err(|error| format!("unable to write turn end: {error}"))?;
                return Ok(());
            }

            for safe_call in &safe_tool_calls {
                sink.emit_event(&ProtocolEvent::ToolCall {
                    id: safe_call.id.clone(),
                    name: safe_call.name.clone(),
                    arguments: safe_call.arguments.clone(),
                })
                .map_err(|error| format!("unable to write tool call: {error}"))?;
            }
            for (index, raw_call) in turn.tool_calls.iter().enumerate() {
                let safe_call = &safe_tool_calls[index];
                let result = if raw_call.name == "spawn_subagent" {
                    match parse_subagent_arguments(&raw_call.arguments) {
                        Ok((task, model, effort)) => self.spawn_subagent(task, model, effort),
                        Err(error) => serde_json::json!({"error": error}),
                    }
                } else if raw_call.name == "check_subagent" {
                    self.subagent_status(&raw_call.arguments)
                } else if cancellation.is_some_and(|token| token.is_cancelled()) {
                    serde_json::to_value(crate::command::canceled_result(
                        &safe_call.arguments,
                        &secret,
                    ))
                    .map_err(|error| format!("unable to encode cmd result: {error}"))?
                } else {
                    serde_json::to_value(crate::command::execute_with_cancellation(
                        &raw_call.arguments,
                        &self.session.cwd,
                        self.provider.api_key_env(),
                        Some(&secret),
                        cancellation,
                    ))
                    .map_err(|error| format!("unable to encode cmd result: {error}"))?
                };
                let result = redact_json_value(result, &secret);
                let tool_content = serde_json::to_string(&result)
                    .map_err(|error| format!("unable to encode tool result: {error}"))?;
                let tool_message = ChatMessage::tool(
                    safe_call.id.clone(),
                    safe_call.name.clone(),
                    redact_secret(&tool_content, Some(&secret)),
                );
                let observation = crate::session::SessionToolResult {
                    id: safe_call.id.clone(),
                    name: safe_call.name.clone(),
                    result: result.clone(),
                };
                if let Err(error) = self.session.append_message(tool_message) {
                    if cancellation.is_some_and(|token| token.is_cancelled()) {
                        let interruption =
                            self.interrupt(sink, COMMAND_PHASE, "", &[], vec![observation]);
                        return interruption
                            .map_err(|interrupt_error| format!("{error}; {interrupt_error}"));
                    }
                    return Err(error.to_string());
                }
                sink.emit_event(&ProtocolEvent::ToolResult {
                    id: safe_call.id.clone(),
                    name: safe_call.name.clone(),
                    result: result.clone(),
                })
                .map_err(|error| format!("unable to write tool result: {error}"))?;
                if cancellation.is_some_and(|token| token.is_cancelled()) {
                    for pending_call in safe_tool_calls.iter().skip(index + 1) {
                        let pending_result = redact_json_value(
                            serde_json::to_value(crate::command::canceled_result(
                                &pending_call.arguments,
                                &secret,
                            ))
                            .map_err(|error| format!("unable to encode cmd result: {error}"))?,
                            &secret,
                        );
                        let pending_content = serde_json::to_string(&pending_result)
                            .map_err(|error| format!("unable to encode tool result: {error}"))?;
                        let pending_message = ChatMessage::tool(
                            pending_call.id.clone(),
                            pending_call.name.clone(),
                            redact_secret(&pending_content, Some(&secret)),
                        );
                        let pending_observation = crate::session::SessionToolResult {
                            id: pending_call.id.clone(),
                            name: pending_call.name.clone(),
                            result: pending_result.clone(),
                        };
                        if let Err(error) = self.session.append_message(pending_message) {
                            if cancellation.is_some_and(|token| token.is_cancelled()) {
                                let interruption = self.interrupt(
                                    sink,
                                    COMMAND_PHASE,
                                    "",
                                    &[],
                                    vec![pending_observation],
                                );
                                return interruption.map_err(|interrupt_error| {
                                    format!("{error}; {interrupt_error}")
                                });
                            }
                            return Err(error.to_string());
                        }
                        sink.emit_event(&ProtocolEvent::ToolResult {
                            id: pending_call.id.clone(),
                            name: pending_call.name.clone(),
                            result: pending_result.clone(),
                        })
                        .map_err(|error| format!("unable to write tool result: {error}"))?;
                    }
                    return self.interrupt(sink, COMMAND_PHASE, "", &[], Vec::new());
                }
            }
        }
    }

    fn interrupt<S: EventSink>(
        &mut self,
        sink: &mut S,
        phase: &str,
        assistant_text: &str,
        tool_calls: &[ChatToolCall],
        tool_results: Vec<crate::session::SessionToolResult>,
    ) -> Result<(), String> {
        let secret = self.provider.api_key();
        let safe_tool_calls = tool_calls
            .iter()
            .filter(|call| call.name == "cmd")
            .map(|call| safe_partial_tool_call(call, secret))
            .collect::<Vec<_>>();
        let safe_tool_results = tool_results.clone();
        let interruption = crate::session::InterruptionRecord {
            timestamp: 0,
            reason: USER_CANCEL_REASON.to_owned(),
            phase: phase.to_owned(),
            assistant_text: redact_secret(assistant_text, Some(secret)),
            tool_calls: safe_tool_calls.clone(),
            tool_results,
        };
        let persistence_error = self.session.append_interruption(interruption).err();
        let mut event_error = None;
        for call in &safe_tool_calls {
            if let Err(error) = sink.emit_event(&ProtocolEvent::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            }) {
                event_error.get_or_insert(error);
            }
        }
        for observation in &safe_tool_results {
            if let Err(error) = sink.emit_event(&ProtocolEvent::ToolResult {
                id: observation.id.clone(),
                name: observation.name.clone(),
                result: observation.result.clone(),
            }) {
                event_error.get_or_insert(error);
            }
        }
        if let Err(error) = sink.emit_event(&ProtocolEvent::TurnInterrupted {
            reason: USER_CANCEL_REASON.to_owned(),
            phase: phase.to_owned(),
        }) {
            event_error.get_or_insert(error);
        }
        match (persistence_error, event_error) {
            (None, None) => Ok(()),
            (Some(error), None) => Err(format!("unable to persist interruption: {error}")),
            (None, Some(error)) => Err(format!("unable to write interruption event: {error}")),
            (Some(persistence), Some(event)) => Err(format!(
                "unable to persist interruption: {persistence}; unable to write interruption event: {event}"
            )),
        }
    }
}

fn parse_subagent_arguments(
    arguments: &str,
) -> Result<(String, Option<String>, Option<String>), String> {
    let value: Value = serde_json::from_str(arguments)
        .map_err(|_| "spawn_subagent arguments must be a JSON object")?;
    let object = value
        .as_object()
        .ok_or("spawn_subagent arguments must be a JSON object")?;
    if object
        .keys()
        .any(|key| key != "task" && key != "model" && key != "effort")
    {
        return Err("spawn_subagent arguments contain an unsupported field".to_owned());
    }
    let task = object
        .get("task")
        .and_then(Value::as_str)
        .ok_or("spawn_subagent task must be a string")?
        .trim()
        .to_owned();
    if task.is_empty() || task.len() > MAX_SUBAGENT_TASK_BYTES {
        return Err("spawn_subagent task must be non-empty and bounded".to_owned());
    }
    let optional = |key: &str| -> Result<Option<String>, String> {
        match object.get(key) {
            None => Ok(None),
            Some(Value::String(value)) if !value.trim().is_empty() => {
                Ok(Some(value.trim().to_owned()))
            }
            _ => Err(format!("spawn_subagent {key} must be a non-empty string")),
        }
    };
    Ok((task, optional("model")?, optional("effort")?))
}

fn run_subagent(
    mut settings: crate::config::LlmSettings,
    boot_context: String,
    cwd: std::path::PathBuf,
    task: String,
    model: Option<String>,
    effort: Option<String>,
    cancellation: Option<crate::cancellation::CancellationToken>,
) -> Value {
    if let Some(model) = model {
        settings.model = model;
    }
    if let Some(effort) = effort {
        settings.effort = Some(effort);
    }
    let selected_model = settings.model.clone();
    let selected_effort = settings.effort.clone();
    let provider = match Provider::new(&settings) {
        Ok(provider) => provider,
        Err(error) => return serde_json::json!({"error": error.to_string()}),
    };
    let mut messages = vec![ChatMessage::system(boot_context), ChatMessage::user(task)];
    let cancellation = cancellation.unwrap_or_default();
    loop {
        if cancellation.is_cancelled() {
            return serde_json::json!({"cancelled": true});
        }
        let mut ignored = |_text: &str| Ok(());
        let turn = match provider.stream_chat_cancellable_with_options(
            &messages,
            &mut ignored,
            &cancellation,
            true,
            false,
        ) {
            Ok(turn) => turn,
            Err(error) if error.is_cancelled() || cancellation.is_cancelled() => {
                return serde_json::json!({"cancelled": true})
            }
            Err(error) => return serde_json::json!({"error": error.to_string()}),
        };
        if turn.tool_calls.iter().any(|call| call.name != "cmd") {
            return serde_json::json!({"error": "subagent requested an unsupported tool"});
        }
        messages.push(ChatMessage::assistant(
            turn.content.clone(),
            turn.tool_calls.clone(),
        ));
        if turn.tool_calls.is_empty() {
            return serde_json::json!({"model": selected_model, "effort": selected_effort, "output": turn.content});
        }
        for call in turn.tool_calls {
            let result = crate::command::execute_with_cancellation(
                &call.arguments,
                &cwd,
                provider.api_key_env(),
                Some(provider.api_key()),
                Some(&cancellation),
            );
            let content = match serde_json::to_string(&result) {
                Ok(content) => content,
                Err(_) => {
                    return serde_json::json!({"error": "unable to encode subagent command result"})
                }
            };
            messages.push(ChatMessage::tool(call.id, call.name, content));
        }
    }
}

struct SecretRedactor {
    secret_text: String,
    secret: Vec<char>,
    marker: String,
    pending: String,
}

impl SecretRedactor {
    fn new(secret: &str) -> Self {
        Self {
            secret_text: secret.to_owned(),
            secret: secret.chars().collect(),
            marker: redaction_marker(secret).unwrap_or_default(),
            pending: String::new(),
        }
    }

    fn push<F>(&mut self, text: &str, mut emit: F) -> io::Result<()>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        if self.secret.is_empty() {
            return emit(text);
        }

        let mut output = String::new();
        for character in text.chars() {
            self.pending.push(character);
            if self.pending.chars().eq(self.secret.iter().copied()) {
                self.pending.clear();
                output.push_str(&self.marker);
                continue;
            }
            if self.pending_is_secret_prefix() {
                continue;
            }

            let pending = self.pending.chars().collect::<Vec<_>>();
            let suffix_len = (1..pending.len())
                .rev()
                .find(|length| {
                    pending[pending.len() - length..].iter().copied().eq(self
                        .secret
                        .iter()
                        .copied()
                        .take(*length))
                })
                .unwrap_or(0);
            let safe_len = pending.len() - suffix_len;
            output.extend(pending[..safe_len].iter());
            self.pending = pending[safe_len..].iter().collect();
        }

        if output.is_empty() {
            Ok(())
        } else {
            let safe_output = redact_secret(&output, Some(&self.secret_text));
            emit(&safe_output)
        }
    }

    fn finish<F>(&mut self, mut emit: F) -> io::Result<()>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return Ok(());
        }
        let safe_pending = redact_secret(&pending, Some(&self.secret_text));
        emit(&safe_pending)
    }

    fn pending_is_secret_prefix(&self) -> bool {
        let length = self.pending.chars().count();
        length < self.secret.len()
            && self
                .pending
                .chars()
                .zip(self.secret.iter().copied())
                .all(|(pending, secret)| pending == secret)
    }
}

/// Return the AGENTS.md files selected for the current new-session boot context.
/// Paths are secret-redacted before they can reach the terminal UI.
fn attached_agents(instruction_files: Vec<InstructionSource>, secret: &str) -> Vec<String> {
    instruction_files
        .into_iter()
        .filter(|source| {
            source
                .path
                .file_name()
                .is_some_and(|name| name == "AGENTS.md")
        })
        .map(|source| redact_secret(&source.path.display().to_string(), Some(secret)))
        .collect()
}

/// Store a secret-safe skill snapshot with the session. The source is read
/// once during secure context discovery; later invocations never follow paths.
fn escape_xml_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&apos;")
}

fn redact_skills(skills: Vec<SkillEntry>, secret: &str) -> Vec<SkillEntry> {
    skills
        .into_iter()
        .map(|skill| SkillEntry {
            name: redact_secret(&skill.name, Some(secret)),
            description: redact_secret(&skill.description, Some(secret)),
            path: std::path::PathBuf::from(redact_secret(
                &skill.path.display().to_string(),
                Some(secret),
            )),
            contents: redact_secret(&skill.contents, Some(secret)),
            model_invocable: skill.model_invocable,
        })
        .collect()
}

/// The message delivered to the provider and the optional name of the saved
/// skill snapshot that was attached to it.
#[derive(Debug)]
struct ExpandedSkillInvocation {
    text: String,
    attached_skill: Option<String>,
}

/// Expand slash-prefixed skill names into the user message sent to the
/// provider. This deliberately adds no model-facing tool: skills are context,
/// not an executable capability of their own.
fn expand_skill_invocation(
    text: &str,
    skills: &[SkillEntry],
) -> Result<ExpandedSkillInvocation, String> {
    let Some(invocation) = text.strip_prefix('/') else {
        return Ok(ExpandedSkillInvocation {
            text: text.to_owned(),
            attached_skill: None,
        });
    };
    let mut pieces = invocation.splitn(2, char::is_whitespace);
    let name = pieces.next().unwrap_or_default();
    if name.is_empty() {
        return Err("skill command requires a skill name: /<name> [args]".to_owned());
    }
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        return Err(format!("unknown skill: {name}"));
    };
    let arguments = pieces.next().unwrap_or_default().trim();
    let mut message = format!(
        "<skill name=\"{}\" location=\"{}\">\n{}\n</skill>",
        escape_xml_attribute(&skill.name),
        escape_xml_attribute(&skill.path.display().to_string()),
        skill.contents.trim()
    );
    if !arguments.is_empty() {
        message.push_str("\n\nUser: ");
        message.push_str(arguments);
    }
    Ok(ExpandedSkillInvocation {
        text: message,
        attached_skill: Some(skill.name.clone()),
    })
}

#[cfg(test)]
fn redact_tool_arguments(arguments: &str, secret: &str) -> String {
    safe_tool_call(
        &ChatToolCall {
            id: String::new(),
            name: "cmd".to_owned(),
            arguments: arguments.to_owned(),
        },
        secret,
    )
    .arguments
}

fn safe_tool_call(call: &ChatToolCall, secret: &str) -> ChatToolCall {
    let valid = match call.name.as_str() {
        "cmd" => serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .is_some_and(|object| {
                object.len() == 1 && object.get("command").is_some_and(Value::is_string)
            }),
        "spawn_subagent" => parse_subagent_arguments(&call.arguments).is_ok(),
        "check_subagent" => serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .is_some_and(|object| {
                object.len() == 1 && object.get("task_id").is_some_and(Value::is_string)
            }),
        _ => false,
    };
    let arguments = if valid {
        serde_json::to_string(&redact_json_value(
            serde_json::from_str(&call.arguments).unwrap_or(Value::Null),
            secret,
        ))
        .unwrap_or_else(|_| "{}".to_owned())
    } else {
        "{}".to_owned()
    };
    ChatToolCall {
        id: redact_secret(&call.id, Some(secret)),
        name: redact_secret(&call.name, Some(secret)),
        arguments,
    }
}

fn safe_partial_tool_call(call: &ChatToolCall, secret: &str) -> ChatToolCall {
    let arguments = if serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| object.len() == 1 && object.contains_key("command"))
    {
        safe_tool_call(call, secret).arguments
    } else {
        // An incomplete argument fragment is an observation only. Do not
        // preserve malformed provider JSON: decoding it later could expose a
        // credential that was hidden by the outer JSON string.
        "{}".to_owned()
    };
    ChatToolCall {
        id: redact_secret(&call.id, Some(secret)),
        name: redact_secret(&call.name, Some(secret)),
        arguments,
    }
}

fn redact_json_value(value: Value, secret: &str) -> Value {
    match value {
        Value::String(text) => Value::String(redact_secret(&text, Some(secret))),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_json_value(value, secret))
                .collect(),
        ),
        Value::Object(object) => {
            let marker = redaction_marker(secret).unwrap_or_default();
            let mut redacted = Map::new();
            for (key, value) in object {
                let mut safe_key = if is_structural_key(&key) {
                    key
                } else {
                    redact_secret(&key, Some(secret))
                };
                if redacted.contains_key(&safe_key) {
                    if marker.is_empty() {
                        continue;
                    }
                    while redacted.contains_key(&safe_key) {
                        safe_key.push_str(&marker);
                    }
                }
                redacted.insert(safe_key, redact_json_value(value, secret));
            }
            Value::Object(redacted)
        }
        value => value,
    }
}

fn redact_reasoning_details(details: &[Value], secret: &str) -> Option<Vec<Value>> {
    if details.is_empty() {
        return None;
    }
    match redact_json_value(Value::Array(details.to_vec()), secret) {
        Value::Array(details) => Some(details),
        _ => None,
    }
}

fn write_version<W: Write>(mut output: W) -> io::Result<()> {
    writeln!(output, "lucy {}", env!("CARGO_PKG_VERSION"))
}

fn parse_args(args: &[String]) -> Result<CliOptions, String> {
    let mut options = CliOptions {
        session: None,
        list_sessions: false,
        jsonl: false,
        tui: false,
        version: false,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--session" => {
                if options.list_sessions || options.session.is_some() {
                    return Err("--session cannot be combined or repeated".to_owned());
                }
                index += 1;
                let Some(id) = args.get(index) else {
                    return Err("--session requires an id".to_owned());
                };
                options.session = Some(id.clone());
            }
            "--list-sessions" => {
                if options.session.is_some() || options.list_sessions {
                    return Err("--list-sessions cannot be combined or repeated".to_owned());
                }
                options.list_sessions = true;
            }
            "--jsonl" => {
                if options.jsonl || options.tui {
                    return Err("--jsonl cannot be combined or repeated".to_owned());
                }
                options.jsonl = true;
            }
            "--tui" => {
                if options.tui || options.jsonl {
                    return Err("--tui cannot be combined or repeated".to_owned());
                }
                options.tui = true;
            }
            "--version" => {
                if options.version {
                    return Err("--version cannot be repeated".to_owned());
                }
                options.version = true;
            }
            "--help" | "-h" => {
                return Err(
                    "usage: lucy [--version] [--jsonl|--tui] [--session <id>] [--list-sessions]"
                        .to_owned(),
                );
            }
            _ => return Err("unknown argument".to_owned()),
        }
        index += 1;
    }
    Ok(options)
}

fn parse_input_message(line: &str) -> Result<String, String> {
    let record: InputRecord = serde_json::from_str(line)
        .map_err(|_| "input must be a JSONL message record".to_owned())?;
    if record.record_type != "message" {
        return Err("input record type must be message".to_owned());
    }
    record
        .text
        .ok_or_else(|| "message record requires a text string".to_owned())
}

fn home_directory() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; Lucy needs a user home directory".to_owned())
}

fn configured_api_key_env(config: &Config) -> Option<String> {
    let api_key_env = config
        .llm
        .api_key_env
        .as_deref()
        .unwrap_or(DEFAULT_API_KEY_ENV)
        .trim();
    (!api_key_env.is_empty()).then(|| api_key_env.to_owned())
}

fn configured_api_key(config: &Config) -> Option<String> {
    configured_api_key_env(config)
        .and_then(|api_key_env| std::env::var(api_key_env).ok())
        .filter(|secret| !secret.is_empty())
}

fn write_diagnostic_safe<W: Write>(diagnostics: &mut W, message: &str, secret: Option<&str>) {
    write_diagnostic_safe_with_environment(
        diagnostics,
        message,
        secret,
        std::env::vars().map(|(_, value)| value),
    );
}

fn write_diagnostic_safe_with_environment<W, I>(
    diagnostics: &mut W,
    message: &str,
    secret: Option<&str>,
    environment_values: I,
) where
    W: Write,
    I: IntoIterator<Item = String>,
{
    let mut safe_line = format!("!: {message}");
    safe_line = redact_secret(&safe_line, secret);
    let mut environment_secrets = environment_values
        .into_iter()
        .filter(|value| !value.is_empty() && !conflicts_with_protected_literal(value))
        .collect::<Vec<_>>();
    environment_secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for environment_secret in environment_secrets {
        safe_line = redact_secret(&safe_line, Some(&environment_secret));
    }
    let _ = writeln!(diagnostics, "{safe_line}");
}

fn write_diagnostic<W: Write>(diagnostics: &mut W, message: &str) {
    write_diagnostic_safe(diagnostics, message, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::CancellationToken;
    use std::io::{Cursor, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn auto_compaction_triggers_at_or_above_ninety_five_percent_only() {
        assert!(!should_compact_context(94, 100));
        assert!(should_compact_context(95, 100));
        assert!(should_compact_context(96, 100));
        assert!(!should_compact_context(100, 0));
    }

    #[test]
    fn compaction_boundary_keeps_complete_recent_turns() {
        let messages = [
            ChatMessage::user("old request".to_owned()),
            ChatMessage::assistant("old answer".to_owned(), Vec::new()),
            ChatMessage::user("recent request".to_owned()),
            ChatMessage::assistant("recent answer ".repeat(8_000), Vec::new()),
        ];

        assert_eq!(find_compaction_boundary(&messages, None), Some(2));
        assert_eq!(find_compaction_boundary(&messages, Some(2)), None);
    }

    #[test]
    fn spawned_worker_has_no_tool_round_limit() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("worker listener");
        listener
            .set_nonblocking(true)
            .expect("worker listener nonblocking");
        let address = listener.local_addr().expect("worker address");
        let mut responses = (0..33)
            .map(|index| {
                let tool = serde_json::json!({
                    "id": "provider-id",
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": format!("worker-call-{index}"),
                                "type": "function",
                                "function": {
                                    "name": "cmd",
                                    "arguments": "{\"command\":\"true\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }]
                });
                format!("data: {tool}\n\ndata: [DONE]\n\n")
            })
            .collect::<Vec<_>>();
        responses.push(normalized_provider_response("worker complete"));
        let expected_requests = responses.len();
        let server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut requests = 0;
            for response in responses {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok((stream, address)) => {
                            stream
                                .set_nonblocking(false)
                                .expect("worker connection blocking");
                            break (stream, address);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "worker request timed out"
                            );
                            thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(error) => panic!("worker accept: {error}"),
                    }
                };
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("worker clone"));
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("worker request header");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().expect("worker content length");
                        }
                    }
                }
                let mut body = vec![0_u8; content_length];
                reader.read_exact(&mut body).expect("worker request body");
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                stream.write_all(header.as_bytes()).expect("worker header");
                stream
                    .write_all(response.as_bytes())
                    .expect("worker response");
                stream.flush().expect("worker flush");
                requests += 1;
            }
            requests
        });

        let key_env = format!("LUCY_WORKER_LOOP_KEY_{}", std::process::id());
        std::env::set_var(&key_env, "provider-secret");
        let settings = crate::config::LlmSettings {
            base_url: format!("http://{address}/v1"),
            model: "worker-model".to_owned(),
            api_key_env: key_env.clone(),
            effort: None,
        };
        let result = run_subagent(
            settings,
            "boot context".to_owned(),
            std::env::current_dir().expect("worker cwd"),
            "inspect many steps".to_owned(),
            None,
            None,
            Some(CancellationToken::new()),
        );

        assert_eq!(result["output"], "worker complete");
        assert_eq!(server.join().expect("worker server"), expected_requests);
        std::env::remove_var(key_env);
    }

    fn normalized_provider_response(text: &str) -> String {
        let payload = serde_json::json!({
            "id": "provider-id",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
        });
        format!("data: {payload}\n\ndata: [DONE]\n\n")
    }

    #[test]
    fn mid_turn_compaction_summarizes_without_tools_then_continues_original_request() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("compaction listener");
        let address = listener.local_addr().expect("compaction address");
        let responses = ["summary", "continued"];
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for response_text in responses {
                let (mut stream, _) = listener.accept().expect("compaction request");
                let mut request = String::new();
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("request header");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().expect("content length");
                        }
                    }
                }
                let mut body = vec![0u8; content_length];
                reader.read_exact(&mut body).expect("request body");
                request.push_str(std::str::from_utf8(&body).expect("request JSON"));
                requests.push(serde_json::from_str::<Value>(&request).expect("request value"));
                let payload = serde_json::json!({
                    "choices": [{
                        "delta": {"content": response_text},
                        "finish_reason": null
                    }]
                });
                let body = format!("data: {payload}\n\ndata: [DONE]\n\n");
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream
                    .write_all(header.as_bytes())
                    .expect("response header");
                stream.write_all(body.as_bytes()).expect("response body");
                stream.flush().expect("response flush");
            }
            requests
        });

        let key_env = format!("LUCY_COMPACTION_APP_KEY_{}", std::process::id());
        std::env::set_var(&key_env, "provider-secret");
        let settings = crate::config::LlmSettings {
            base_url: format!("http://{address}/v1"),
            model: "model".to_owned(),
            api_key_env: key_env.clone(),
            effort: None,
        };
        let provider = Provider::new(&settings).expect("provider");
        let home = std::env::temp_dir().join(format!("lucy-app-compaction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir(&home).expect("temp home");
        let cwd = std::env::current_dir().expect("cwd");
        let mut session = Session::create_with_secret(
            &home,
            &cwd,
            "prompt".to_owned(),
            settings,
            Some("provider-secret"),
        )
        .expect("session");
        session
            .append_message(ChatMessage::user("old request".to_owned()))
            .expect("old user");
        session
            .append_message(ChatMessage::assistant("old answer".to_owned(), Vec::new()))
            .expect("old answer");
        session
            .append_message(ChatMessage::user("recent request".to_owned()))
            .expect("recent user");
        session
            .append_message(ChatMessage::assistant(
                "recent answer ".repeat(8_000),
                Vec::new(),
            ))
            .expect("recent answer");

        struct Sink {
            events: Vec<ProtocolEvent>,
            compaction_started: bool,
            compaction_finished: bool,
        }
        impl EventSink for Sink {
            fn emit_event(&mut self, event: &ProtocolEvent) -> io::Result<()> {
                self.events.push(event.clone());
                Ok(())
            }
            fn compaction_started(&mut self) -> io::Result<()> {
                self.compaction_started = true;
                Ok(())
            }
            fn compaction_finished(&mut self, _: usize, _: usize) -> io::Result<()> {
                self.compaction_finished = true;
                Ok(())
            }
        }

        let mut harness = Harness {
            home: std::env::temp_dir(),
            session,
            provider,
            context_window: Some(1),
            attached_agents: Vec::new(),
            subagents: Arc::new(Mutex::new(HashMap::new())),
            completed_subagents: mpsc::channel(),
        };
        let cancellation = CancellationToken::new();
        let mut sink = Sink {
            events: Vec::new(),
            compaction_started: false,
            compaction_finished: false,
        };
        harness
            .handle_message("continue", &mut sink, Some(&cancellation))
            .expect("continued turn");

        let requests = server.join().expect("server");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].get("tools").is_none());
        assert!(requests[1].get("tools").is_some());
        assert!(sink.compaction_started);
        assert!(sink.compaction_finished);
        assert!(sink.events.iter().any(
            |event| matches!(event, ProtocolEvent::AssistantDelta { text } if text == "continued")
        ));
        assert!(harness
            .session
            .history
            .iter()
            .any(|record| matches!(record, crate::session::SessionHistoryRecord::Compaction(_))));
        let provider_text = harness
            .session
            .provider_messages()
            .iter()
            .filter_map(|message| message.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!provider_text.contains("old request"));
        assert!(provider_text.contains("continue"));

        std::env::remove_var(key_env);
        std::fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn parses_only_message_records() {
        assert_eq!(
            parse_input_message(r#"{"type":"message","text":"hello"}"#).expect("message"),
            "hello"
        );
        assert!(parse_input_message(r#"{"type":"event","text":"hello"}"#).is_err());
        assert_eq!(
            parse_input_message(r#"{"type":"message","text":""}"#).expect("empty message"),
            ""
        );
    }

    #[test]
    fn resolves_terminal_and_forced_modes() {
        assert_eq!(
            resolve_mode(&[], true, true).expect("default TUI"),
            FrontendMode::Tui
        );
        assert_eq!(
            resolve_mode(&[], true, false).expect("automatic JSONL"),
            FrontendMode::Jsonl
        );
        assert_eq!(
            resolve_mode(&["--jsonl".to_owned()], true, true).expect("forced JSONL"),
            FrontendMode::Jsonl
        );
        assert!(resolve_mode(&["--tui".to_owned()], true, false).is_err());
    }

    #[test]
    fn redactor_does_not_leak_a_secret_across_deltas() {
        let mut redactor = SecretRedactor::new("secret");
        let mut output = Vec::new();
        redactor
            .push("prefix sec", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("push");
        redactor
            .push("ret suffix", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("push");
        redactor
            .finish(|text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("finish");
        let output = output.join("");
        assert_eq!(
            output,
            format!("prefix {} suffix", redaction_marker("secret").unwrap())
        );
        assert!(!output.contains("secret"));
    }

    #[test]
    fn redactor_handles_secrets_introduced_by_protocol_json_escaping() {
        let mut redactor = SecretRedactor::new("n0");
        let mut output = String::new();
        redactor
            .push("\n0", |text| {
                output.push_str(text);
                Ok(())
            })
            .expect("push");
        redactor
            .finish(|text| {
                output.push_str(text);
                Ok(())
            })
            .expect("finish");
        assert!(!output.contains("n0"));
        assert_eq!(output, redaction_marker("n0").unwrap());
    }

    #[test]
    fn redactor_does_not_emit_a_secret_when_it_completes_at_a_delta_boundary() {
        let mut redactor = SecretRedactor::new("secret");
        let mut output = Vec::new();
        redactor
            .push("xsecre", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("first delta");
        redactor
            .push("t", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("second delta");
        redactor
            .finish(|text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("finish");
        let output = output.join("");
        assert_eq!(output, format!("x{}", redaction_marker("secret").unwrap()));
        assert!(!output.contains("secret"));
    }

    #[test]
    fn streaming_redaction_handles_marker_collision_keys_at_delta_boundaries() {
        for secret in ["REDACTED", "[REDACTED]"] {
            let mut redactor = SecretRedactor::new(secret);
            let split = secret.len() / 2;
            let (first, second) = secret.split_at(split);
            let mut output = String::new();
            redactor
                .push(first, |text| {
                    output.push_str(text);
                    Ok(())
                })
                .expect("first delta");
            redactor
                .push(second, |text| {
                    output.push_str(text);
                    Ok(())
                })
                .expect("second delta");
            redactor
                .finish(|text| {
                    output.push_str(text);
                    Ok(())
                })
                .expect("finish");
            assert!(!output.contains(secret));
            assert!(output.len() <= secret.len());
        }
    }

    #[test]
    fn malformed_tool_arguments_use_a_safe_copy() {
        let secret = "provider-secret";
        let escaped = secret
            .chars()
            .map(|character| format!(r#"\u{:04x}"#, character as u32))
            .collect::<String>();
        let arguments = format!(r#"{{"command":"{escaped}""#);
        let safe = redact_tool_arguments(&arguments, secret);
        assert_eq!(safe, "{}");
        serde_json::from_str::<Value>(&safe).expect("safe arguments JSON");
        assert!(!safe.contains(secret));
        assert!(!safe.contains(&escaped));
        for invalid in ["[]", "{\"command\":1}", "{\"other\":\"value\"}"] {
            assert_eq!(redact_tool_arguments(invalid, secret), "{}");
        }
    }

    #[test]
    fn structured_redaction_preserves_tool_and_result_schema_keys() {
        let secret = "provider-secret";
        let value = serde_json::json!({
            "command": "printf provider-secret",
            "stdout": "provider-secret",
            "stderr": "ordinary",
            "exit_code": 0,
            "timed_out": false,
            "stdout_truncated": false,
            "stderr_truncated": false,
            "unknown-provider-secret": "provider-secret"
        });
        let redacted = redact_json_value(value, secret);
        for key in [
            "command",
            "stdout",
            "stderr",
            "exit_code",
            "timed_out",
            "stdout_truncated",
            "stderr_truncated",
        ] {
            assert!(redacted.get(key).is_some(), "missing schema key: {key}");
        }
        let encoded = serde_json::to_string(&redacted).expect("redacted JSON");
        assert!(!encoded.contains(secret));
        assert!(redacted.get("unknown-provider-secret").is_none());
    }

    #[test]
    fn structured_redaction_preserves_typed_values_even_for_a_pathological_key() {
        let value = serde_json::json!({
            "exit_code": 0,
            "timed_out": false,
            "stdout_truncated": true,
            "error": null,
        });
        let redacted = redact_json_value(value, "0");
        assert!(redacted["exit_code"].is_number());
        assert!(redacted["timed_out"].is_boolean());
        assert!(redacted["stdout_truncated"].is_boolean());
        assert!(redacted["error"].is_null());
    }

    #[test]
    fn reasoning_details_are_recursively_redacted_before_persistence() {
        let details = vec![serde_json::json!({
            "type": "reasoning.text",
            "text": "provider-secret",
            "nested": [{"value": "provider-secret"}],
            "provider-secret": "provider-secret"
        })];
        let redacted = redact_reasoning_details(&details, "provider-secret")
            .expect("non-empty reasoning details");
        let redacted = Value::Array(redacted);
        let encoded = serde_json::to_string(&redacted).expect("reasoning details JSON");
        assert!(!encoded.contains("provider-secret"));
        assert_eq!(redacted[0]["type"], "reasoning.text");
        assert_eq!(redacted[0]["text"], "[REDACTED]");
        assert_eq!(redacted[0]["nested"][0]["value"], "[REDACTED]");
        assert!(redacted[0].get("provider-secret").is_none());
    }

    #[test]
    fn malformed_input_error_does_not_echo_secret_bearing_input() {
        let error =
            parse_input_message(r#"{"type":"message","text":"provider-secret","unexpected":}"#)
                .expect_err("invalid input");
        assert!(!error.contains("provider-secret"));
    }

    #[test]
    fn malformed_input_is_an_error_event_and_not_diagnostic_json() {
        let mut output = Vec::new();
        let error = parse_input_message("not json").expect_err("invalid input");
        let mut protocol = ProtocolWriter::new(&mut output);
        protocol.error(&error).expect("error event");
        assert_eq!(String::from_utf8_lossy(&output).lines().count(), 1);
        let _ = Cursor::new("");
    }

    #[test]
    fn early_diagnostic_scrubbing_removes_short_values_from_the_complete_line() {
        let secret = "lucy";
        let mut diagnostics = Vec::new();
        write_diagnostic_safe_with_environment(
            &mut diagnostics,
            secret,
            None,
            vec![secret.to_owned()],
        );
        let diagnostics = String::from_utf8(diagnostics).expect("diagnostic UTF-8");
        assert!(!diagnostics.contains(secret));
    }
    #[test]
    fn attached_agents_keeps_only_agents_files_and_redacts_their_paths() {
        let sources = vec![
            InstructionSource {
                path: std::path::PathBuf::from("/project/AGENTS.md"),
                contents: "agents".to_owned(),
            },
            InstructionSource {
                path: std::path::PathBuf::from("/project/CLAUDE.md"),
                contents: "claude".to_owned(),
            },
            InstructionSource {
                path: std::path::PathBuf::from("/private-secret/AGENTS.md"),
                contents: "agents".to_owned(),
            },
        ];

        assert_eq!(
            attached_agents(sources, "secret"),
            vec!["/project/AGENTS.md", "/private-!/AGENTS.md"]
        );
    }

    #[test]
    fn expands_slash_prefixed_skill_names_and_keeps_ordinary_messages() {
        let skill = SkillEntry {
            name: "release-notes".to_owned(),
            description: "Writes release notes".to_owned(),
            path: std::path::PathBuf::from("/skills/release-notes/SKILL.md"),
            contents: "# Release notes\nUse the template.".to_owned(),
            model_invocable: true,
        };
        let expanded = expand_skill_invocation("/release-notes v1.2", std::slice::from_ref(&skill))
            .expect("skill command");
        assert!(expanded.text.contains("# Release notes"));
        assert!(expanded.text.contains("User: v1.2"));
        assert_eq!(expanded.attached_skill.as_deref(), Some("release-notes"));
        let ordinary = expand_skill_invocation("ordinary message", &[]).expect("ordinary message");
        assert_eq!(ordinary.text, "ordinary message");
        assert_eq!(ordinary.attached_skill, None);
        assert_eq!(
            expand_skill_invocation("/missing", &[]).unwrap_err(),
            "unknown skill: missing"
        );
        assert_eq!(
            expand_skill_invocation("/skill:release-notes", &[skill]).unwrap_err(),
            "unknown skill: skill:release-notes"
        );
    }
}
