mod base;
mod turn;

use std::path::{Path, PathBuf};

use crate::attention::{causal_events, latest_attention_head, routed_message_event};
use crate::attention_lock::AttentionLease;
use crate::config::LlmSettings;
use crate::context::{resolve_boot_context_with_api_key_env, SkillEntry};
use crate::journal::{Journal, JournalEvent};
use crate::model::{ChatMessage, OBSERVATION_ROLE};
use crate::redaction::redact_secret;

pub use base::{
    CompactionRecord, InterruptionRecord, SessionHistoryRecord, SessionMetadata, SessionToolResult,
};
pub use turn::{
    TurnEvent, TurnKind, TurnLifecycleRecord, TurnOutcome, TurnPhase, TurnState, TurnStatus,
};

const GLOBAL_ID: &str = "global";
const DEFAULT_SURFACE: &str = "cli";
const DEFAULT_SOURCE: &str = "main";
const INTERRUPTION_KIND: &str = "interruption";
const PROVIDER_SETTINGS_KIND: &str = "provider_settings";

#[derive(Debug)]
pub struct SessionError(String);

impl SessionError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SessionError {}

/// Compatibility façade used by the existing frontends while Lucy 2 removes
/// session UX. Persistent authority is the one global journal; no per-session
/// transcript, boot snapshot, lifecycle sidecar, or lifetime writer lease is
/// created by this type.
#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub saved_cwd: PathBuf,
    pub cwd_fallback: bool,
    pub boot_system_prompt: String,
    pub llm: LlmSettings,
    pub skills: Vec<SkillEntry>,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<ChatMessage>,
    pub history: Vec<SessionHistoryRecord>,
    pub(crate) home: PathBuf,
    pub(crate) head_id: Option<String>,
    pub(crate) turn_lifecycle: Vec<TurnLifecycleRecord>,
    pub(crate) attention_lease: Option<AttentionLease>,
    secret: Option<String>,
    surface: String,
    source_id: String,
}

impl Session {
    pub fn create(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
    ) -> Result<Self, SessionError> {
        let secret = std::env::var(&llm.api_key_env).ok();
        Self::create_with_secret(home, cwd, boot_system_prompt, llm, secret.as_deref())
    }

    pub fn create_with_secret(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
        secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::create_with_skills_and_secret(home, cwd, boot_system_prompt, llm, Vec::new(), secret)
    }

    pub fn create_with_skills_and_secret(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
        skills: Vec<SkillEntry>,
        secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        let cwd = std::fs::canonicalize(cwd)
            .map_err(|_| SessionError::new("unable to resolve working directory"))?;
        let journal = Journal::for_home(home);
        journal
            .recover_incomplete_tail()
            .map_err(SessionError::new)?;
        let mut session = Self {
            id: GLOBAL_ID.to_owned(),
            path: journal.path(),
            cwd: cwd.clone(),
            saved_cwd: cwd,
            cwd_fallback: false,
            boot_system_prompt,
            llm,
            skills,
            created_at: 0,
            updated_at: 0,
            messages: Vec::new(),
            history: Vec::new(),
            home: home.to_path_buf(),
            head_id: None,
            turn_lifecycle: Vec::new(),
            attention_lease: None,
            secret: secret.map(str::to_owned),
            surface: DEFAULT_SURFACE.to_owned(),
            source_id: DEFAULT_SOURCE.to_owned(),
        };
        session.reload_from_journal()?;
        Ok(session)
    }

    /// Legacy resume syntax now resolves only to Lucy's one global continuity.
    pub fn resume(home: &Path, id: &str) -> Result<Self, SessionError> {
        Self::resume_with_secret(home, id, None)
    }

    pub(crate) fn ensure_resumable(_home: &Path, id: &str) -> Result<(), SessionError> {
        if id == GLOBAL_ID {
            Ok(())
        } else {
            Err(SessionError::new("Lucy 2 has no named sessions"))
        }
    }

