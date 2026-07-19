use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    ensure_not_symlink, ensure_private_dir, ensure_private_file, lucy_dir, LlmSettings,
};
use crate::context::SkillEntry;
use crate::model::{ChatMessage, ChatToolCall};
use crate::redaction::{conflicts_with_protected_literal, redact_secret};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

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

impl From<io::Error> for SessionError {
    fn from(_error: io::Error) -> Self {
        Self::new("session storage error")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionStatus {
    Running,
    Completed,
    Failed,
    Canceled,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record")]
enum ChildSessionRecord {
    #[serde(rename = "subagent_session")]
    Session {
        version: u8,
        session_id: String,
        session_kind: String,
        parent_session_id: String,
        cwd: String,
        boot_system_prompt: String,
        llm: LlmSettings,
        task: String,
    },
    #[serde(rename = "message")]
    Message {
        timestamp: u64,
        message: ChatMessage,
    },
    #[serde(rename = "status")]
    Status {
        timestamp: u64,
        status: ChildSessionStatus,
        reason: Option<String>,
        result: Option<Value>,
    },
}

#[derive(Debug)]
pub struct ChildSession {
    pub id: String,
    pub path: PathBuf,
    pub parent_session_id: String,
    pub cwd: PathBuf,
    pub boot_system_prompt: String,
    pub llm: LlmSettings,
    pub task: String,
    pub messages: Vec<ChatMessage>,
    pub status: ChildSessionStatus,
    secret: Option<String>,
}

impl ChildSession {
    pub fn create(
        home: &Path,
        parent_session_id: &str,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
        task: String,
        secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        let cwd = fs::canonicalize(cwd)
            .map_err(|_| SessionError::new("unable to resolve subagent cwd"))?;
        let directory = sessions_dir(home);
        ensure_private_dir(&lucy_dir(home))?;
        ensure_private_dir(&directory)?;
        let id = format!("subagent-{}", new_session_id());
        let path = directory.join(format!("{id}.jsonl"));
        let record = ChildSessionRecord::Session {
            version: 1,
            session_id: id.clone(),
            session_kind: "subagent".to_owned(),
            parent_session_id: parent_session_id.to_owned(),
            cwd: cwd.display().to_string(),
            boot_system_prompt: boot_system_prompt.clone(),
            llm: llm.clone(),
            task: task.clone(),
        };
        if let Some(secret) = secret {
            if child_record_contains_secret(&record, secret) {
                return Err(SessionError::new("subagent session record rejected"));
            }
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&path)
            .map_err(|_| SessionError::new("unable to create subagent session file"))?;
        ensure_private_file(&path)?;
        write_json_record(&mut file, &record)?;
        write_json_record(
            &mut file,
            &ChildSessionRecord::Status {
                timestamp: now(),
                status: ChildSessionStatus::Running,
                reason: None,
                result: None,
            },
        )?;
        Ok(Self {
            id,
            path,
            parent_session_id: parent_session_id.to_owned(),
            cwd,
            boot_system_prompt,
            llm,
            task,
            messages: Vec::new(),
            status: ChildSessionStatus::Running,
            secret: secret.map(str::to_owned),
        })
    }

    pub fn append_message(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        let record = ChildSessionRecord::Message {
            timestamp: now(),
            message: message.clone(),
        };
        if self
            .secret
            .as_deref()
            .is_some_and(|secret| child_record_contains_secret(&record, secret))
        {
            return Err(SessionError::new("subagent session record rejected"));
        }
        let mut file = open_session_for_append(&self.path)?;
        write_json_record(&mut file, &record)?;
        self.messages.push(message);
        Ok(())
    }

    pub fn append_status(
        &mut self,
        status: ChildSessionStatus,
        reason: Option<String>,
        result: Option<Value>,
    ) -> Result<(), SessionError> {
        let record = ChildSessionRecord::Status {
            timestamp: now(),
            status,
            reason,
            result,
        };
        if self
            .secret
            .as_deref()
            .is_some_and(|secret| child_record_contains_secret(&record, secret))
        {
            return Err(SessionError::new("subagent session record rejected"));
        }
        let mut file = open_session_for_append(&self.path)?;
        write_json_record(&mut file, &record)?;
        self.status = status;
        Ok(())
    }

    pub fn provider_messages(&self) -> Vec<ChatMessage> {
        let mut messages = Vec::with_capacity(self.messages.len() + 1);
        messages.push(ChatMessage::system(self.boot_system_prompt.clone()));
        messages.extend(self.messages.iter().cloned());
        messages
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record")]
enum SessionRecord {
    #[serde(rename = "session")]
    Session {
        version: u8,
        session_id: String,
        created_at: u64,
        cwd: String,
        boot_system_prompt: String,
        llm: LlmSettings,
        #[serde(default)]
        skills: Vec<SkillEntry>,
    },
    #[serde(rename = "provider_settings")]
    ProviderSettings {
        timestamp: u64,
        model: String,
        effort: Option<String>,
    },
    #[serde(rename = "message")]
    Message {
        timestamp: u64,
        message: ChatMessage,
    },
    #[serde(rename = "interruption")]
    Interruption {
        timestamp: u64,
        reason: String,
        phase: String,
        #[serde(default)]
        assistant_text: String,
        #[serde(default)]
        tool_calls: Vec<ChatToolCall>,
        #[serde(default)]
        tool_results: Vec<SessionToolResult>,
    },
    #[serde(rename = "compaction")]
    Compaction {
        timestamp: u64,
        summary: String,
        first_kept_message: usize,
        tokens_before: usize,
    },
    #[serde(rename = "background_result_pending")]
    BackgroundResultPending(BackgroundResultPending),
    #[serde(rename = "background_result_delivered")]
    BackgroundResultDelivered(BackgroundResultDelivered),
}

/// A bounded, secret-safe observation retained for a canceled tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionToolResult {
    pub id: String,
    pub name: String,
    pub result: Value,
}

/// The safe observations written when a user stops an active turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterruptionRecord {
    #[serde(default)]
    pub timestamp: u64,
    pub reason: String,
    pub phase: String,
    #[serde(default)]
    pub assistant_text: String,
    #[serde(default)]
    pub tool_calls: Vec<ChatToolCall>,
    #[serde(default)]
    pub tool_results: Vec<SessionToolResult>,
}

/// A durable summary boundary that lets provider context shrink without
/// rewriting the append-only session history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionRecord {
    pub timestamp: u64,
    pub summary: String,
    /// Message ordinal (excluding the system prompt) at which retained context
    /// begins. Messages before this boundary remain available for replay only.
    pub first_kept_message: usize,
    pub tokens_before: usize,
}

pub const BACKGROUND_RESULT_TOOL_NAME: &str = "background_result";

/// The parent-owned source of truth for one terminal child result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackgroundResultPending {
    pub timestamp: u64,
    pub completion_id: String,
    pub task_id: String,
    pub child_session_id: String,
    pub task: String,
    pub status: ChildSessionStatus,
    pub result: Value,
    pub completed_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundResultDelivery {
    Synthetic,
    WaitSubagent,
}

/// An append-only commit marker. Its history position is the provider-context
/// position at which an automatic synthetic observation is materialized.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackgroundResultDelivered {
    pub timestamp: u64,
    pub completion_id: String,
    pub logical_turn_id: String,
    pub delivery: BackgroundResultDelivery,
}

/// The ordered, replayable records after a session header.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "record")]
pub enum SessionHistoryRecord {
    #[serde(rename = "provider_settings")]
    ProviderSettings {
        timestamp: u64,
        model: String,
        effort: Option<String>,
    },
    #[serde(rename = "message")]
    Message {
        timestamp: u64,
        message: ChatMessage,
    },
    #[serde(rename = "interruption")]
    Interruption {
        timestamp: u64,
        reason: String,
        phase: String,
        assistant_text: String,
        tool_calls: Vec<ChatToolCall>,
        tool_results: Vec<SessionToolResult>,
    },
    #[serde(rename = "compaction")]
    Compaction(CompactionRecord),
    #[serde(rename = "background_result_pending")]
    BackgroundResultPending(BackgroundResultPending),
    #[serde(rename = "background_result_delivered")]
    BackgroundResultDelivered(BackgroundResultDelivered),
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub boot_system_prompt: String,
    pub llm: LlmSettings,
    /// Skills are immutable per-session just like the boot prompt, so a
    /// resumed `/<name>` skill command cannot silently load changed files.
    pub skills: Vec<SkillEntry>,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<ChatMessage>,
    pub history: Vec<SessionHistoryRecord>,
    secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionMetadata {
    #[serde(rename = "type")]
    pub record_type: &'static str,
    pub session_id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub first_message: Option<String>,
    pub last_message: Option<String>,
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
        let cwd = fs::canonicalize(cwd)
            .map_err(|_error| SessionError::new("unable to resolve session cwd"))?;
        let sessions_directory = sessions_dir(home);
        ensure_private_dir(&lucy_dir(home))?;
        ensure_private_dir(&sessions_directory)?;
        let created_at = now();

        if let Some(secret) = secret {
            if conflicts_with_protected_literal(secret) {
                return Err(session_header_rejected(secret));
            }
        }

        for _ in 0..16 {
            let id = new_session_id();
            let path = sessions_directory.join(format!("{id}.jsonl"));
            let record = SessionRecord::Session {
                version: 1,
                session_id: id.clone(),
                created_at,
                cwd: cwd.display().to_string(),
                boot_system_prompt: boot_system_prompt.clone(),
                llm: llm.clone(),
                skills: skills.clone(),
            };
            if let Some(secret) = secret {
                if record_contains_secret(&record, secret) {
                    return Err(session_header_rejected(secret));
                }
            }

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    ensure_private_file(&path)?;
                    write_record(&mut file, &record)?;
                    return Ok(Self {
                        id,
                        path,
                        cwd,
                        boot_system_prompt,
                        llm,
                        skills,
                        created_at,
                        updated_at: created_at,
                        messages: Vec::new(),
                        history: Vec::new(),
                        secret: secret.map(str::to_owned),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_error) => return Err(SessionError::new("unable to create session file")),
            }
        }
        Err(SessionError::new("unable to allocate a unique session id"))
    }

