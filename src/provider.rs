use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader};
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Value};

use crate::config::LlmSettings;
use crate::model::{ChatMessage, ChatToolCall};
use crate::redaction::{conflicts_with_protected_literal, redact_secret, redaction_marker};

pub const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PROVIDER_TOOL_CALLS: usize = 64;
const MAX_PROVIDER_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_SSE_LINE_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_SSE_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_SSE_DATA_LINES: usize = 1024;
const MAX_PROVIDER_TOOL_CALL_ID_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_TOOL_NAME_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub struct ProviderError(String);

impl ProviderError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTurn {
    pub content: String,
    pub tool_calls: Vec<ChatToolCall>,
}

pub struct Provider {
    client: Client,
    endpoint: String,
    model: String,
    api_key_env: String,
    api_key: String,
}

impl Provider {
    pub fn new(settings: &LlmSettings) -> Result<Self, ProviderError> {
        let api_key = match std::env::var(&settings.api_key_env) {
            Ok(api_key) if !api_key.is_empty() => api_key,
            Ok(_) | Err(_) => return Err(ProviderError::new("missing provider API key")),
        };
        if conflicts_with_protected_literal(&api_key) {
            return Err(ProviderError::new(redact_secret(
                "API key conflicts with a required structured output literal",
                Some(&api_key),
            )));
        }
        if redaction_marker(&api_key).is_none() {
            return Err(ProviderError::new(redact_secret(
                "API key cannot be safely redacted",
                Some(&api_key),
            )));
        }
        if settings.model.trim().is_empty() {
            return Err(ProviderError::new(redact_secret(
                "missing llm.model; set a model in config.toml",
                Some(&api_key),
            )));
        }
        let endpoint = format!(
            "{}/chat/completions",
            settings.base_url.trim_end_matches('/')
        );
        let client = Client::builder()
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .map_err(|_| {
                ProviderError::new(redact_secret(
                    "unable to initialize HTTP client",
                    Some(&api_key),
                ))
            })?;
        Ok(Self {
            client,
            endpoint,
            model: settings.model.clone(),
            api_key_env: settings.api_key_env.clone(),
            api_key,
        })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn api_key_env(&self) -> &str {
        &self.api_key_env
    }

    pub fn stream_chat(
        &self,
        messages: &[ChatMessage],
        on_text: &mut dyn FnMut(&str) -> io::Result<()>,
    ) -> Result<ProviderTurn, ProviderError> {
        let request = json!({
            "model": self.model,
            "messages": messages
                .iter()
                .map(ChatMessage::to_openai_value)
                .collect::<Vec<_>>(),
            "stream": true,
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "cmd",
                        "description": "Execute a finite shell command in the session starting directory.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "command": { "type": "string" }
                            },
                            "required": ["command"],
                            "additionalProperties": false
                        }
                    }
                }
            ]
        });

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("accept", "text/event-stream")
            .json(&request)
            .send()
            .map_err(|_| ProviderError::new("provider request failed"))?;
        if !response.status().is_success() {
            return Err(ProviderError::new(format!(
                "provider returned HTTP status {}",
                response.status().as_u16()
            )));
        }

        let mut content = String::new();
        let mut tool_calls = BTreeMap::<usize, PartialToolCall>::new();
        let mut tool_argument_bytes: usize = 0;
        let mut finish_reason = None;
        {
            let mut on_data = |data: Value| -> Result<(), ProviderError> {
                if let Some(message) = provider_error_message(&data) {
                    return Err(ProviderError::new(format!(
                        "provider stream error: {}",
                        redact_secret(message, Some(&self.api_key))
                    )));
                }
                let Some(choice) = data
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.first())
                else {
                    return Ok(());
                };
                if let Some(reason) = validate_finish_reason(choice)? {
                    finish_reason = Some(reason.to_owned());
                }
                let Some(delta) = choice.get("delta") else {
                    return Ok(());
                };
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    if content.len().saturating_add(text.len()) > MAX_PROVIDER_CONTENT_BYTES {
                        return Err(ProviderError::new(
                            "provider assistant content exceeded the response limit",
                        ));
                    }
                    content.push_str(text);
                    if on_text(text).is_err() {
                        return Err(ProviderError::new("unable to emit assistant delta"));
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (position, call) in calls.iter().enumerate() {
                        let index = call
                            .get("index")
                            .and_then(Value::as_u64)
                            .map_or(position, |index| index as usize);
                        if !tool_calls.contains_key(&index)
                            && tool_calls.len() >= MAX_PROVIDER_TOOL_CALLS
                        {
                            return Err(ProviderError::new(
                                "provider response exceeded the tool-call limit",
                            ));
                        }
                        let partial = tool_calls.entry(index).or_default();
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            append_provider_field(
                                &mut partial.id,
                                id,
                                MAX_PROVIDER_TOOL_CALL_ID_BYTES,
                                "provider tool-call id exceeded the response limit",
                            )?;
                        }
                        if let Some(function) = call.get("function") {
                            if let Some(name) = function.get("name").and_then(Value::as_str) {
                                append_provider_field(
                                    &mut partial.name,
                                    name,
                                    MAX_PROVIDER_TOOL_NAME_BYTES,
                                    "provider tool-call name exceeded the response limit",
                                )?;
                            }
                            if let Some(arguments) =
                                function.get("arguments").and_then(Value::as_str)
                            {
                                if tool_argument_bytes.saturating_add(arguments.len())
                                    > MAX_PROVIDER_TOOL_ARGUMENT_BYTES
                                {
                                    return Err(ProviderError::new(
                                        "provider tool arguments exceeded the response limit",
                                    ));
                                }
                                tool_argument_bytes += arguments.len();
                                partial.arguments.push_str(arguments);
                            }
                        }
                    }
                }
                if let Some(function_call) = delta.get("function_call") {
                    if !tool_calls.contains_key(&0) && tool_calls.len() >= MAX_PROVIDER_TOOL_CALLS {
                        return Err(ProviderError::new(
                            "provider response exceeded the tool-call limit",
                        ));
                    }
                    let partial = tool_calls.entry(0).or_default();
                    if let Some(name) = function_call.get("name").and_then(Value::as_str) {
                        append_provider_field(
                            &mut partial.name,
                            name,
                            MAX_PROVIDER_TOOL_NAME_BYTES,
                            "provider tool-call name exceeded the response limit",
                        )?;
                    }
                    if let Some(arguments) = function_call.get("arguments").and_then(Value::as_str)
                    {
                        if tool_argument_bytes.saturating_add(arguments.len())
                            > MAX_PROVIDER_TOOL_ARGUMENT_BYTES
                        {
                            return Err(ProviderError::new(
                                "provider tool arguments exceeded the response limit",
                            ));
                        }
                        tool_argument_bytes += arguments.len();
                        partial.arguments.push_str(arguments);
                    }
                }
                Ok(())
            };
            let mut reader = BufReader::new(response);
            let parse_result = parse_sse(&mut reader, &mut on_data)?;
            if !parse_result.received_payload {
                return Err(ProviderError::new(
                    "provider stream contained no valid payload",
                ));
            }
            if !parse_result.received_done {
                return Err(ProviderError::new("provider stream ended before [DONE]"));
            }
        }

        let tool_calls = tool_calls
            .into_iter()
            .map(|(index, partial)| ChatToolCall {
                id: if partial.id.is_empty() {
                    format!("call_{index}")
                } else {
                    partial.id
                },
                name: partial.name,
                arguments: partial.arguments,
            })
            .collect::<Vec<_>>();
        if let Some(reason) = finish_reason.as_deref() {
            if !tool_calls.is_empty() && !matches!(reason, "tool_calls" | "function_call") {
                return Err(ProviderError::new(
                    "provider tool calls ended with an incompatible finish reason",
                ));
            }
            if tool_calls.is_empty() && matches!(reason, "tool_calls" | "function_call") {
                return Err(ProviderError::new(
                    "provider reported tool completion without a tool call",
                ));
            }
        }
        if content.is_empty() && tool_calls.is_empty() {
            return Err(ProviderError::new(
                "provider stream contained no assistant content or tool calls",
            ));
        }
        Ok(ProviderTurn {
            content,
            tool_calls,
        })
    }
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn append_provider_field(
    target: &mut String,
    fragment: &str,
    limit: usize,
    error_message: &str,
) -> Result<(), ProviderError> {
    if target.len().saturating_add(fragment.len()) > limit {
        return Err(ProviderError::new(error_message));
    }
    target.push_str(fragment);
    Ok(())
}

