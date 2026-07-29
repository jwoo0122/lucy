use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

/// Current public JSONL protocol version.
pub const PROTOCOL_VERSION: u8 = 1;

/// Capabilities advertised in the protocol handshake record.
pub const PROTOCOL_CAPABILITIES: &[&str] = &["sessions", "cancellation", "background_commands"];

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum ProtocolEvent {
    /// Emitted as the first stdout record in JSONL mode to declare the
    /// protocol version and capabilities before any session or turn events.
    #[serde(rename = "protocol")]
    Protocol {
        version: u8,
        capabilities: Vec<&'static str>,
    },
    #[serde(rename = "session")]
    Session {
        session_id: String,
        resumed: bool,
    },
    /// A chunk of assistant text streamed from the provider.
    #[serde(rename = "assistant_delta")]
    AssistantDelta {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// Declares a tool call the model selected. Emitted before the result.
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// The normalized result of a completed tool call.
    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        name: String,
        result: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// Marks the end of a complete turn. The model will not produce more
    /// output until a new message is received.
    #[serde(rename = "turn_end")]
    TurnEnd {
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// Marks an interruption caused by user cancellation or a fatal error
    /// during a turn.
    #[serde(rename = "turn_interrupted")]
    TurnInterrupted {
        reason: String,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// A recoverable or terminal error that does not end the process.
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
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

    /// Emit the protocol handshake as the first stdout record.
    pub fn handshake(&mut self) -> io::Result<()> {
        self.emit(&ProtocolEvent::Protocol {
            version: PROTOCOL_VERSION,
            capabilities: PROTOCOL_CAPABILITIES.to_vec(),
        })
    }

    pub fn session(&mut self, session_id: &str, resumed: bool) -> io::Result<()> {
        self.emit(&ProtocolEvent::Session {
            session_id: session_id.to_owned(),
            resumed,
        })
    }

    pub fn assistant_delta(&mut self, text: &str) -> io::Result<()> {
        self.assistant_delta_correlated(text, None, None)
    }

    pub fn assistant_delta_correlated(
        &mut self,
        text: &str,
        turn_id: Option<&str>,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.emit(&ProtocolEvent::AssistantDelta {
            text: text.to_owned(),
            turn_id: turn_id.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
        })
    }

    pub fn tool_call(&mut self, id: &str, name: &str, arguments: &str) -> io::Result<()> {
        self.tool_call_correlated(id, name, arguments, None, None)
    }

    pub fn tool_call_correlated(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        turn_id: Option<&str>,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        self.emit(&ProtocolEvent::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            turn_id: turn_id.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
        })
    }

    pub fn tool_result(&mut self, id: &str, name: &str, result: Value) -> io::Result<()> {
        self.tool_result_correlated(id, name, result, None, None)
    }

    pub fn tool_result_correlated(
        &mut self,
        id: &str,
        name: &str,
        result: Value,
        turn_id: Option<&str>,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        self.emit(&ProtocolEvent::ToolResult {
            id: id.to_owned(),
            name: name.to_owned(),
            result,
            turn_id: turn_id.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
        })
    }

    pub fn turn_end(&mut self) -> io::Result<()> {
        self.turn_end_correlated(None, None)
    }

    pub fn turn_end_correlated(
        &mut self,
        turn_id: Option<&str>,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        self.emit(&ProtocolEvent::TurnEnd {
            turn_id: turn_id.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
        })
    }

    pub fn turn_interrupted(&mut self, reason: &str, phase: &str) -> io::Result<()> {
        self.turn_interrupted_correlated(reason, phase, None, None)
    }

    pub fn turn_interrupted_correlated(
        &mut self,
        reason: &str,
        phase: &str,
        turn_id: Option<&str>,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        self.emit(&ProtocolEvent::TurnInterrupted {
            reason: reason.to_owned(),
            phase: phase.to_owned(),
            turn_id: turn_id.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
        })
    }

    pub fn error(&mut self, message: &str) -> io::Result<()> {
        self.error_correlated(message, None)
    }

    pub fn error_correlated(
        &mut self,
        message: &str,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        self.emit(&ProtocolEvent::Error {
            message: message.to_owned(),
            request_id: request_id.map(str::to_owned),
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
            turn_id: None,
            request_id: None,
        };
        let value = serde_json::to_value(event).expect("event JSON");
        assert_eq!(value["type"], "turn_interrupted");
        assert_eq!(value["reason"], "user_cancelled");
        assert_eq!(value["phase"], "provider_stream");
        assert!(!serde_json::to_string(&value)
            .expect("serialized event")
            .contains("choices"));
    }

    #[test]
    fn handshake_emits_protocol_record_first() {
        let mut output = Vec::new();
        {
            let mut writer = ProtocolWriter::new(&mut output);
            writer.handshake().expect("handshake");
            writer.session("s1", false).expect("session");
        }
        let text = String::from_utf8(output).expect("UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).expect("first record");
        assert_eq!(first["type"], "protocol");
        assert_eq!(first["version"], 1);
        let caps = first["capabilities"].as_array().expect("capabilities");
        assert!(caps.iter().any(|c| c == "sessions"));
        assert!(caps.iter().any(|c| c == "cancellation"));
        assert!(caps.iter().any(|c| c == "background_commands"));
        let second: Value = serde_json::from_str(lines[1]).expect("second record");
        assert_eq!(second["type"], "session");
    }

    #[test]
    fn optional_fields_are_omitted_when_none() {
        let event = ProtocolEvent::AssistantDelta {
            text: "hi".to_owned(),
            turn_id: None,
            request_id: None,
        };
        let value = serde_json::to_value(event).expect("event JSON");
        assert!(value.get("turn_id").is_none());
        assert!(value.get("request_id").is_none());
    }

    #[test]
    fn optional_fields_are_present_when_set() {
        let event = ProtocolEvent::AssistantDelta {
            text: "hi".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            request_id: Some("build-42".to_owned()),
        };
        let value = serde_json::to_value(event).expect("event JSON");
        assert_eq!(value["turn_id"], "turn-1");
        assert_eq!(value["request_id"], "build-42");
    }

    #[test]
    fn error_event_includes_request_id_when_set() {
        let event = ProtocolEvent::Error {
            message: "fail".to_owned(),
            request_id: Some("build-42".to_owned()),
        };
        let value = serde_json::to_value(event).expect("event JSON");
        assert_eq!(value["type"], "error");
        assert_eq!(value["request_id"], "build-42");
    }

    #[test]
    fn turn_end_serializes_with_optional_fields() {
        let event = ProtocolEvent::TurnEnd {
            turn_id: Some("turn-1".to_owned()),
            request_id: None,
        };
        let value = serde_json::to_value(event).expect("event JSON");
        assert_eq!(value["type"], "turn_end");
        assert_eq!(value["turn_id"], "turn-1");
        assert!(value.get("request_id").is_none());
    }
}