    pub fn resume(home: &Path, id: &str) -> Result<Self, SessionError> {
        validate_session_id(id)?;
        let directory = sessions_dir(home);
        let lucy_directory = lucy_dir(home);
        ensure_not_symlink(&lucy_directory)?;
        if lucy_directory.is_dir() {
            ensure_private_dir(&lucy_directory)?;
        }
        ensure_not_symlink(&directory)?;
        if directory.is_dir() {
            ensure_private_dir(&directory)?;
        }
        let path = directory.join(format!("{id}.jsonl"));
        ensure_not_symlink(&path)?;
        if !path.is_file() {
            return Err(SessionError::new("session not found"));
        }

        ensure_private_file(&path)?;
        let raw =
            fs::read(&path).map_err(|_error| SessionError::new("unable to read session file"))?;
        let active_secret = session_header_secret(&raw);
        if let Some(secret) = active_secret.as_deref() {
            if conflicts_with_protected_literal(secret) || bytes_contain_secret(&raw, secret) {
                return Err(session_header_rejected(secret));
            }
        }

        let reader = BufReader::new(raw.as_slice());
        let mut header = None;
        let mut messages = Vec::new();
        let mut history = Vec::new();
        let mut updated_at = None;

        for (line_number, line) in reader.lines().enumerate() {
            let line = line.map_err(|_error| {
                session_error("unable to read session file", active_secret.as_deref())
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let value = parse_json_value(&line).map_err(|_error| {
                session_error(
                    format!("invalid session record at line {}", line_number + 1),
                    active_secret.as_deref(),
                )
            })?;
            if let Some(secret) = active_secret.as_deref() {
                if json_value_contains_secret(&value, secret) {
                    return Err(session_header_rejected(secret));
                }
            }
            let record: SessionRecord = serde_json::from_value(value).map_err(|_error| {
                session_error(
                    format!("invalid session record at line {}", line_number + 1),
                    active_secret.as_deref(),
                )
            })?;
            if let Some(secret) = active_secret.as_deref() {
                if record_contains_secret(&record, secret) {
                    return Err(session_header_rejected(secret));
                }
            }
            match record {
                SessionRecord::Session {
                    version,
                    session_id,
                    created_at,
                    cwd,
                    boot_system_prompt,
                    llm,
                    skills,
                } => {
                    if version != 1 || session_id != id || header.is_some() {
                        return Err(session_error(
                            "invalid session header",
                            active_secret.as_deref(),
                        ));
                    }
                    header = Some((created_at, cwd, boot_system_prompt, llm, skills));
                }
                SessionRecord::ProviderSettings {
                    timestamp,
                    model,
                    effort,
                } => {
                    if header.is_none() {
                        return Err(session_error(
                            "session provider settings precede header",
                            active_secret.as_deref(),
                        ));
                    }
                    updated_at = Some(timestamp);
                    history.push(SessionHistoryRecord::ProviderSettings {
                        timestamp,
                        model,
                        effort,
                    });
                }
                SessionRecord::Message { timestamp, message } => {
                    if header.is_none() {
                        return Err(session_error(
                            "session message precedes header",
                            active_secret.as_deref(),
                        ));
                    }
                    updated_at = Some(timestamp);
                    history.push(SessionHistoryRecord::Message {
                        timestamp,
                        message: message.clone(),
                    });
                    messages.push(message);
                }
                SessionRecord::Interruption {
                    timestamp,
                    reason,
                    phase,
                    assistant_text,
                    tool_calls,
                    tool_results,
                } => {
                    if header.is_none() {
                        return Err(session_error(
                            "session interruption precedes header",
                            active_secret.as_deref(),
                        ));
                    }
                    updated_at = Some(timestamp);
                    history.push(SessionHistoryRecord::Interruption {
                        timestamp,
                        reason,
                        phase,
                        assistant_text,
                        tool_calls,
                        tool_results,
                    });
                }
                SessionRecord::Compaction {
                    timestamp,
                    summary,
                    first_kept_message,
                    tokens_before,
                } => {
                    if header.is_none() {
                        return Err(session_error(
                            "session compaction precedes header",
                            active_secret.as_deref(),
                        ));
                    }
                    updated_at = Some(timestamp);
                    history.push(SessionHistoryRecord::Compaction(CompactionRecord {
                        timestamp,
                        summary,
                        first_kept_message,
                        tokens_before,
                    }));
                }
                SessionRecord::BackgroundResultPending(pending) => {
                    if header.is_none() {
                        return Err(session_error(
                            "background result precedes header",
                            active_secret.as_deref(),
                        ));
                    }
                    if history.iter().any(|record| {
                        matches!(
                            record,
                            SessionHistoryRecord::BackgroundResultPending(existing)
                                if existing.completion_id == pending.completion_id
                        )
                    }) {
                        return Err(session_error(
                            "duplicate pending background result",
                            active_secret.as_deref(),
                        ));
                    }
                    updated_at = Some(pending.timestamp);
                    history.push(SessionHistoryRecord::BackgroundResultPending(pending));
                }
                SessionRecord::BackgroundResultDelivered(delivered) => {
                    if header.is_none()
                        || !history.iter().any(|record| {
                            matches!(
                                record,
                                SessionHistoryRecord::BackgroundResultPending(pending)
                                    if pending.completion_id == delivered.completion_id
                            )
                        })
                        || history.iter().any(|record| {
                            matches!(
                                record,
                                SessionHistoryRecord::BackgroundResultDelivered(existing)
                                    if existing.completion_id == delivered.completion_id
                            )
                        })
                    {
                        return Err(session_error(
                            "invalid delivered background result",
                            active_secret.as_deref(),
                        ));
                    }
                    updated_at = Some(delivered.timestamp);
                    history.push(SessionHistoryRecord::BackgroundResultDelivered(delivered));
                }
            }
        }

        let message_count = messages.len();
        if history.iter().any(|record| {
            matches!(
                record,
                SessionHistoryRecord::Compaction(compaction)
                    if compaction.first_kept_message > message_count
            )
        }) {
            return Err(session_error(
                "invalid compaction boundary",
                active_secret.as_deref(),
            ));
        }

        let Some((created_at, cwd, boot_system_prompt, llm, skills)) = header else {
            return Err(session_error(
                "session has no header",
                active_secret.as_deref(),
            ));
        };
        let cwd = PathBuf::from(cwd);
        Ok(Self {
            id: id.to_owned(),
            path,
            cwd,
            boot_system_prompt,
            llm,
            skills,
            created_at,
            updated_at: updated_at.unwrap_or(created_at),
            messages,
            history,
            secret: active_secret,
        })
    }

    pub fn append_provider_settings(
        &mut self,
        model: String,
        effort: Option<String>,
    ) -> Result<(), SessionError> {
        let timestamp = now();
        let record = SessionRecord::ProviderSettings {
            timestamp,
            model: model.clone(),
            effort: effort.clone(),
        };
        if let Some(secret) = self.secret.as_deref() {
            if record_contains_secret(&record, secret) {
                return Err(session_record_rejected(secret));
            }
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.history.push(SessionHistoryRecord::ProviderSettings {
            timestamp,
            model,
            effort,
        });
        self.updated_at = timestamp;
        Ok(())
    }

    pub fn append_message(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        let timestamp = now();
        let record = SessionRecord::Message {
            timestamp,
            message: message.clone(),
        };
        if let Some(secret) = self.secret.as_deref() {
            if record_contains_secret(&record, secret) {
                return Err(session_record_rejected(secret));
            }
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.messages.push(message.clone());
        self.history
            .push(SessionHistoryRecord::Message { timestamp, message });
        self.updated_at = timestamp;
        Ok(())
    }

    pub fn append_interruption(
        &mut self,
        mut interruption: InterruptionRecord,
    ) -> Result<(), SessionError> {
        let timestamp = now();
        interruption.timestamp = timestamp;
        let record = SessionRecord::Interruption {
            timestamp,
            reason: interruption.reason.clone(),
            phase: interruption.phase.clone(),
            assistant_text: interruption.assistant_text.clone(),
            tool_calls: interruption.tool_calls.clone(),
            tool_results: interruption.tool_results.clone(),
        };
        if let Some(secret) = self.secret.as_deref() {
            if record_contains_secret(&record, secret) {
                return Err(session_record_rejected(secret));
            }
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.history.push(SessionHistoryRecord::Interruption {
            timestamp,
            reason: interruption.reason,
            phase: interruption.phase,
            assistant_text: interruption.assistant_text,
            tool_calls: interruption.tool_calls,
            tool_results: interruption.tool_results,
        });
        self.updated_at = timestamp;
        Ok(())
    }

    pub fn append_background_result_pending(
        &mut self,
        mut pending: BackgroundResultPending,
    ) -> Result<bool, SessionError> {
        if let Some(existing) = self.history.iter().find_map(|record| match record {
            SessionHistoryRecord::BackgroundResultPending(existing)
                if existing.completion_id == pending.completion_id =>
            {
                Some(existing)
            }
            _ => None,
        }) {
            let same_completion = existing.task_id == pending.task_id
                && existing.child_session_id == pending.child_session_id
                && existing.task == pending.task
                && existing.status == pending.status
                && existing.result == pending.result
                && existing.completed_at == pending.completed_at;
            return if same_completion {
                Ok(false)
            } else {
                Err(SessionError::new("background result identity collision"))
            };
        }
        pending.timestamp = now();
        let record = SessionRecord::BackgroundResultPending(pending.clone());
        if self
            .secret
            .as_deref()
            .is_some_and(|secret| record_contains_secret(&record, secret))
        {
            return Err(session_record_rejected(
                self.secret.as_deref().unwrap_or_default(),
            ));
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.updated_at = pending.timestamp;
        self.history
            .push(SessionHistoryRecord::BackgroundResultPending(pending));
        Ok(true)
    }

    pub fn append_background_result_delivered(
        &mut self,
        completion_id: &str,
        logical_turn_id: String,
        delivery: BackgroundResultDelivery,
    ) -> Result<bool, SessionError> {
        let has_pending = self.history.iter().any(|record| {
            matches!(
                record,
                SessionHistoryRecord::BackgroundResultPending(pending)
                    if pending.completion_id == completion_id
            )
        });
        if !has_pending {
            return Err(SessionError::new("background result has no pending record"));
        }
        if self.history.iter().any(|record| {
            matches!(
                record,
                SessionHistoryRecord::BackgroundResultDelivered(existing)
                    if existing.completion_id == completion_id
            )
        }) {
            return Ok(false);
        }
        let delivered = BackgroundResultDelivered {
            timestamp: now(),
            completion_id: completion_id.to_owned(),
            logical_turn_id,
            delivery,
        };
        let record = SessionRecord::BackgroundResultDelivered(delivered.clone());
        if self
            .secret
            .as_deref()
            .is_some_and(|secret| record_contains_secret(&record, secret))
        {
            return Err(session_record_rejected(
                self.secret.as_deref().unwrap_or_default(),
            ));
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.updated_at = delivered.timestamp;
        self.history
            .push(SessionHistoryRecord::BackgroundResultDelivered(delivered));
        Ok(true)
    }

    pub fn undelivered_background_results(&self) -> Vec<BackgroundResultPending> {
        let delivered = self
            .history
            .iter()
            .filter_map(|record| match record {
                SessionHistoryRecord::BackgroundResultDelivered(delivered) => {
                    Some(delivered.completion_id.as_str())
                }
                _ => None,
            })
            .collect::<HashSet<_>>();
        self.history
            .iter()
            .filter_map(|record| match record {
                SessionHistoryRecord::BackgroundResultPending(pending)
                    if !delivered.contains(pending.completion_id.as_str()) =>
                {
                    Some(pending.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Append a summary boundary without deleting the historical records that
    /// preceded it. `first_kept_message` counts ordinary message records from
    /// the start of the session, excluding the boot system prompt.
    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_message: usize,
        tokens_before: usize,
    ) -> Result<(), SessionError> {
        let timestamp = now();
        let record = SessionRecord::Compaction {
            timestamp,
            summary: summary.clone(),
            first_kept_message,
            tokens_before,
        };
        if let Some(secret) = self.secret.as_deref() {
            if record_contains_secret(&record, secret) {
                return Err(session_record_rejected(secret));
            }
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.history
            .push(SessionHistoryRecord::Compaction(CompactionRecord {
                timestamp,
                summary,
                first_kept_message,
                tokens_before,
            }));
        self.updated_at = timestamp;
        Ok(())
    }

    pub fn provider_messages(&self) -> Vec<ChatMessage> {
        let latest_compaction = self.history.iter().rev().find_map(|record| match record {
            SessionHistoryRecord::Compaction(compaction) => Some(compaction),
            _ => None,
        });
        let first_kept_message = latest_compaction.map(|compaction| compaction.first_kept_message);
        let interruption_results = self
            .history
            .iter()
            .filter_map(|record| match record {
                SessionHistoryRecord::Interruption {
                    phase,
                    tool_results,
                    ..
                } if phase == "cmd" => Some(tool_results),
                _ => None,
            })
            .flatten()
            .count();
        let mut messages = Vec::with_capacity(
            self.messages.len()
                + 1
                + interruption_results
                + usize::from(latest_compaction.is_some()),
        );
        let mut declared_tool_calls = HashSet::new();
        let mut completed_tool_calls = HashSet::new();
        let mut pending_background_results = std::collections::HashMap::new();
        messages.push(ChatMessage::system(self.boot_system_prompt.clone()));
        if let Some(compaction) = latest_compaction {
            messages.push(compaction_summary_message(&compaction.summary));
        }

        let mut message_ordinal = 0usize;
        for record in &self.history {
            match record {
                SessionHistoryRecord::Message { message, .. } => {
                    let include =
                        first_kept_message.is_none_or(|boundary| message_ordinal >= boundary);
                    message_ordinal += 1;
                    if !include {
                        continue;
                    }
                    if message.role == "assistant" {
                        declared_tool_calls
                            .extend(message.tool_calls.iter().map(|call| call.id.clone()));
                    }
                    if message.role == "tool" {
                        if let Some(id) = message.tool_call_id.as_deref() {
                            completed_tool_calls.insert(id.to_owned());
                        }
                    }
                    messages.push(message.clone());
                }
                SessionHistoryRecord::Interruption {
                    phase,
                    tool_results,
                    ..
                } if phase == "cmd" => {
                    for observation in tool_results {
                        if !declared_tool_calls.contains(&observation.id)
                            || completed_tool_calls.contains(&observation.id)
                        {
                            continue;
                        }
                        let Ok(content) = serde_json::to_string(&observation.result) else {
                            continue;
                        };
                        messages.push(ChatMessage::tool(
                            observation.id.clone(),
                            observation.name.clone(),
                            content,
                        ));
                        completed_tool_calls.insert(observation.id.clone());
                    }
                }
                SessionHistoryRecord::BackgroundResultPending(pending) => {
                    pending_background_results
                        .insert(pending.completion_id.clone(), pending.clone());
                }
                SessionHistoryRecord::BackgroundResultDelivered(delivered)
                    if delivered.delivery == BackgroundResultDelivery::Synthetic
                        && first_kept_message
                            .is_none_or(|boundary| message_ordinal >= boundary) =>
                {
                    if let Some(pending) = pending_background_results.get(&delivered.completion_id)
                    {
                        messages.extend(background_result_messages(pending));
                    }
                }
                SessionHistoryRecord::ProviderSettings { .. }
                | SessionHistoryRecord::Interruption { .. }
                | SessionHistoryRecord::Compaction(_)
                | SessionHistoryRecord::BackgroundResultDelivered(_) => {}
            }
        }
        messages
    }

    pub fn list(home: &Path) -> Result<Vec<SessionMetadata>, SessionError> {
        let directory = sessions_dir(home);
        let lucy_directory = lucy_dir(home);
        ensure_not_symlink(&lucy_directory)?;
        if lucy_directory.is_dir() {
            ensure_private_dir(&lucy_directory)?;
        }
        ensure_not_symlink(&directory)?;
        if directory.is_dir() {
            ensure_private_dir(&directory)?;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_error) => return Err(SessionError::new("unable to list sessions")),
        };

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
                && metadata.is_file()
            {
                paths.push(path);
            }
        }
        paths.sort();

        let mut metadata = Vec::new();
        for path in paths {
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(session) = Self::resume(home, id) else {
                continue;
            };
            metadata.push(SessionMetadata {
                record_type: "session_metadata",
                session_id: session.id,
                created_at: session.created_at,
                updated_at: session.updated_at,
                first_message: session.messages.first().map(safe_message_summary),
                last_message: session.messages.last().map(safe_message_summary),
            });
        }
        Ok(metadata)
    }
}

struct DuplicateKeyValue(Value);

impl<'de> Deserialize<'de> for DuplicateKeyValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer
            .deserialize_any(DuplicateKeyValueVisitor)
            .map(Self)
    }
}

struct DuplicateKeyValueSeed;

impl<'de> DeserializeSeed<'de> for DuplicateKeyValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyValueVisitor)
    }
}

struct DuplicateKeyValueVisitor;

impl<'de> Visitor<'de> for DuplicateKeyValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a valid JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_i128(value)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("JSON number out of range"))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_u128(value)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("JSON number out of range"))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(DuplicateKeyValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate object key"));
            }
            let value = access.next_value_seed(DuplicateKeyValueSeed)?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

fn parse_json_value(line: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str::<DuplicateKeyValue>(line).map(|value| value.0)
}

fn session_header_secret(raw: &[u8]) -> Option<String> {
    let line = raw
        .split(|byte| *byte == b'\n')
        .find(|line| !line.iter().all(|byte| byte.is_ascii_whitespace()))?;
    let value = parse_json_value(std::str::from_utf8(line).ok()?).ok()?;
    let api_key_env = value
        .get("llm")
        .and_then(|llm| llm.get("api_key_env"))
        .and_then(Value::as_str)?;
    let secret = std::env::var(api_key_env).ok()?;
    (!secret.is_empty()).then_some(secret)
}

fn bytes_contain_secret(raw: &[u8], secret: &str) -> bool {
    let secret = secret.as_bytes();
    !secret.is_empty() && raw.windows(secret.len()).any(|window| window == secret)
}

fn record_contains_secret(record: &SessionRecord, secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    if serde_json::to_vec(record)
        .ok()
        .is_some_and(|serialized| bytes_contain_secret(&serialized, secret))
    {
        return true;
    }
    match record {
        SessionRecord::Session {
            version,
            session_id,
            created_at,
            cwd,
            boot_system_prompt,
            llm,
            skills,
        } => {
            version.to_string().contains(secret)
                || session_id.contains(secret)
                || created_at.to_string().contains(secret)
                || cwd.contains(secret)
                || boot_system_prompt.contains(secret)
                || llm.base_url.contains(secret)
                || llm.model.contains(secret)
                || llm.api_key_env.contains(secret)
                || skills.iter().any(|skill| {
                    skill.name.contains(secret)
                        || skill.description.contains(secret)
                        || skill.path.display().to_string().contains(secret)
                        || skill.contents.contains(secret)
                })
        }
        SessionRecord::ProviderSettings {
            timestamp,
            model,
            effort,
        } => {
            timestamp.to_string().contains(secret)
                || model.contains(secret)
                || effort
                    .as_deref()
                    .is_some_and(|value| value.contains(secret))
        }
        SessionRecord::Message { timestamp, message } => {
            timestamp.to_string().contains(secret) || message_contains_secret(message, secret)
        }
        SessionRecord::Interruption {
            timestamp,
            reason,
            phase,
            assistant_text,
            tool_calls,
            tool_results,
        } => {
            timestamp.to_string().contains(secret)
                || reason.contains(secret)
                || phase.contains(secret)
                || assistant_text.contains(secret)
                || tool_calls.iter().any(|call| {
                    call.id.contains(secret)
                        || call.name.contains(secret)
                        || call.arguments.contains(secret)
                })
                || tool_results.iter().any(|observation| {
                    observation.id.contains(secret)
                        || observation.name.contains(secret)
                        || json_value_contains_secret(&observation.result, secret)
                })
        }
        SessionRecord::Compaction {
            timestamp,
            summary,
            first_kept_message,
            tokens_before,
        } => {
            timestamp.to_string().contains(secret)
                || summary.contains(secret)
                || first_kept_message.to_string().contains(secret)
                || tokens_before.to_string().contains(secret)
        }
        SessionRecord::BackgroundResultPending(pending) => {
            pending.timestamp.to_string().contains(secret)
                || pending.completion_id.contains(secret)
                || pending.task_id.contains(secret)
                || pending.child_session_id.contains(secret)
                || pending.task.contains(secret)
                || json_value_contains_secret(&pending.result, secret)
                || pending.completed_at.to_string().contains(secret)
        }
        SessionRecord::BackgroundResultDelivered(delivered) => {
            delivered.timestamp.to_string().contains(secret)
                || delivered.completion_id.contains(secret)
                || delivered.logical_turn_id.contains(secret)
        }
    }
}

fn message_contains_secret(message: &ChatMessage, secret: &str) -> bool {
    message.role.contains(secret)
        || message
            .content
            .as_deref()
            .is_some_and(|content| content.contains(secret))
        || message.reasoning_details.as_ref().is_some_and(|details| {
            details
                .iter()
                .any(|detail| json_value_contains_secret(detail, secret))
        })
        || message
            .name
            .as_deref()
            .is_some_and(|name| name.contains(secret))
        || message
            .tool_call_id
            .as_deref()
            .is_some_and(|id| id.contains(secret))
        || message.tool_calls.iter().any(|call| {
            call.id.contains(secret)
                || call.name.contains(secret)
                || call.arguments.contains(secret)
                || tool_arguments_contain_secret(&call.arguments, secret)
        })
}

fn tool_arguments_contain_secret(arguments: &str, secret: &str) -> bool {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .is_some_and(|value| json_value_contains_secret(&value, secret))
}

fn json_value_contains_secret(value: &Value, secret: &str) -> bool {
    match value {
        Value::String(text) => text.contains(secret),
        Value::Array(values) => values
            .iter()
            .any(|value| json_value_contains_secret(value, secret)),
        Value::Object(object) => {
            object.keys().any(|key| key.contains(secret))
                || object
                    .values()
                    .any(|value| json_value_contains_secret(value, secret))
        }
        Value::Number(number) => number.to_string().contains(secret),
        Value::Bool(_) | Value::Null => false,
    }
}

fn session_error(message: impl Into<String>, secret: Option<&str>) -> SessionError {
    let message = message.into();
    SessionError::new(redact_secret(&message, secret))
}

fn session_header_rejected(secret: &str) -> SessionError {
    session_error("session header rejected", Some(secret))
}

fn session_record_rejected(secret: &str) -> SessionError {
    session_error("session record rejected", Some(secret))
}

fn open_session_for_append(path: &Path) -> Result<File, SessionError> {
    let mut options = OpenOptions::new();
    options.write(true).append(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(not(unix))]
    ensure_not_symlink(path)?;

    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SessionError::new(
            "session file is not a regular private file",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(SessionError::new(
            "session file is not a regular private file",
        ));
    }
    Ok(file)
}

fn write_json_record<T: Serialize>(file: &mut File, record: &T) -> Result<(), SessionError> {
    let line = serde_json::to_string(record)
        .map_err(|error| SessionError::new(format!("unable to encode session record: {error}")))?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn write_record(file: &mut File, record: &SessionRecord) -> Result<(), SessionError> {
    write_json_record(file, record)
}

fn child_record_contains_secret<T: Serialize>(record: &T, secret: &str) -> bool {
    !secret.is_empty()
        && serde_json::to_vec(record)
            .ok()
            .is_some_and(|serialized| bytes_contain_secret(&serialized, secret))
}

fn background_result_messages(pending: &BackgroundResultPending) -> [ChatMessage; 2] {
    let call_id = format!("background-result-{}", pending.completion_id);
    let arguments = serde_json::json!({
        "completion_id": pending.completion_id,
        "task_id": pending.task_id,
        "child_session_id": pending.child_session_id,
    })
    .to_string();
    let content = serde_json::json!({
        "completion_id": pending.completion_id,
        "task_id": pending.task_id,
        "child_session_id": pending.child_session_id,
        "task": pending.task,
        "status": pending.status,
        "result": pending.result,
        "completed_at": pending.completed_at,
    })
    .to_string();
    [
        ChatMessage::assistant(
            String::new(),
            vec![ChatToolCall {
                id: call_id.clone(),
                name: BACKGROUND_RESULT_TOOL_NAME.to_owned(),
                arguments,
            }],
        ),
        ChatMessage::tool(call_id, BACKGROUND_RESULT_TOOL_NAME.to_owned(), content),
    ]
}

const COMPACTION_SUMMARY_PREFIX: &str = "<context_compaction>\nThe earlier conversation was compacted. Treat the following summary as authoritative context for the continued turn.\n\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</context_compaction>";

fn compaction_summary_message(summary: &str) -> ChatMessage {
    ChatMessage::user(format!(
        "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
    ))
}

fn safe_message_summary(message: &ChatMessage) -> String {
    let role = message.role.as_str();
    let text = message
        .content
        .as_deref()
        .or_else(|| message.tool_calls.first().map(|call| call.name.as_str()))
        .unwrap_or("");
    let mut summary = text.chars().take(120).collect::<String>();
    if text.chars().count() > 120 {
        summary.push('…');
    }
    format!("{role}: {summary}")
}

pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join(".lucy").join("sessions")
}

pub fn validate_session_id(id: &str) -> Result<(), SessionError> {
    if id.is_empty()
        || !id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(SessionError::new("session id contains invalid characters"));
    }
    Ok(())
}

fn new_session_id() -> String {
    let timestamp = now();
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp}-{}-{counter}", std::process::id())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmSettings;
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temporary_home() -> PathBuf {
        loop {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lucy-session-{stamp}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temp home: {error}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_non_private_opened_session_file_without_chmod() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_APPEND_TEST_KEY".to_owned(),
            effort: None,
        };
        let mut session =
            Session::create(&home, &cwd, "prompt".to_owned(), llm).expect("create session");
        fs::set_permissions(&session.path, fs::Permissions::from_mode(0o644))
            .expect("make session group-readable");

        let error = session
            .append_message(ChatMessage::user("must not append".to_owned()))
            .expect_err("unsafe permissions should be rejected");
        assert!(error.to_string().contains("private"));
        assert_eq!(
            fs::metadata(&session.path)
                .expect("session metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_symlinked_session_path() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_APPEND_LINK_KEY".to_owned(),
            effort: None,
        };
        let mut session =
            Session::create(&home, &cwd, "prompt".to_owned(), llm).expect("create session");
        let target = home.join("append-target.jsonl");
        fs::write(&target, "target\n").expect("target file");
        fs::remove_file(&session.path).expect("remove session path");
        symlink(&target, &session.path).expect("session symlink");

        session
            .append_message(ChatMessage::user("must not append".to_owned()))
            .expect_err("symlink should be rejected");
        assert_eq!(
            fs::read_to_string(&target).expect("target contents"),
            "target\n"
        );

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_fifo_without_blocking() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_APPEND_FIFO_KEY".to_owned(),
            effort: None,
        };
        let mut session =
            Session::create(&home, &cwd, "prompt".to_owned(), llm).expect("create session");
        fs::remove_file(&session.path).expect("remove session path");
        let fifo_path = CString::new(session.path.as_os_str().as_bytes()).expect("FIFO path");
        let result = unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) };
        assert_eq!(result, 0, "mkfifo: {:?}", io::Error::last_os_error());

