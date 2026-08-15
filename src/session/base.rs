use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{ChatMessage, ChatToolCall};

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

/// Read-only compatibility shape for historical semantic compaction records.
/// Lucy 2 never creates this record in the canonical journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionRecord {
    pub timestamp: u64,
    pub summary: String,
    pub first_kept_message: usize,
    pub tokens_before: usize,
}

/// Compatibility view consumed by the existing frontend while the session UX
/// is removed. New durable state is stored as factual JournalEvents instead.
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
}

/// Compatibility metadata shape for old JSONL/TUI session surfaces. Lucy 2
/// exposes at most one synthetic `global` item while these surfaces exist.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionMetadata {
    #[serde(rename = "type")]
    pub record_type: &'static str,
    pub session_id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub cwd: String,
    pub first_message: Option<String>,
    pub last_message: Option<String>,
    pub last_user_message: Option<String>,
    pub last_assistant_message: Option<String>,
}