    pub fn resume_with_secret(
        home: &Path,
        id: &str,
        external_secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::ensure_resumable(home, id)?;
        let config = crate::config::Config::load_or_create(home)
            .map_err(|error| SessionError::new(error.to_string()))?;
        let llm = config
            .resolved_llm()
            .map_err(|error| SessionError::new(error.to_string()))?;
        let cwd = std::env::current_dir()
            .map_err(|_| SessionError::new("unable to resolve working directory"))?;
        let secret = external_secret
            .map(str::to_owned)
            .or_else(|| std::env::var(&llm.api_key_env).ok())
            .filter(|secret| !secret.is_empty());
        let context = resolve_boot_context_with_api_key_env(home, &cwd, None)
            .map_err(|error| SessionError::new(error.to_string()))?;
        let boot_system_prompt = redact_secret(&context.system_prompt, secret.as_deref());
        let skills = redact_skills(context.skills, secret.as_deref());
        Self::create_with_skills_and_secret(
            home,
            &cwd,
            boot_system_prompt,
            llm,
            skills,
            secret.as_deref(),
        )
    }

    pub fn list(home: &Path) -> Result<Vec<SessionMetadata>, SessionError> {
        Self::list_with_secret(home, None)
    }

    pub fn list_with_secret(
        home: &Path,
        _external_secret: Option<&str>,
    ) -> Result<Vec<SessionMetadata>, SessionError> {
        let journal = Journal::for_home(home);
        let events = journal.read_all().map_err(SessionError::new)?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let messages = current_messages_from_events(&events)?;
        let first_message = messages.iter().find_map(|message| message.content.clone());
        let last_message = messages
            .iter()
            .rev()
            .find_map(|message| message.content.clone());
        let last_user_message = messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .and_then(|message| message.content.clone());
        let last_assistant_message = messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .and_then(|message| message.content.clone());
        let created_at = events.first().map_or(0, |event| event.timestamp_ms / 1000);
        let updated_at = events
            .last()
            .map_or(created_at, |event| event.timestamp_ms / 1000);
        Ok(vec![SessionMetadata {
            record_type: "session",
            session_id: GLOBAL_ID.to_owned(),
            created_at,
            updated_at,
            cwd: std::env::current_dir()
                .ok()
                .map(|cwd| cwd.display().to_string())
                .unwrap_or_default(),
            first_message,
            last_message,
            last_user_message,
            last_assistant_message,
        }])
    }

    pub fn set_routing(&mut self, surface: impl Into<String>, source_id: impl Into<String>) {
        self.surface = surface.into();
        self.source_id = source_id.into();
    }

    pub fn has_history(&self) -> bool {
        !self.messages.is_empty() || self.head_id.is_some()
    }

    pub fn provider_messages(&self) -> Vec<ChatMessage> {
        let mut messages = Vec::with_capacity(self.messages.len() + 1);
        messages.push(ChatMessage::system(self.boot_system_prompt.clone()));
        messages.extend(self.messages.iter().cloned());

        let mut completed_tool_calls = std::collections::HashSet::new();
        for message in &messages {
            if message.role == "tool" {
                if let Some(id) = message.tool_call_id.as_deref() {
                    completed_tool_calls.insert(id.to_owned());
                }
            }
        }
        for message in &mut messages {
            if message.role == "assistant" && !message.tool_calls.is_empty() {
                message
                    .tool_calls
                    .retain(|call| completed_tool_calls.contains(&call.id));
            }
        }
        crate::tool_pruning::prune_old_tool_outputs(&messages)
    }

    pub fn validate_provider_settings(
        &self,
        model: &str,
        effort: Option<&str>,
    ) -> Result<(), SessionError> {
        let payload = serde_json::json!({"model": model, "effort": effort});
        self.reject_secret(&payload)
    }