        session
            .append_message(ChatMessage::user("must not append".to_owned()))
            .expect_err("FIFO should be rejected without a blocking open");

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_session_files_and_directories() {
        let home = temporary_home();
        let directory = home.join(".lucy/sessions");
        fs::create_dir_all(&directory).expect("sessions directory");
        let target = home.join("session-target.jsonl");
        fs::write(&target, "not a session\n").expect("target session");
        let path = directory.join("linked.jsonl");
        symlink(&target, &path).expect("session symlink");
        assert!(Session::resume(&home, "linked").is_err());
        assert!(Session::list(&home).expect("list sessions").is_empty());
        fs::remove_file(path).expect("remove session symlink");
        fs::remove_file(target).expect("remove target session");
        fs::remove_dir_all(home).expect("remove temp home");

        let home = temporary_home();
        let lucy = home.join(".lucy");
        fs::create_dir(&lucy).expect("Lucy directory");
        let target = home.join("sessions-target");
        fs::create_dir(&target).expect("target sessions directory");
        symlink(&target, lucy.join("sessions")).expect("sessions directory symlink");
        assert!(Session::list(&home).is_err());
        fs::remove_file(lucy.join("sessions")).expect("remove sessions directory symlink");
        fs::remove_dir(target).expect("remove target sessions directory");
        fs::remove_dir(lucy).expect("remove Lucy directory");
        fs::remove_dir(home).expect("remove temp home");
    }