fn provider_error_message(data: &Value) -> Option<&str> {
    let error = data.get("error")?;
    let message = if let Some(message) = error.get("message").and_then(Value::as_str) {
        message
    } else if let Some(message) = error.as_str() {
        message
    } else {
        return Some("provider returned an error payload");
    };
    if message.len() > MAX_PROVIDER_ERROR_BYTES {
        Some("provider error text exceeded the response limit")
    } else {
        Some(message)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SseParseResult {
    pub received_payload: bool,
    pub received_done: bool,
}

pub fn parse_sse<R, F>(reader: &mut R, mut on_data: F) -> Result<SseParseResult, ProviderError>
where
    R: BufRead,
    F: FnMut(Value) -> Result<(), ProviderError>,
{
    let mut data_lines = Vec::new();
    let mut data_event_bytes = 0;
    let mut stream_bytes: usize = 0;
    let mut result = SseParseResult::default();
    let mut line = Vec::with_capacity(MAX_SSE_LINE_BYTES);
    loop {
        let (has_line, line_bytes) = read_sse_line(reader, &mut line)?;
        if stream_bytes.saturating_add(line_bytes) > MAX_SSE_STREAM_BYTES {
            return Err(ProviderError::new(
                "provider SSE stream exceeded the response limit",
            ));
        }
        stream_bytes += line_bytes;
        if !has_line {
            if !data_lines.is_empty() {
                dispatch_data(
                    &mut data_lines,
                    &mut data_event_bytes,
                    &mut on_data,
                    &mut result,
                )?;
            }
            return Ok(result);
        }

        let line = std::str::from_utf8(&line)
            .map_err(|_| ProviderError::new("unable to read provider stream"))?
            .trim_end_matches('\r');
        if line.is_empty() {
            if dispatch_data(
                &mut data_lines,
                &mut data_event_bytes,
                &mut on_data,
                &mut result,
            )? {
                return Ok(result);
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line
            .split_once(':')
            .map_or((line, ""), |(field, value)| (field, value));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            let separator_bytes = (!data_lines.is_empty()) as usize;
            let added_bytes = separator_bytes.saturating_add(value.len());
            if data_event_bytes.saturating_add(added_bytes) > MAX_SSE_EVENT_BYTES {
                return Err(ProviderError::new(
                    "provider SSE data event exceeded the response limit",
                ));
            }
            if data_lines.len() >= MAX_SSE_DATA_LINES {
                return Err(ProviderError::new(
                    "provider SSE data line count exceeded the response limit",
                ));
            }
            data_event_bytes += added_bytes;
            data_lines.push(value.to_owned());
        }
    }
}

fn read_sse_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<(bool, usize), ProviderError> {
    line.clear();
    let mut consumed_bytes = 0;
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|_| ProviderError::new("unable to read provider stream"))?;
        if buffer.is_empty() {
            return Ok((!line.is_empty(), consumed_bytes));
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let chunk_length = newline.unwrap_or(buffer.len());
        if line.len().saturating_add(chunk_length) > MAX_SSE_LINE_BYTES {
            return Err(ProviderError::new(
                "provider SSE line exceeded the response limit",
            ));
        }
        line.extend_from_slice(&buffer[..chunk_length]);
        let consumed = newline.map_or(chunk_length, |index| index + 1);
        reader.consume(consumed);
        consumed_bytes += consumed;
        if newline.is_some() {
            return Ok((true, consumed_bytes));
        }
    }
}

fn dispatch_data<F>(
    data_lines: &mut Vec<String>,
    data_event_bytes: &mut usize,
    on_data: &mut F,
    result: &mut SseParseResult,
) -> Result<bool, ProviderError>
where
    F: FnMut(Value) -> Result<(), ProviderError>,
{
    if data_lines.is_empty() {
        *data_event_bytes = 0;
        return Ok(false);
    }
    let data = data_lines.join("\n");
    data_lines.clear();
    *data_event_bytes = 0;
    if data.trim().is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        result.received_done = true;
        return Ok(true);
    }
    let value: Value = serde_json::from_str(&data)
        .map_err(|_| ProviderError::new("provider sent malformed SSE data"))?;
    result.received_payload = true;
    on_data(value)?;
    Ok(false)
}

fn validate_finish_reason(choice: &Value) -> Result<Option<&str>, ProviderError> {
    let Some(reason) = choice.get("finish_reason") else {
        return Ok(None);
    };
    if reason.is_null() {
        return Ok(None);
    }
    match reason.as_str() {
        Some("stop") | Some("tool_calls") | Some("function_call") => Ok(reason.as_str()),
        Some("length") | Some("content_filter") => Err(ProviderError::new(
            "provider response ended before completion",
        )),
        Some(_) | None => Err(ProviderError::new(
            "provider response has an unsupported finish reason",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_sse_comments_multiline_data_and_done() {
        let stream = b": keep-alive\n\ndata: \n\ndata: {\"choices\":[]\ndata: }\n\ndata: [DONE]\n";
        let mut values = Vec::new();
        let result = parse_sse(&mut Cursor::new(stream), |value| {
            values.push(value);
            Ok(())
        })
        .expect("SSE");
        assert!(result.received_payload);
        assert!(result.received_done);
        assert_eq!(values.len(), 1);
        assert!(values[0]["choices"].is_array());
    }

    #[test]
    fn parses_text_and_fragmented_tool_calls() {
        let first = serde_json::json!({
            "choices": [{"delta": {"content": "hi"}}]
        });
        let second = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "c1",
                        "function": {"name": "cmd", "arguments": "{command:"}
                    }]
                }
            }]
        });
        let third = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": "pwd}"}
                    }]
                }
            }]
        });
        let stream =
            format!("data: {first}\n\n data: {second}\n\ndata: {third}\n\ndata: [DONE]\n\n")
                .replace(" data:", "data:");
        let mut content = String::new();
        let mut calls = BTreeMap::<usize, PartialToolCall>::new();
        let result = parse_sse(&mut Cursor::new(stream.as_bytes()), |value| {
            let choice = &value["choices"][0];
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str() {
                content.push_str(text);
            }
            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                for call in tool_calls {
                    let index = call["index"].as_u64().expect("index") as usize;
                    let partial = calls.entry(index).or_default();
                    partial.id.push_str(call["id"].as_str().unwrap_or(""));
                    partial
                        .name
                        .push_str(call["function"]["name"].as_str().unwrap_or(""));
                    partial
                        .arguments
                        .push_str(call["function"]["arguments"].as_str().unwrap_or(""));
                }
            }
            Ok(())
        })
        .expect("SSE");
        assert!(result.received_payload);
        assert!(result.received_done);
        assert_eq!(content, "hi");
        assert_eq!(calls[&0].id, "c1");
        assert_eq!(calls[&0].name, "cmd");
        assert_eq!(calls[&0].arguments, "{command:pwd}");
    }

    #[test]
    fn accepts_compatible_finish_reasons_and_rejects_incomplete_ones() {
        for reason in [
            None,
            Some(Value::Null),
            Some(Value::String("stop".to_owned())),
        ] {
            let mut choice = serde_json::json!({"delta": {}});
            if let Some(reason) = reason {
                choice["finish_reason"] = reason;
            }
            validate_finish_reason(&choice).expect("compatible finish reason");
        }
        for reason in ["tool_calls", "function_call"] {
            validate_finish_reason(&serde_json::json!({
                "delta": {},
                "finish_reason": reason
            }))
            .expect("tool finish reason");
        }
        for reason in ["length", "content_filter", "error"] {
            assert!(validate_finish_reason(&serde_json::json!({
                "delta": {},
                "finish_reason": reason
            }))
            .is_err());
        }
    }

    #[test]
    fn rejects_api_keys_that_conflict_with_fixed_literals() {
        for (index, secret) in [
            "session",
            "tool",
            "cmd",
            "command",
            "finite",
            "0",
            ":",
            "[REDACTED]",
        ]
        .into_iter()
        .enumerate()
        {
            let environment = format!("LUCY_PROVIDER_CONFLICT_{}_{}", std::process::id(), index);
            std::env::set_var(&environment, secret);
            let settings = LlmSettings {
                base_url: "http://localhost".to_owned(),
                model: "model".to_owned(),
                api_key_env: environment.clone(),
            };
            let error = match Provider::new(&settings) {
                Ok(_) => panic!("fixed literal conflict should be rejected: {secret}"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("structured output"));
            assert!(!error.to_string().contains(secret));
            std::env::remove_var(environment);
        }
    }

    #[test]
    fn accepts_a_normal_long_provider_key() {
        let environment = format!("LUCY_PROVIDER_NORMAL_{}", std::process::id());
        std::env::set_var(&environment, "provider-secret");
        let settings = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: environment.clone(),
        };
        assert!(Provider::new(&settings).is_ok());
        std::env::remove_var(environment);
    }

    #[test]
    fn missing_api_key_error_does_not_echo_the_environment_name() {
        let environment = format!("LUCY_MISSING_KEY_{}", std::process::id());
        std::env::remove_var(&environment);
        let settings = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: environment.clone(),
        };
        let error = match Provider::new(&settings) {
            Ok(_) => panic!("missing key should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "missing provider API key");
        assert!(!error.to_string().contains(&environment));
    }

    #[test]
    fn rejects_an_oversized_sse_line_before_json_parsing() {
        let stream = format!("data: {}\n\n", "x".repeat(MAX_SSE_LINE_BYTES));
        let error = parse_sse(&mut Cursor::new(stream.as_bytes()), |_| Ok(())).expect_err("limit");
        assert_eq!(
            error.to_string(),
            "provider SSE line exceeded the response limit"
        );
    }

    #[test]
    fn rejects_an_oversized_sse_data_event_before_json_parsing() {
        let payload = "x".repeat(MAX_SSE_LINE_BYTES - "data: ".len());
        let line_count = MAX_SSE_EVENT_BYTES / payload.len() + 2;
        let mut stream = String::new();
        for _ in 0..line_count {
            stream.push_str("data: ");
            stream.push_str(&payload);
            stream.push('\n');
        }
        stream.push('\n');

        let error = parse_sse(&mut Cursor::new(stream.as_bytes()), |_| Ok(())).expect_err("limit");
        assert_eq!(
            error.to_string(),
            "provider SSE data event exceeded the response limit"
        );
    }

    #[test]
    fn rejects_an_oversized_sse_stream_of_ignored_fields() {
        let line = format!("ignored: {}\n", "x".repeat(1024));
        let mut stream = Vec::new();
        while stream.len() <= MAX_SSE_STREAM_BYTES {
            stream.extend_from_slice(line.as_bytes());
        }

        let error = parse_sse(&mut Cursor::new(stream), |_| Ok(())).expect_err("limit");
        assert_eq!(
            error.to_string(),
            "provider SSE stream exceeded the response limit"
        );
    }

    #[test]
    fn rejects_too_many_empty_sse_data_lines() {
        let stream = format!("{}\n", "data:\n".repeat(MAX_SSE_DATA_LINES + 1));
        let error = parse_sse(&mut Cursor::new(stream.as_bytes()), |_| Ok(())).expect_err("limit");
        assert_eq!(
            error.to_string(),
            "provider SSE data line count exceeded the response limit"
        );
    }

    #[test]
    fn reports_eof_before_done_as_incomplete() {
        let stream = b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let result = parse_sse(&mut Cursor::new(stream), |_| Ok(())).expect("SSE parse");
        assert!(result.received_payload);
        assert!(!result.received_done);
    }

    #[test]
    fn reports_empty_non_sse_input_without_payload_or_done() {
        let result =
            parse_sse(&mut Cursor::new(b"not an SSE response\n"), |_| Ok(())).expect("SSE parse");
        assert_eq!(result, SseParseResult::default());
    }

    #[test]
    fn caps_accumulated_tool_call_id_and_name_fields() {
        let fragment = "x".repeat(MAX_PROVIDER_TOOL_CALL_ID_BYTES);
        let mut id = String::new();
        append_provider_field(
            &mut id,
            &fragment,
            MAX_PROVIDER_TOOL_CALL_ID_BYTES,
            "provider tool-call id exceeded the response limit",
        )
        .expect("id within limit");
        let error = append_provider_field(
            &mut id,
            "x",
            MAX_PROVIDER_TOOL_CALL_ID_BYTES,
            "provider tool-call id exceeded the response limit",
        )
        .expect_err("id limit");
        assert_eq!(
            error.to_string(),
            "provider tool-call id exceeded the response limit"
        );

        let fragment = "x".repeat(MAX_PROVIDER_TOOL_NAME_BYTES);
        let mut name = String::new();
        append_provider_field(
            &mut name,
            &fragment,
            MAX_PROVIDER_TOOL_NAME_BYTES,
            "provider tool-call name exceeded the response limit",
        )
        .expect("name within limit");
        let error = append_provider_field(
            &mut name,
            "x",
            MAX_PROVIDER_TOOL_NAME_BYTES,
            "provider tool-call name exceeded the response limit",
        )
        .expect_err("name limit");
        assert_eq!(
            error.to_string(),
            "provider tool-call name exceeded the response limit"
        );
    }

    #[test]
    fn caps_provider_error_text_without_copying_the_full_message() {
        let message = "x".repeat(MAX_PROVIDER_ERROR_BYTES + 1);
        let value = serde_json::json!({"error": {"message": message}});
        assert_eq!(
            provider_error_message(&value),
            Some("provider error text exceeded the response limit")
        );
    }

    #[test]
    fn reports_midstream_error_without_echoing_provider_body() {
        let stream = b"data: {\"error\":{\"message\":\"bad request\"}}\n\n";
        let error = parse_sse(&mut Cursor::new(stream), |value| {
            if let Some(message) = provider_error_message(&value) {
                return Err(ProviderError::new(format!(
                    "provider stream error: {message}"
                )));
            }
            Ok(())
        })
        .expect_err("error");
        assert_eq!(error.to_string(), "provider stream error: bad request");
    }
}
