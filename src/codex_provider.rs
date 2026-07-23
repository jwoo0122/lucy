use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader};
use std::sync::{mpsc, Mutex, OnceLock};

use reqwest::blocking::Client;
use serde_json::{json, Value};

use crate::auth::{
    refresh_credentials, AuthStore, CodexCredentials, DEFAULT_CLIENT_ID, DEFAULT_TOKEN_ENDPOINT,
};
use crate::cancellation::CancellationToken;
use crate::config::LlmSettings;
use crate::model::{ChatMessage, ChatToolCall};
use crate::provider::{ProviderError, ProviderModel, ProviderStreamEvent, ProviderTurn};
use crate::redaction::{redact_secret, redaction_marker};

pub const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const CODEX_ENV_SENTINEL: &str = crate::config::CODEX_API_KEY_ENV_SENTINEL;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_SSE_LINE_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_SSE_STREAM_BYTES: usize = 8 * 1024 * 1024;

fn refresh_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub struct CodexProvider {
    client: Client,
    store: AuthStore,
    credentials: Mutex<CodexCredentials>,
    model: String,
    effort: Option<String>,
    initial_access: String,
}

impl CodexProvider {
    pub fn new(home: &std::path::Path, settings: &LlmSettings) -> Result<Self, ProviderError> {
        if settings.model.trim().is_empty() {
            return Err(ProviderError::new(
                "missing llm.model; set a model in config.toml",
            ));
        }
        let store = AuthStore::for_home(home);
        let credentials = store
            .load()
            .map_err(|error| ProviderError::new(error.to_string()))?
            .ok_or_else(|| ProviderError::new("Codex is not logged in; run `lucy codex login`"))?;
        if credentials.access.is_empty()
            || credentials.refresh.is_empty()
            || credentials.account_id.is_empty()
        {
            return Err(ProviderError::new(
                "Codex credentials are incomplete; run `lucy codex login`",
            ));
        }
        if redaction_marker(&credentials.access).is_none() {
            return Err(ProviderError::new(
                "Codex credential cannot be safely redacted",
            ));
        }
        let effort = settings.effort.as_deref().map(str::trim).map(str::to_owned);
        if effort.as_deref().is_some_and(str::is_empty) {
            return Err(ProviderError::new("llm.effort must not be empty"));
        }
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ProviderError::new("unable to initialize HTTP client"))?;
        Ok(Self {
            client,
            store,
            initial_access: credentials.access.clone(),
            credentials: Mutex::new(credentials),
            model: settings.model.clone(),
            effort,
        })
    }

    pub fn api_key(&self) -> &str {
        &self.initial_access
    }

    pub fn active_access(&self) -> String {
        self.credentials
            .lock()
            .map(|credentials| credentials.access.clone())
            .unwrap_or_else(|_| self.initial_access.clone())
    }

    pub fn api_key_env(&self) -> &str {
        CODEX_ENV_SENTINEL
    }

    pub fn models(&self) -> Vec<ProviderModel> {
        [
            ("gpt-5.3-codex", Some(vec!["low", "medium", "high"])),
            ("gpt-5.3-codex-spark", Some(vec!["low", "medium", "high"])),
            ("gpt-5.4", Some(vec!["low", "medium", "high"])),
            ("gpt-5.4-mini", Some(vec!["low", "medium", "high"])),
        ]
        .into_iter()
        .map(|(id, efforts)| ProviderModel {
            id: id.to_owned(),
            efforts: efforts.map(|values| values.into_iter().map(str::to_owned).collect()),
        })
        .collect()
    }

    pub fn context_window(&self) -> Option<usize> {
        Some(400_000)
    }

    fn access_token(&self) -> Result<(String, String), ProviderError> {
        let _refresh_guard = refresh_lock()
            .lock()
            .map_err(|_| ProviderError::new("Codex refresh lock unavailable"))?;
        let mut credentials = self
            .credentials
            .lock()
            .map_err(|_| ProviderError::new("Codex credential lock unavailable"))?;
        if let Some(stored) = self
            .store
            .load()
            .map_err(|error| ProviderError::new(error.to_string()))?
        {
            if stored.access != credentials.access || stored.refresh != credentials.refresh {
                *credentials = stored;
            }
        }
        let now = now_seconds();
        if credentials.near_expiry(now) {
            let refreshed =
                refresh_credentials(&credentials, DEFAULT_TOKEN_ENDPOINT, DEFAULT_CLIENT_ID)
                    .map_err(|error| {
                        ProviderError::new(redact_secret(
                            &error.to_string(),
                            Some(&credentials.access),
                        ))
                    })?;
            self.store
                .save(&refreshed)
                .map_err(|error| ProviderError::new(error.to_string()))?;
            *credentials = refreshed;
        }
        Ok((credentials.access.clone(), credentials.account_id.clone()))
    }

    fn send_request_cancellable(
        &self,
        request: &Value,
        access: &str,
        account_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::blocking::Response, ProviderError> {
        let client = self.client.clone();
        let request = request.clone();
        let access = access.to_owned();
        let request_access = access.clone();
        let account_id = account_id.to_owned();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = client
                .post(CODEX_ENDPOINT)
                .bearer_auth(&request_access)
                .header("chatgpt-account-id", &account_id)
                .header("originator", "lucy")
                .header("accept", "text/event-stream")
                .header("content-type", "application/json")
                .json(&request)
                .send()
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        loop {
            match receiver.recv_timeout(std::time::Duration::from_millis(25)) {
                Ok(Ok(response)) => return Ok(response),
                Ok(Err(error)) => {
                    return Err(ProviderError::new(redact_secret(
                        &format!("Codex request failed: {error}"),
                        Some(&access),
                    )))
                }
                Err(mpsc::RecvTimeoutError::Timeout) if cancellation.is_cancelled() => {
                    return Err(ProviderError::cancelled(ProviderTurn {
                        content: String::new(),
                        tool_calls: Vec::new(),
                        reasoning_details: Vec::new(),
                    }))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderError::new("Codex request worker stopped"))
                }
            }
        }
    }

    pub fn stream_chat(
        &self,
        messages: &[ChatMessage],
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
        cancellation: &CancellationToken,
        include_tools: bool,
        include_subagents: bool,
    ) -> Result<ProviderTurn, ProviderError> {
        let (access, account_id) = self.access_token()?;
        let request = codex_request(
            &self.model,
            messages,
            &self.effort,
            include_tools,
            include_subagents,
        );
        let mut response =
            self.send_request_cancellable(&request, &access, &account_id, cancellation)?;
        let mut active_access = access;
        if response.status().as_u16() == 401 {
            let _refresh_guard = refresh_lock()
                .lock()
                .map_err(|_| ProviderError::new("Codex refresh lock unavailable"))?;
            let (new_access, new_account) = {
                let mut current = self
                    .credentials
                    .lock()
                    .map_err(|_| ProviderError::new("Codex credential lock unavailable"))?;
                if let Some(stored) = self
                    .store
                    .load()
                    .map_err(|error| ProviderError::new(error.to_string()))?
                {
                    if stored.access != current.access || stored.refresh != current.refresh {
                        *current = stored;
                    }
                }
                if current.access == active_access {
                    let refreshed =
                        refresh_credentials(&current, DEFAULT_TOKEN_ENDPOINT, DEFAULT_CLIENT_ID)
                            .map_err(|error| {
                                ProviderError::new(redact_secret(
                                    &error.to_string(),
                                    Some(&current.access),
                                ))
                            })?;
                    self.store
                        .save(&refreshed)
                        .map_err(|error| ProviderError::new(error.to_string()))?;
                    *current = refreshed;
                }
                (current.access.clone(), current.account_id.clone())
            };
            active_access = new_access.clone();
            response =
                self.send_request_cancellable(&request, &new_access, &new_account, cancellation)?;
        }
        if !response.status().is_success() {
            return Err(ProviderError::new(format!(
                "Codex provider returned HTTP status {}",
                response.status().as_u16()
            )));
        }
        parse_stream_cancellable(response, active_access, cancellation, on_event)
    }
}