    #[test]
    fn resume_rejects_duplicate_header_as_an_invalid_record() {
        let home = temporary_home();
        let sessions = home.join(".lucy/sessions");
        fs::create_dir_all(&sessions).expect("sessions");
        let id = "duplicate-header";
        let environment = format!("LUCY_DUPLICATE_HEADER_{}", std::process::id());
        let secret = "provider-secret";
        std::env::set_var(&environment, secret);
        let header = format!(
            r#"{{"record":"session","version":1,"session_id":"{id}","created_at":1,"cwd":".","boot_system_prompt":"{secret}","boot_system_prompt":"safe","llm":{{"base_url":"http://localhost","model":"model","api_key_env":"{environment}"}}}}"#
        );
        fs::write(sessions.join(format!("{id}.jsonl")), format!("{header}\n"))
            .expect("duplicate header");

        let error = Session::resume(&home, id).expect_err("duplicate header should be rejected");
        assert_eq!(error.to_string(), "invalid session record at line 1");

        std::env::remove_var(environment);
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn creates_appends_resumes_and_lists_jsonl_session() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost:1234/api/v1".to_owned(),
            model: "test-model".to_owned(),
            api_key_env: "TEST_KEY".to_owned(),
            effort: None,
        };
        let mut session =
            Session::create(&home, &cwd, "stable prompt".to_owned(), llm.clone()).expect("create");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(sessions_dir(&home))
                    .expect("sessions directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&session.path)
                    .expect("session file metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let id = session.id.clone();
        session
            .append_message(ChatMessage::user("first".to_owned()))
            .expect("append user");
        session
            .append_message(ChatMessage::assistant("last".to_owned(), Vec::new()))
            .expect("append assistant");

        let resumed = Session::resume(&home, &id).expect("resume");
        assert_eq!(resumed.boot_system_prompt, "stable prompt");
        assert_eq!(resumed.llm, llm);
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(resumed.cwd, fs::canonicalize(cwd).expect("canonical cwd"));
        let listed = Session::list(&home).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, id);
        assert!(listed[0]
            .first_message
            .as_deref()
            .is_some_and(|summary| summary.contains("first")));
        assert!(Session::resume(&home, "missing").is_err());

        let file = fs::read_to_string(resumed.path).expect("session file");
        assert!(file.lines().count() >= 3);
        assert!(!file.contains("TEST_KEY_VALUE"));
        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[test]
    fn compaction_appends_a_boundary_and_reconstructs_only_retained_messages() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_COMPACTION_KEY".to_owned(),
            effort: None,
        };
        let mut session =
            Session::create_with_secret(&home, &cwd, "stable prompt".to_owned(), llm, None)
                .expect("create");
        session
            .append_message(ChatMessage::user("old request".to_owned()))
            .expect("old user");
        session
            .append_message(ChatMessage::assistant("old answer".to_owned(), Vec::new()))
            .expect("old assistant");
        session
            .append_message(ChatMessage::user("recent request".to_owned()))
            .expect("recent user");
        session
            .append_message(ChatMessage::assistant(
                "recent answer".to_owned(),
                Vec::new(),
            ))
            .expect("recent assistant");

