use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<Value>>,
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

/// Estimate the number of context tokens represented by provider messages.
///
/// Lucy supports arbitrary OpenAI-compatible providers and does not bundle a
/// provider-specific tokenizer. Four UTF-8 bytes per token is therefore a
/// deliberately conservative display estimate; the statusline should expose
/// context pressure without pretending that every provider uses the same
/// tokenizer.
pub(crate) fn estimate_message_tokens(message: &ChatMessage) -> usize {
    serde_json::to_vec(message)
        .map(|encoded| encoded.len().div_ceil(4).max(1))
        .unwrap_or(1)
}

pub(crate) fn estimate_context_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(estimate_message_tokens)
        .sum::<usize>()
        .max(1)
}

impl ChatMessage {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".to_owned(),
            content: Some(content),
            reasoning_details: None,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".to_owned(),
            content: Some(content),
            reasoning_details: None,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: String, tool_calls: Vec<ChatToolCall>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: (!content.is_empty()).then_some(content),
            reasoning_details: None,
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    pub fn tool(tool_call_id: String, name: String, content: String) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content),
            reasoning_details: None,
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
        if self.role == "assistant" {
            if let Some(reasoning_details) = &self.reasoning_details {
                message["reasoning_details"] = Value::Array(reasoning_details.clone());
            }
        }
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
    fn context_token_estimate_is_nonzero_and_grows_with_messages() {
        let empty = estimate_context_tokens(&[]);
        let one = estimate_context_tokens(&[ChatMessage::user("hello".to_owned())]);

        assert_eq!(empty, 1);
        assert!(one > empty);
        assert!(estimate_message_tokens(&ChatMessage::user("hello".to_owned())) > 0);
    }

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

    #[test]
    fn reasoning_details_are_optional_and_only_sent_for_assistant_messages() {
        let details = vec![json!({
            "type": "reasoning.text",
            "text": "private reasoning"
        })];
        let mut assistant = ChatMessage::assistant("answer".to_owned(), Vec::new());
        assistant.reasoning_details = Some(details.clone());
        assert_eq!(
            assistant.to_openai_value()["reasoning_details"],
            json!(details)
        );

        let old_message: ChatMessage = serde_json::from_value(json!({
            "role": "assistant",
            "content": "old session"
        }))
        .expect("old message without reasoning details");
        assert_eq!(old_message.reasoning_details, None);

        let mut user = ChatMessage::user("question".to_owned());
        user.reasoning_details = Some(vec![json!({"text": "must not be sent"})]);
        assert!(user.to_openai_value().get("reasoning_details").is_none());
    }
}
