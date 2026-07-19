use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum ProtocolEvent {
    #[serde(rename = "session")]
    Session { session_id: String, resumed: bool },
    #[serde(rename = "assistant_delta")]
    AssistantDelta { text: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        name: String,
        result: Value,
    },
    #[serde(rename = "background_result_pending")]
    BackgroundResultPending {
        completion_id: String,
        task_id: String,
        child_session_id: String,
        status: String,
        result: Value,
        completed_at: u64,
    },
    #[serde(rename = "background_result_delivered")]
    BackgroundResultDelivered {
        completion_id: String,
        task_id: String,
        logical_turn_id: String,
        delivery: String,
    },
    #[serde(rename = "turn_end")]
    TurnEnd,
    #[serde(rename = "turn_interrupted")]
    TurnInterrupted { reason: String, phase: String },
    #[serde(rename = "error")]
    Error { message: String },
}

/// The normalized event boundary shared by the machine protocol and the TUI.
pub trait EventSink {
    fn emit_event(&mut self, event: &ProtocolEvent) -> io::Result<()>;

    /// Notify interactive frontends that the provider sent genuine reasoning
    /// metadata. This is intentionally not part of the public protocol.
    fn reasoning_started(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Notify interactive frontends after the provider's reasoning metadata
    /// has ended. This is intentionally not part of the public protocol.
    fn reasoning_completed(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Notify interactive frontends after an explicit skill invocation was
    /// expanded from the immutable session snapshot. This is not a public
    /// JSONL protocol event.
    fn skill_instruction_attached(&mut self, _name: &str) -> io::Result<()> {
        Ok(())
    }

    /// Notify interactive frontends of the estimated prompt context size.
    /// This is intentionally not part of the public JSONL protocol.
    fn context_usage(&mut self, _tokens: usize) -> io::Result<()> {
        Ok(())
    }

    /// Notify interactive frontends that an internal context compaction began.
    /// This is intentionally not part of the public JSONL protocol.
    fn compaction_started(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Notify interactive frontends after a compaction boundary was persisted.
    /// This is intentionally not part of the public JSONL protocol.
    fn compaction_finished(
        &mut self,
        _tokens_before: usize,
        _tokens_after: usize,
    ) -> io::Result<()> {
        Ok(())
    }
}

pub struct ProtocolWriter<W> {
    writer: W,
}

impl<W: Write> ProtocolWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub fn emit(&mut self, event: &ProtocolEvent) -> io::Result<()> {
        self.emit_serializable(event)
    }

    pub fn emit_serializable<T: Serialize>(&mut self, record: &T) -> io::Result<()> {
        serde_json::to_writer(&mut self.writer, record)
            .map_err(|error| io::Error::other(format!("encode protocol event: {error}")))?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }

    pub fn session(&mut self, session_id: &str, resumed: bool) -> io::Result<()> {
        self.emit(&ProtocolEvent::Session {
            session_id: session_id.to_owned(),
            resumed,
        })
    }

    pub fn assistant_delta(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.emit(&ProtocolEvent::AssistantDelta {
            text: text.to_owned(),
        })
    }

    pub fn tool_call(&mut self, id: &str, name: &str, arguments: &str) -> io::Result<()> {
        self.emit(&ProtocolEvent::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        })
    }

    pub fn tool_result(&mut self, id: &str, name: &str, result: Value) -> io::Result<()> {
        self.emit(&ProtocolEvent::ToolResult {
            id: id.to_owned(),
            name: name.to_owned(),
            result,
        })
    }

    pub fn turn_end(&mut self) -> io::Result<()> {
        self.emit(&ProtocolEvent::TurnEnd)
    }

    pub fn turn_interrupted(&mut self, reason: &str, phase: &str) -> io::Result<()> {
        self.emit(&ProtocolEvent::TurnInterrupted {
            reason: reason.to_owned(),
            phase: phase.to_owned(),
        })
    }

    pub fn error(&mut self, message: &str) -> io::Result<()> {
        self.emit(&ProtocolEvent::Error {
            message: message.to_owned(),
        })
    }
}

impl<W: Write> EventSink for ProtocolWriter<W> {
    fn emit_event(&mut self, event: &ProtocolEvent) -> io::Result<()> {
        self.emit(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_writer_emits_only_single_line_json_records() {
        let mut output = Vec::new();
        {
            let mut writer = ProtocolWriter::new(&mut output);
            writer.assistant_delta("line one\nline two").expect("event");
            writer
                .tool_result(
                    "call-1",
                    "cmd",
                    serde_json::json!({"stdout":"provider-shape-is-not-forwarded"}),
                )
                .expect("result");
        }
        let text = String::from_utf8(output).expect("UTF-8");
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            serde_json::from_str::<Value>(line).expect("JSONL record");
        }
        assert!(!text.contains("choices"));
    }

    #[test]
    fn compaction_frontend_state_is_not_emitted_to_jsonl() {
        let mut output = Vec::new();
        let mut writer = ProtocolWriter::new(&mut output);
        writer.context_usage(100).expect("context usage");
        writer.compaction_started().expect("compaction start");
        writer
            .compaction_finished(100, 20)
            .expect("compaction finish");

        assert!(output.is_empty());
    }

    #[test]
    fn skill_attachment_state_is_not_emitted_to_jsonl() {
        let mut output = Vec::new();
        let mut writer = ProtocolWriter::new(&mut output);
        writer
            .skill_instruction_attached("release-notes")
            .expect("non-public TUI state");
        assert!(output.is_empty());
    }

    #[test]
    fn interruption_event_is_a_normalized_json_record() {
        let event = ProtocolEvent::TurnInterrupted {
            reason: "user_cancelled".to_owned(),
            phase: "provider_stream".to_owned(),
        };
        let value = serde_json::to_value(event).expect("event JSON");
        assert_eq!(value["type"], "turn_interrupted");
        assert_eq!(value["reason"], "user_cancelled");
        assert_eq!(value["phase"], "provider_stream");
        assert!(!serde_json::to_string(&value)
            .expect("serialized event")
            .contains("choices"));
    }
}