        session
            .append_compaction("old work summary".to_owned(), 2, 123)
            .expect("append compaction");

        let provider_messages = session.provider_messages();
        assert_eq!(provider_messages[0].role, "system");
        assert_eq!(provider_messages[1].role, "user");
        assert!(provider_messages[1]
            .content
            .as_deref()
            .is_some_and(|content| content.contains("old work summary")));
        let provider_text = provider_messages
            .iter()
            .filter_map(|message| message.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!provider_text.contains("old request"));
        assert!(!provider_text.contains("old answer"));
        assert!(provider_text.contains("recent request"));
        assert!(provider_text.contains("recent answer"));
        assert!(matches!(
            session.history.last(),
            Some(SessionHistoryRecord::Compaction(CompactionRecord {
                first_kept_message: 2,
                tokens_before: 123,
                ..
            }))
        ));

        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.provider_messages(), provider_messages);
        assert_eq!(resumed.messages.len(), 4, "history remains append-only");
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn compaction_rejects_a_secret_in_the_summary_without_appending() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let key_env = format!("LUCY_COMPACTION_SECRET_{}", std::process::id());
        let secret = "provider-secret";
        std::env::set_var(&key_env, secret);
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: key_env.clone(),
            effort: None,
        };
        let mut session =
            Session::create_with_secret(&home, &cwd, "prompt".to_owned(), llm, Some(secret))
                .expect("create");
        session
            .append_message(ChatMessage::user("one".to_owned()))
            .expect("user");
        let before = fs::read_to_string(&session.path).expect("session bytes");

        let error = session
            .append_compaction(secret.to_owned(), 0, 1)
            .expect_err("secret summary should be rejected");
        assert!(error.to_string().contains("session record rejected"));
        assert_eq!(
            fs::read_to_string(&session.path).expect("session bytes"),
            before
        );
        assert!(!session
            .history
            .iter()
            .any(|record| matches!(record, SessionHistoryRecord::Compaction(_))));

        std::env::remove_var(key_env);
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn background_results_persist_and_materialize_once_at_delivery_position() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_BACKGROUND_RESULT_KEY".to_owned(),
            effort: None,
        };
        let mut session = Session::create_with_secret(&home, &cwd, "prompt".to_owned(), llm, None)
            .expect("create");
        session
            .append_message(ChatMessage::user("original request".to_owned()))
            .expect("user");
        let pending = BackgroundResultPending {
            timestamp: 0,
            completion_id: "completion-1".to_owned(),
            task_id: "subagent-1".to_owned(),
            child_session_id: "child-1".to_owned(),
            task: "inspect".to_owned(),
            status: ChildSessionStatus::Completed,
            result: serde_json::json!({"output":"done"}),
            completed_at: 10,
        };
        assert!(session
            .append_background_result_pending(pending.clone())
            .expect("pending"));
        let undelivered = session.undelivered_background_results();
        assert_eq!(undelivered.len(), 1);
        assert_eq!(undelivered[0].completion_id, pending.completion_id);
        assert_eq!(undelivered[0].result, pending.result);
        let mut collision = pending.clone();
        collision.child_session_id = "different-child".to_owned();
        assert!(session
            .append_background_result_pending(collision)
            .expect_err("identity collision rejected")
            .to_string()
            .contains("identity collision"));
        assert!(session
            .append_background_result_delivered(
                "completion-1",
                "turn-1".to_owned(),
                BackgroundResultDelivery::Synthetic,
            )
            .expect("delivered"));
        assert!(!session
            .append_background_result_delivered(
                "completion-1",
                "turn-1".to_owned(),
                BackgroundResultDelivery::Synthetic,
            )
            .expect("duplicate is idempotent"));
        session
            .append_message(ChatMessage::user("later request".to_owned()))
            .expect("later user");

        let messages = session.provider_messages();
        let synthetic = messages
            .iter()
            .position(|message| {
                message.role == "assistant"
                    && message
                        .tool_calls
                        .first()
                        .is_some_and(|call| call.name == BACKGROUND_RESULT_TOOL_NAME)
            })
            .expect("synthetic call");
        assert_eq!(messages[synthetic + 1].role, "tool");
        assert_eq!(
            messages[synthetic + 1].name.as_deref(),
            Some(BACKGROUND_RESULT_TOOL_NAME)
        );
        assert_eq!(
            messages[synthetic + 2].content.as_deref(),
            Some("later request")
        );
        assert!(session.undelivered_background_results().is_empty());

        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.provider_messages(), messages);
        assert!(resumed.undelivered_background_results().is_empty());
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn background_result_rejects_a_secret_without_appending() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let secret = "provider-secret";
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_BACKGROUND_RESULT_SECRET".to_owned(),
            effort: None,
        };
        let mut session =
            Session::create_with_secret(&home, &cwd, "prompt".to_owned(), llm, Some(secret))
                .expect("create");
        let before = fs::read_to_string(&session.path).expect("before");
        let error = session
            .append_background_result_pending(BackgroundResultPending {
                timestamp: 0,
                completion_id: "completion-1".to_owned(),
                task_id: "subagent-1".to_owned(),
                child_session_id: "child-1".to_owned(),
                task: "inspect".to_owned(),
                status: ChildSessionStatus::Completed,
                result: serde_json::json!({"output":secret}),
                completed_at: 10,
            })
            .expect_err("secret result rejected");
        assert!(error.to_string().contains("session record rejected"));
        assert_eq!(fs::read_to_string(&session.path).expect("after"), before);
        assert!(session.undelivered_background_results().is_empty());
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn reasoning_details_round_trip_through_session_and_provider_history() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_REASONING_DETAILS_KEY".to_owned(),
            effort: None,
        };
        let mut session = Session::create_with_secret(&home, &cwd, "prompt".to_owned(), llm, None)
            .expect("create");
        let details = vec![serde_json::json!({
            "type": "reasoning.text",
            "text": "provider detail"
        })];
        let mut assistant = ChatMessage::assistant("answer".to_owned(), Vec::new());
        assistant.reasoning_details = Some(details.clone());
        session.append_message(assistant).expect("assistant");

        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.messages[0].reasoning_details, Some(details.clone()));
        let provider_assistant = resumed
            .provider_messages()
            .into_iter()
            .find(|message| message.role == "assistant")
            .expect("provider assistant");
        assert_eq!(provider_assistant.reasoning_details, Some(details));
        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[test]
    fn append_rejects_secrets_nested_in_reasoning_details() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_REASONING_SECRET_KEY".to_owned(),
            effort: None,
        };
        let mut session = Session::create_with_secret(
            &home,
            &cwd,
            "prompt".to_owned(),
            llm,
            Some("provider-secret"),
        )
        .expect("create");
        let mut assistant = ChatMessage::assistant("answer".to_owned(), Vec::new());
        assistant.reasoning_details = Some(vec![serde_json::json!({
            "type": "reasoning.text",
            "text": "provider-secret"
        })]);
        let error = session
            .append_message(assistant)
            .expect_err("secret reasoning details");
        assert_eq!(error.to_string(), "session record rejected");
        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[test]
    fn child_session_persists_parent_link_transcript_and_terminal_status() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let key_env = format!("LUCY_CHILD_SESSION_KEY_{}", std::process::id());
        std::env::set_var(&key_env, "provider-secret");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: key_env.clone(),
            effort: Some("medium".to_owned()),
        };
        let mut child = ChildSession::create(
            &home,
            "parent-session",
            &cwd,
            "boot context".to_owned(),
            llm,
            "inspect the worker".to_owned(),
            Some("provider-secret"),
        )
        .expect("child session");
        child
            .append_message(ChatMessage::user("inspect the worker".to_owned()))
            .expect("task message");
        child
            .append_message(ChatMessage::assistant("done".to_owned(), Vec::new()))
            .expect("assistant message");
        child
            .append_status(
                ChildSessionStatus::Completed,
                None,
                Some(serde_json::json!({"output":"done"})),
            )
            .expect("status");

        let raw = fs::read_to_string(&child.path).expect("child JSONL");
        assert!(raw.contains("\"record\":\"subagent_session\""));
        assert!(raw.contains("\"parent_session_id\":\"parent-session\""));
        assert!(raw.contains("\"session_kind\":\"subagent\""));
        assert!(raw.contains("\"status\":\"completed\""));
        assert!(!raw.contains("provider-secret"));
        assert_eq!(child.provider_messages().len(), 3);
        assert_eq!(child.status, ChildSessionStatus::Completed);

        std::env::remove_var(key_env);
        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[test]
    fn interruption_records_are_valid_and_resume_in_file_order_without_provider_fragments() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_NO_SESSION_KEY".to_owned(),
            effort: None,
        };
        let mut session = Session::create_with_secret(&home, &cwd, "prompt".to_owned(), llm, None)
            .expect("create");
        session
            .append_message(ChatMessage::user("hello".to_owned()))
            .expect("user");
        session
            .append_interruption(InterruptionRecord {
                timestamp: 0,
                reason: "user_cancelled".to_owned(),
                phase: "provider_stream".to_owned(),
                assistant_text: "partial answer".to_owned(),
                tool_calls: vec![ChatToolCall {
                    id: "partial-call".to_owned(),
                    name: "cmd".to_owned(),
                    arguments: "{\"command\":".to_owned(),
                }],
                tool_results: Vec::new(),
            })
            .expect("interruption");

        session
            .append_message(ChatMessage::assistant(
                String::new(),
                vec![ChatToolCall {
                    id: "call-1".to_owned(),
                    name: "cmd".to_owned(),
                    arguments: r#"{"command":"sleep 1"}"#.to_owned(),
                }],
            ))
            .expect("assistant tool call");
        session
            .append_interruption(InterruptionRecord {
                timestamp: 0,
                reason: "user_cancelled".to_owned(),
                phase: "cmd".to_owned(),
                assistant_text: String::new(),
                tool_calls: Vec::new(),
                tool_results: vec![SessionToolResult {
                    id: "call-1".to_owned(),
                    name: "cmd".to_owned(),
                    result: serde_json::json!({"canceled": true}),
                }],
            })
            .expect("command interruption");

        let raw = fs::read_to_string(&session.path).expect("session JSONL");
        for line in raw.lines() {
            serde_json::from_str::<Value>(line).expect("valid JSONL record");
        }
        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.history.len(), 4);
        assert!(matches!(
            resumed.history[0],
            SessionHistoryRecord::Message { .. }
        ));
        assert!(matches!(
            resumed.history[1],
            SessionHistoryRecord::Interruption { .. }
        ));
        assert_eq!(resumed.messages.len(), 2);
        let provider_messages = resumed.provider_messages();
        assert_eq!(provider_messages.len(), 4);
        assert!(provider_messages.iter().any(|message| {
            message.role == "tool" && message.tool_call_id.as_deref() == Some("call-1")
        }));
        assert!(!resumed.provider_messages().iter().any(|message| {
            message
                .tool_calls
                .iter()
                .any(|call| call.id == "partial-call")
        }));
        fs::remove_dir_all(home).expect("remove temp home");
    }
}