fn parse_stream_cancellable(
    response: reqwest::blocking::Response,
    secret: String,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
) -> Result<ProviderTurn, ProviderError> {
    let (sender, receiver) = mpsc::channel();
    let token = cancellation.clone();
    let worker = std::thread::spawn(move || {
        let mut emit = |event| {
            sender
                .send(Ok(event))
                .map_err(|_| io::Error::other("provider output sink closed"))
        };
        let result = parse_stream(response, &secret, &token, &mut emit);
        let _ = sender.send(Err(result));
    });
    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(25)) {
            Ok(Ok(event)) => {
                on_event(event).map_err(|_| ProviderError::new("provider output sink failed"))?
            }
            Ok(Err(result)) => {
                let _ = worker.join();
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) if cancellation.is_cancelled() => {
                return Err(ProviderError::cancelled(ProviderTurn {
                    content: String::new(),
                    tool_calls: Vec::new(),
                    reasoning_details: Vec::new(),
                }));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                return Err(ProviderError::new("Codex stream worker stopped"));
            }
        }
    }
}

fn codex_request(
    model: &str,
    messages: &[ChatMessage],
    effort: &Option<String>,
    include_tools: bool,
    include_subagents: bool,
) -> Value {
    let instructions = messages
        .iter()
        .filter(|message| message.role == "system")
        .filter_map(|message| message.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n\n");
    let input = messages
        .iter()
        .filter(|message| message.role != "system")
        .flat_map(response_input)
        .collect::<Vec<_>>();
    let mut request = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(effort) = effort {
        request["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    if include_tools {
        let mut tools = vec![tool_schema(
            "cmd",
            "Execute a finite shell command in the session starting directory.",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}),
        )];
        if include_subagents {
            tools.extend([
                tool_schema("spawn_subagent", "Start an isolated background task.", json!({"type":"object","properties":{"task":{"type":"string"}},"required":["task"],"additionalProperties":false})),
                tool_schema("check_subagent", "Inspect an in-process background subagent.", json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false})),
                tool_schema("wait_subagent", "Wait for a background subagent.", json!({"type":"object","properties":{"task_id":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}},"required":["task_id"],"additionalProperties":false})),
                tool_schema("send_subagent", "Queue a message for a running background subagent.", json!({"type":"object","properties":{"task_id":{"type":"string"},"message":{"type":"string"}},"required":["task_id","message"],"additionalProperties":false})),
                tool_schema("cancel_subagent", "Cancel a running background subagent.", json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false})),
            ]);
        }
        request["tools"] = Value::Array(tools);
    }
    request
}