    pub fn append_provider_settings(
        &mut self,
        model: String,
        effort: Option<String>,
    ) -> Result<(), SessionError> {
        let payload = serde_json::json!({"model": model, "effort": effort});
        self.reject_secret(&payload)?;
        let mut event =
            JournalEvent::new(PROVIDER_SETTINGS_KIND, payload).map_err(SessionError::new)?;
        self.decorate_event(&mut event);
        self.journal().append(&event).map_err(SessionError::new)?;
        self.updated_at = event.timestamp_ms / 1000;
        self.history.push(SessionHistoryRecord::ProviderSettings {
            timestamp: self.updated_at,
            model,
            effort,
        });
        Ok(())
    }

    /// Append a provider-visible message while maintaining the one global
    /// causal line and the existing turn-engine invariants.
    pub fn append_message(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        if message.role == "user" {
            return self.append_user_or_retry(message);
        }

        let starts_background = message.role == OBSERVATION_ROLE
            && self
                .latest_turn()
                .map_err(SessionError::new)?
                .is_none_or(|turn| !turn.is_pending());
        if starts_background {
            self.start_turn(TurnKind::Background)
                .map_err(SessionError::new)?;
        }

        self.append_raw_message(message.clone())?;
        let Some(turn) = self
            .latest_turn()
            .map_err(SessionError::new)?
            .filter(|turn| turn.is_pending())
        else {
            return Ok(());
        };

        if message.role == "assistant" {
            if message.tool_calls.is_empty() {
                self.finish_turn(&turn.turn_id, TurnOutcome::Completed, None)
                    .map_err(SessionError::new)?;
            } else {
                self.set_turn_phase(&turn.turn_id, TurnPhase::ExecutingTools)
                    .map_err(SessionError::new)?;
            }
        } else if message.role == "tool" {
            self.set_turn_phase(&turn.turn_id, TurnPhase::ProviderStream)
                .map_err(SessionError::new)?;
        }
        Ok(())
    }

    pub(crate) fn append_steering_message(
        &mut self,
        message: ChatMessage,
    ) -> Result<(), SessionError> {
        if message.role != "user" {
            return Err(SessionError::new("steering input must be a user message"));
        }
        let Some(_turn) = self
            .latest_turn()
            .map_err(SessionError::new)?
            .filter(|turn| turn.status == TurnStatus::Active)
        else {
            return Err(SessionError::new("Lucy has no active turn to steer"));
        };
        self.append_raw_message(message)
    }

