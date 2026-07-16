use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
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
    #[serde(rename = "turn_end")]
    TurnEnd,
    #[serde(rename = "error")]
    Error { message: String },
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

    pub fn error(&mut self, message: &str) -> io::Result<()> {
        self.emit(&ProtocolEvent::Error {
            message: message.to_owned(),
        })
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
}