fn tool_schema(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type":"function","name":name,"description":description,"parameters":parameters,"strict":false})
}

fn response_input(message: &ChatMessage) -> Vec<Value> {
    match message.role.as_str() {
        "tool" => vec![json!({
            "type": "function_call_output",
            "call_id": message.tool_call_id,
            "output": message.content.clone().unwrap_or_default()
        })],
        "assistant" => {
            let mut values = Vec::new();
            if let Some(details) = &message.reasoning_details {
                values.extend(details.iter().cloned());
            }
            if let Some(content) = &message.content {
                values.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": content}]
                }));
            }
            values.extend(message.tool_calls.iter().map(|call| {
                json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments
                })
            }));
            values
        }
        _ => vec![json!({
            "role": message.role,
            "content": [{"type": "input_text", "text": message.content.clone().unwrap_or_default()}]
        })],
    }
}

fn parse_stream(
    response: reqwest::blocking::Response,
    secret: &str,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
) -> Result<ProviderTurn, ProviderError> {
    let mut content = String::new();
    let mut tool_calls: BTreeMap<String, ChatToolCall> = BTreeMap::new();
    let mut tool_call_ids: HashMap<String, String> = HashMap::new();
    let mut reasoning_details = Vec::new();
    let mut event_name = String::new();
    let mut event_data = String::new();
    let mut total = 0usize;
    let mut terminal = false;
    let mut reader = BufReader::new(response);
    loop {
        if cancellation.is_cancelled() {
            return Err(ProviderError::cancelled(ProviderTurn {
                content,
                tool_calls: tool_calls.into_values().collect(),
                reasoning_details,
            }));
        }
        let mut line = String::new();
        let read = match reader.read_line(&mut line) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue
            }
            Err(_) => return Err(ProviderError::new("Codex stream read failed")),
        };
        if read == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        total = total.saturating_add(line.len());
        if total > MAX_SSE_STREAM_BYTES || line.len() > MAX_SSE_LINE_BYTES {
            return Err(ProviderError::new(
                "Codex SSE stream exceeded the response limit",
            ));
        }
        if line.is_empty() {
            if !event_data.is_empty() {
                let data = std::mem::take(&mut event_data);
                let name = std::mem::take(&mut event_name);
                if process_event(
                    &name,
                    &data,
                    secret,
                    &mut content,
                    &mut tool_calls,
                    &mut tool_call_ids,
                    &mut reasoning_details,
                    on_event,
                )? {
                    terminal = true;
                    break;
                }
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event_name = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("data:") {
            if event_data.len() + value.len() > MAX_SSE_EVENT_BYTES {
                return Err(ProviderError::new(
                    "Codex SSE event exceeded the response limit",
                ));
            }
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(value.trim_start());
        }
    }
    if !event_data.is_empty() && !terminal {
        process_event(
            &event_name,
            &event_data,
            secret,
            &mut content,
            &mut tool_calls,
            &mut tool_call_ids,
            &mut reasoning_details,
            on_event,
        )?;
    }
    if !terminal {
        return Err(ProviderError::new("Codex stream ended before completion"));
    }
    Ok(ProviderTurn {
        content,
        tool_calls: tool_calls.into_values().collect(),
        reasoning_details,
    })
}