    pub fn append_interruption(
        &mut self,
        mut interruption: InterruptionRecord,
    ) -> Result<(), SessionError> {
        let mut event = JournalEvent::new(INTERRUPTION_KIND, serde_json::json!({}))
            .map_err(SessionError::new)?;
        interruption.timestamp = event.timestamp_ms / 1000;
        event.payload = serde_json::to_value(&interruption)
            .map_err(|_| SessionError::new("unable to encode interruption"))?;
        self.reject_secret(&event.payload)?;
        self.decorate_event(&mut event);
        self.journal().append(&event).map_err(SessionError::new)?;
        self.history.push(SessionHistoryRecord::Interruption {
            timestamp: interruption.timestamp,
            reason: interruption.reason,
            phase: interruption.phase,
            assistant_text: interruption.assistant_text,
            tool_calls: interruption.tool_calls,
            tool_results: interruption.tool_results,
        });
        self.updated_at = event.timestamp_ms / 1000;
        if let Some(turn) = self.latest_turn().map_err(SessionError::new)? {
            if turn.is_pending() {
                self.finish_turn(&turn.turn_id, TurnOutcome::Interrupted, None)
                    .map_err(SessionError::new)?;
            }
        }
        self.release_attention();
        Ok(())
    }

    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_message: usize,
        tokens_before: usize,
    ) -> Result<(), SessionError> {
        #[cfg(test)]
        {
            if first_kept_message > self.messages.len() {
                return Err(SessionError::new("invalid context compaction boundary"));
            }
            self.messages.drain(..first_kept_message);
            self.history
                .push(SessionHistoryRecord::Compaction(CompactionRecord {
                    timestamp: self.updated_at,
                    summary,
                    first_kept_message,
                    tokens_before,
                }));
            return Ok(());
        }

        #[cfg(not(test))]
        {
            let _ = (summary, first_kept_message, tokens_before);
            Err(SessionError::new(
                "semantic compaction is disabled; provider context uses deterministic projection",
            ))
        }
    }

    pub(crate) fn ensure_attention(&mut self) -> Result<(), String> {
        if self.attention_lease.is_some() {
            return Ok(());
        }
        let lease = AttentionLease::acquire(&self.home)?;
        self.reload_from_journal()
            .map_err(|error| error.to_string())?;
        self.refresh_current_context()?;
        self.attention_lease = Some(lease);
        Ok(())
    }

    pub(crate) fn release_attention(&mut self) {
        self.attention_lease = None;
    }

    pub(crate) fn journal(&self) -> Journal {
        Journal::for_home(&self.home)
    }

    pub(crate) fn decorate_event(&self, event: &mut JournalEvent) {
        event.parent_id = self.head_id.clone();
        event.surface = Some(self.surface.clone());
        event.source_id = Some(self.source_id.clone());
        event.cwd = Some(self.cwd.display().to_string());
    }

    fn refresh_current_context(&mut self) -> Result<(), String> {
        let context = resolve_boot_context_with_api_key_env(&self.home, &self.cwd, None)
            .map_err(|error| error.to_string())?;
        self.boot_system_prompt = redact_secret(&context.system_prompt, self.secret.as_deref());
        self.skills = redact_skills(context.skills, self.secret.as_deref());
        Ok(())
    }

    fn append_user_or_retry(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        self.ensure_attention().map_err(SessionError::new)?;
        let pending = self
            .latest_turn()
            .map_err(SessionError::new)?
            .filter(|turn| turn.is_pending());
        let is_retry = message.content.as_deref().map(str::trim) == Some("!retry");

        if let Some(turn) = pending {
            if !is_retry {
                return Err(SessionError::new(
                    "Lucy has an unresolved turn; send !retry before starting a new message",
                ));
            }
            if turn.status == TurnStatus::Active {
                self.fail_turn_retryably(
                    &turn.turn_id,
                    "previous turn stopped before completion".to_owned(),
                )
                .map_err(SessionError::new)?;
                self.ensure_attention().map_err(SessionError::new)?;
            }
            self.resume_retryable_turn(&turn.turn_id)
                .map_err(SessionError::new)?;
            return Ok(());
        }
        if is_retry {
            self.release_attention();
            return Err(SessionError::new("Lucy has no pending turn to retry"));
        }

        let turn_id = self.start_turn(TurnKind::User).map_err(SessionError::new)?;
        if let Err(error) = self.append_raw_message(message) {
            let _ = self.finish_turn(
                &turn_id,
                TurnOutcome::TerminalFailure,
                Some(error.to_string()),
            );
            self.release_attention();
            return Err(error);
        }
        Ok(())
    }

    fn append_raw_message(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        if self.attention_lease.is_none() {
            self.ensure_attention().map_err(SessionError::new)?;
        }
        let payload = serde_json::to_value(&message)
            .map_err(|_| SessionError::new("unable to encode journal message"))?;
        self.reject_secret(&payload)?;
        let mut event = routed_message_event(
            message.clone(),
            &self.surface,
            &self.source_id,
            self.head_id.as_deref(),
        )
        .map_err(SessionError::new)?;
        event.cwd = Some(self.cwd.display().to_string());
        if let Some(turn) = self.latest_turn().map_err(SessionError::new)? {
            event.turn_id = Some(turn.turn_id);
        }
        self.journal().append(&event).map_err(SessionError::new)?;
        let timestamp = event.timestamp_ms / 1000;
        self.head_id = Some(event.id);
        self.messages.push(message.clone());
        self.history
            .push(SessionHistoryRecord::Message { timestamp, message });
        self.updated_at = timestamp;
        Ok(())
    }

    pub(crate) fn reload_from_journal(&mut self) -> Result<(), SessionError> {
        let events = self.journal().read_all().map_err(SessionError::new)?;
        self.head_id = latest_attention_head(&events).map(|event| event.id.clone());
        self.messages.clear();
        self.history.clear();
        self.turn_lifecycle.clear();

        if let Some(head) = self.head_id.as_deref() {
            for event in causal_events(&events, head).map_err(SessionError::new)? {
                if event.kind == crate::attention::MESSAGE_EVENT_KIND {
                    let message = serde_json::from_value::<ChatMessage>(event.payload.clone())
                        .map_err(|_| {
                            SessionError::new("journal message event has invalid payload")
                        })?;
                    let timestamp = event.timestamp_ms / 1000;
                    self.messages.push(message.clone());
                    self.history
                        .push(SessionHistoryRecord::Message { timestamp, message });
                }
            }
        }

        for event in &events {
            match event.kind.as_str() {
                turn::TURN_LIFECYCLE_KIND => {
                    let lifecycle =
                        serde_json::from_value::<TurnLifecycleRecord>(event.payload.clone())
                            .map_err(|_| SessionError::new("journal turn lifecycle is invalid"))?;
                    self.turn_lifecycle.push(lifecycle);
                }
                INTERRUPTION_KIND => {
                    let interruption =
                        serde_json::from_value::<InterruptionRecord>(event.payload.clone())
                            .map_err(|_| SessionError::new("journal interruption is invalid"))?;
                    self.history.push(SessionHistoryRecord::Interruption {
                        timestamp: event.timestamp_ms / 1000,
                        reason: interruption.reason,
                        phase: interruption.phase,
                        assistant_text: interruption.assistant_text,
                        tool_calls: interruption.tool_calls,
                        tool_results: interruption.tool_results,
                    });
                }
                _ => {}
            }
        }
        self.created_at = events.first().map_or(0, |event| event.timestamp_ms / 1000);
        self.updated_at = events
            .last()
            .map_or(self.created_at, |event| event.timestamp_ms / 1000);
        Ok(())
    }

    fn reject_secret(&self, value: &serde_json::Value) -> Result<(), SessionError> {
        let Some(secret) = self.secret.as_deref().filter(|secret| !secret.is_empty()) else {
            return Ok(());
        };
        let encoded = serde_json::to_string(value)
            .map_err(|_| SessionError::new("unable to validate journal record"))?;
        if encoded.contains(secret) {
            return Err(SessionError::new("journal record rejected"));
        }
        Ok(())
    }
}

fn redact_skills(skills: Vec<SkillEntry>, secret: Option<&str>) -> Vec<SkillEntry> {
    skills
        .into_iter()
        .map(|skill| SkillEntry {
            name: redact_secret(&skill.name, secret),
            description: redact_secret(&skill.description, secret),
            path: PathBuf::from(redact_secret(&skill.path.display().to_string(), secret)),
            contents: redact_secret(&skill.contents, secret),
            model_invocable: skill.model_invocable,
        })
        .collect()
}

fn current_messages_from_events(events: &[JournalEvent]) -> Result<Vec<ChatMessage>, SessionError> {
    let Some(head) = latest_attention_head(events) else {
        return Ok(Vec::new());
    };
    causal_events(events, &head.id)
        .map_err(SessionError::new)?
        .into_iter()
        .filter(|event| event.kind == crate::attention::MESSAGE_EVENT_KIND)
        .map(|event| {
            serde_json::from_value::<ChatMessage>(event.payload.clone())
                .map_err(|_| SessionError::new("journal message event has invalid payload"))
        })
        .collect()
}
