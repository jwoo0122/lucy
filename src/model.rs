use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".to_owned(),
            content: Some(content),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".to_owned(),
            content: Some(content),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: String, tool_calls: Vec<ChatToolCall>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: (!content.is_empty()).then_some(content),
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    pub fn tool(tool_call_id: String, name: String, content: String) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content),
            name: Some(name),
            tool_call_id: Some(tool_call_id),
            tool_calls: Vec::new(),
        }
    }

    pub fn to_openai_value(&self) -> Value {
        let mut message = json!({
            "role": self.role,
            "content": self.content,
        });
        if let Some(name) = &self.name {
            message["name"] = Value::String(name.clone());
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            message["tool_call_id"] = Value::String(tool_call_id.clone());
        }
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(
                self.tool_calls
                    .iter()
                    .map(|tool_call| {
                        json!({
                            "id": tool_call.id,
                            "type": "function",
                            "function": {
                                "name": tool_call.name,
                                "arguments": tool_call.arguments,
                            }
                        })
                    })
                    .collect(),
            );
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_assistant_messages_have_openai_compatible_shape() {
        let assistant = ChatMessage::assistant(
            String::new(),
            vec![ChatToolCall {
                id: "call-1".to_owned(),
                name: "cmd".to_owned(),
                arguments: r#"{"command":"pwd"}"#.to_owned(),
            }],
        );
        assert_eq!(assistant.to_openai_value()["content"], Value::Null);
        assert_eq!(
            assistant.to_openai_value()["tool_calls"][0]["type"],
            "function"
        );

        let tool = ChatMessage::tool(
            "call-1".to_owned(),
            "cmd".to_owned(),
            "{\"exit_code\":0}".to_owned(),
        );
        assert_eq!(tool.to_openai_value()["tool_call_id"], "call-1");
        assert_eq!(tool.to_openai_value()["name"], "cmd");
    }
}