#[allow(clippy::too_many_arguments)]
fn process_event(
    name: &str,
    data: &str,
    secret: &str,
    content: &mut String,
    tool_calls: &mut BTreeMap<String, ChatToolCall>,
    tool_call_ids: &mut HashMap<String, String>,
    reasoning_details: &mut Vec<Value>,
    on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
) -> Result<bool, ProviderError> {
    let value: Value = serde_json::from_str(data)
        .map_err(|_| ProviderError::new("Codex stream contained malformed JSON"))?;
    let event = if name.is_empty() {
        value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
    } else {
        name
    };
    match event {
        "response.output_text.delta" | "response.refusal.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                content.push_str(delta);
                on_event(ProviderStreamEvent::Text(delta.to_owned()))
                    .map_err(|_| ProviderError::new("provider output sink failed"))?;
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            on_event(ProviderStreamEvent::ReasoningStarted)
                .map_err(|_| ProviderError::new("provider output sink failed"))?;
        }
        "response.function_call_arguments.delta" => {
            let item_id = value
                .get("item_id")
                .or_else(|| value.get("call_id"))
                .and_then(Value::as_str)
                .unwrap_or("codex-call")
                .to_owned();
            let id = tool_call_ids
                .get(&item_id)
                .cloned()
                .unwrap_or_else(|| item_id.clone());
            let call = tool_calls
                .entry(id.clone())
                .or_insert_with(|| ChatToolCall {
                    id: id.clone(),
                    name: String::new(),
                    arguments: String::new(),
                });
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                call.arguments.push_str(delta);
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = value.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let item_id = item
                        .get("id")
                        .or_else(|| item.get("call_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("codex-call")
                        .to_owned();
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or(&item_id)
                        .to_owned();
                    tool_call_ids.insert(item_id.clone(), call_id.clone());
                    if item_id != call_id {
                        if let Some(existing) = tool_calls.remove(&item_id) {
                            tool_calls.insert(
                                call_id.clone(),
                                ChatToolCall {
                                    id: call_id.clone(),
                                    ..existing
                                },
                            );
                        }
                    }
                    let call = tool_calls
                        .entry(call_id.clone())
                        .or_insert_with(|| ChatToolCall {
                            id: call_id.clone(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                    if let Some(name) = item.get("name").and_then(Value::as_str) {
                        call.name = name.to_owned();
                    }
                    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                        call.arguments = arguments.to_owned();
                    }
                } else if item.get("type").and_then(Value::as_str) == Some("reasoning")
                    && event.ends_with("done")
                {
                    reasoning_details.push(item.clone());
                }
            }
        }
        "response.completed" | "response.done" => return Ok(true),
        "response.incomplete" => {
            return Err(ProviderError::new("Codex response was incomplete"));
        }
        "response.failed" | "error" => {
            let message = value
                .pointer("/response/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex provider returned an error");
            return Err(ProviderError::new(redact_secret(message, Some(secret))));
        }
        _ => {}
    }
    Ok(false)
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn codex_request_uses_responses_shape() {
        let request = codex_request(
            "gpt-5.3-codex",
            &[
                ChatMessage::system("system".to_owned()),
                ChatMessage::user("hello".to_owned()),
            ],
            &Some("high".to_owned()),
            true,
            false,
        );
        assert_eq!(request["model"], "gpt-5.3-codex");
        assert_eq!(request["store"], false);
        assert_eq!(request["instructions"], "system");
        assert_eq!(request["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(request["reasoning"]["effort"], "high");
        assert_eq!(request["tools"][0]["name"], "cmd");
    }

    #[test]
    fn codex_sse_normalizes_text_and_function_call_ids() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"{\\\"command\\\":\\\"pwd\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"cmd\",\"arguments\":\"{\\\"command\\\":\\\"pwd\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        );
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 8192];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            )
            .expect("response");
        });
        let response = Client::new()
            .get(format!("http://{address}"))
            .send()
            .expect("response");
        let cancellation = CancellationToken::new();
        let mut events = Vec::new();
        let turn = parse_stream(response, "secret", &cancellation, &mut |event| {
            if let ProviderStreamEvent::Text(text) = event {
                events.push(text);
            }
            Ok(())
        })
        .expect("turn");
        thread.join().expect("server");
        assert_eq!(events, vec!["hello"]);
        assert_eq!(turn.content, "hello");
        assert_eq!(turn.tool_calls[0].id, "call-1");
        assert_eq!(turn.tool_calls[0].name, "cmd");
    }

    #[test]
    fn response_tool_output_drops_name() {
        let value = response_input(&ChatMessage::tool(
            "call".to_owned(),
            "cmd".to_owned(),
            "ok".to_owned(),
        ));
        assert_eq!(value[0]["type"], "function_call_output");
        assert_eq!(value[0]["call_id"], "call");
        assert!(value[0].get("name").is_none());
    }
}
