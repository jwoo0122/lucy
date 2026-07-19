use std::collections::BTreeMap;
use std::io::{self, BufRead};
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::Client as AsyncClient;
use serde_json::{json, Value};

use crate::cancellation::CancellationToken;
use crate::config::LlmSettings;
use crate::model::{ChatMessage, ChatToolCall};
use crate::redaction::{conflicts_with_protected_literal, redact_secret, redaction_marker};

/// Maximum idle interval between provider response reads.
pub const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);
const PROVIDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PROVIDER_RETRY_COUNT: usize = 1;
const PROVIDER_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const MAX_PROVIDER_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_REASONING_DETAILS_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_SSE_LINE_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_SSE_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_SSE_DATA_LINES: usize = 1024;
const MAX_PROVIDER_TOOL_CALL_ID_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_TOOL_NAME_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_ERROR_BYTES: usize = 16 * 1024;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MODEL_METADATA_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_MODEL_METADATA_BYTES: usize = 4 * 1024 * 1024;
const COMPACTION_MAX_SUMMARY_TOKENS: usize = 4_096;
const SPAWN_SUBAGENT_DESCRIPTION: &str = "Start an isolated background task and immediately return its task ID. The worker always inherits the current session model and reasoning effort; callers cannot override either setting. Continue your own work without waiting; when the worker finishes, Lucy resumes the attached logical turn with a typed background result instead of creating user input or a separate user turn. Do not poll with check_subagent unless you need an intermediate status. The worker has cmd but cannot delegate further.";
const CHECK_SUBAGENT_DESCRIPTION: &str = "Inspect an in-process background subagent only when you need an intermediate status or an on-demand result. Do not poll repeatedly: when the worker finishes, Lucy resumes the attached logical turn with its typed result, so continue your own work instead.";
const WAIT_SUBAGENT_DESCRIPTION: &str = "Wait for a background subagent to reach a terminal state. A timeout only ends the wait; it does not cancel the subagent.";
const SEND_SUBAGENT_DESCRIPTION: &str = "Queue an additional message for a running background subagent. It is delivered at the worker's next safe provider boundary.";
const CANCEL_SUBAGENT_DESCRIPTION: &str =
    "Cancel a running background subagent at the nearest safe provider or command boundary.";

#[derive(Debug)]
pub struct ProviderError {
    message: String,
    cancelled: bool,
    partial: Option<ProviderTurn>,
    retryable: bool,
}

impl ProviderError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
            partial: None,
            retryable: false,
        }
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
            partial: None,
            retryable: true,
        }
    }

    fn cancelled(partial: ProviderTurn) -> Self {
        Self {
            message: "provider stream canceled".to_owned(),
            cancelled: true,
            partial: Some(partial),
            retryable: false,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn partial_turn(&self) -> Option<&ProviderTurn> {
        self.partial.as_ref()
    }

    fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

fn transient_http_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn transient_reqwest_error(error: &reqwest::Error) -> bool {
    error.is_timeout()
        || error.is_connect()
        || error.is_request()
        || error.is_body()
        || error.is_decode()
}

fn reqwest_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_request() {
        "request"
    } else {
        "transport"
    }
}

fn reqwest_failure(
    prefix: &str,
    error: reqwest::Error,
    api_key: &str,
    retry_before_payload: bool,
) -> ProviderError {
    let detail = redact_secret(&error.to_string(), Some(api_key));
    let message = format!("{prefix} ({}): {detail}", reqwest_error_kind(&error));
    let mut provider_error = ProviderError::new(message);
    provider_error.retryable = retry_before_payload && transient_reqwest_error(&error);
    provider_error
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTurn {
    pub content: String,
    pub tool_calls: Vec<ChatToolCall>,
    pub reasoning_details: Vec<Value>,
}

fn empty_turn() -> ProviderTurn {
    ProviderTurn {
        content: String::new(),
        tool_calls: Vec::new(),
        reasoning_details: Vec::new(),
    }
}

pub(crate) enum ProviderStreamEvent {
    ReasoningStarted,
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModel {
    pub id: String,
    pub efforts: Option<Vec<String>>,
}

pub struct Provider {
    client: Client,
    async_client: AsyncClient,
    endpoint: String,
    model: String,
    effort: Option<String>,
    api_key_env: String,
    api_key: String,
}

fn model_efforts(entry: &Value) -> Option<Vec<String>> {
    let values = entry
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("supported_efforts"))
        .or_else(|| {
            [
                "supported_reasoning_efforts",
                "reasoning_efforts",
                "reasoning_effort",
                "efforts",
            ]
            .into_iter()
            .find_map(|key| entry.get(key))
        })
        .and_then(Value::as_array)?;
    let efforts = values
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .fold(Vec::new(), |mut efforts, value| {
            if !efforts.iter().any(|effort| effort == value) {
                efforts.push(value.to_owned());
            }
            efforts
        });
    (!efforts.is_empty()).then_some(efforts)
}

fn context_window_from_models(payload: &Value, model: &str) -> Option<usize> {
    let models = payload.get("data").and_then(Value::as_array)?;
    let entry = models.iter().find(|entry| {
        entry.get("id").and_then(Value::as_str) == Some(model)
            || entry.get("name").and_then(Value::as_str) == Some(model)
    })?;
    [
        entry.get("context_length"),
        entry.get("context_window"),
        entry.get("max_context_length"),
        entry
            .get("top_provider")
            .and_then(|provider| provider.get("context_length")),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_u64)
    .and_then(|value| usize::try_from(value).ok())
    .filter(|value| *value > 0)
}

fn chat_request(
    model: &str,
    messages: &[ChatMessage],
    effort: &Option<String>,
    include_tools: bool,
    include_subagents: bool,
) -> Value {
    let mut request = json!({
        "model": model,
        "messages": messages
            .iter()
            .map(ChatMessage::to_openai_value)
            .collect::<Vec<_>>(),
        "stream": true,
    });
    if include_tools {
        let mut tools = vec![json!({
            "type": "function",
            "function": {
                "name": "cmd",
                "description": "Execute a finite shell command in the session starting directory.",
                "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"], "additionalProperties": false}
            }
        })];
        if include_subagents {
            tools.push(json!({"type":"function","function":{"name":"spawn_subagent","description":SPAWN_SUBAGENT_DESCRIPTION,"parameters":{"type":"object","properties":{"task":{"type":"string"}},"required":["task"],"additionalProperties":false}}}));
            tools.push(json!({"type":"function","function":{"name":"check_subagent","description":CHECK_SUBAGENT_DESCRIPTION,"parameters":{"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}}}));
            tools.push(json!({"type":"function","function":{"name":"wait_subagent","description":WAIT_SUBAGENT_DESCRIPTION,"parameters":{"type":"object","properties":{"task_id":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}},"required":["task_id"],"additionalProperties":false}}}));
            tools.push(json!({"type":"function","function":{"name":"send_subagent","description":SEND_SUBAGENT_DESCRIPTION,"parameters":{"type":"object","properties":{"task_id":{"type":"string"},"message":{"type":"string"}},"required":["task_id","message"],"additionalProperties":false}}}));
            tools.push(json!({"type":"function","function":{"name":"cancel_subagent","description":CANCEL_SUBAGENT_DESCRIPTION,"parameters":{"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}}}));
        }
        request["tools"] = Value::Array(tools);
    } else {
        request["max_tokens"] = json!(COMPACTION_MAX_SUMMARY_TOKENS);
    }
    if let Some(effort) = effort {
        request["reasoning_effort"] = json!(effort);
    }
    request
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
        let effort = match &settings.effort {
            Some(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(ProviderError::new(redact_secret(
                        "llm.effort must not be empty",
                        Some(&api_key),
                    )));
                }
                Some(trimmed.to_owned())
            }
            None => None,
        };
        let endpoint = format!(
            "{}/chat/completions",
            settings.base_url.trim_end_matches('/')
        );
        let client = Client::builder()
            .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .map_err(|_| {
                ProviderError::new(redact_secret(
                    "unable to initialize HTTP client",
                    Some(&api_key),
                ))
            })?;
        let async_client = AsyncClient::builder()
            .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
            .read_timeout(PROVIDER_TIMEOUT)
            .build()
            .map_err(|_| {
                ProviderError::new(redact_secret(
                    "unable to initialize HTTP client",
                    Some(&api_key),
                ))
            })?;
        Ok(Self {
            client,
            async_client,
            endpoint,
            model: settings.model.clone(),
            effort,
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

    pub(crate) fn models(&self) -> Result<Vec<ProviderModel>, ProviderError> {
        let base_url = self
            .endpoint
            .strip_suffix("/chat/completions")
            .ok_or_else(|| ProviderError::new("invalid provider endpoint"))?;
        let response = self
            .client
            .get(format!("{base_url}/models"))
            .bearer_auth(&self.api_key)
            .timeout(MODEL_METADATA_TIMEOUT)
            .send()
            .map_err(|_| ProviderError::new("unable to load provider models"))?;
        if !response.status().is_success() {
            return Err(ProviderError::new("unable to load provider models"));
        }
        let bytes = response
            .bytes()
            .map_err(|_| ProviderError::new("unable to load provider models"))?;
        if bytes.len() > MAX_MODEL_METADATA_BYTES {
            return Err(ProviderError::new(
                "provider model catalog exceeded the response limit",
            ));
        }
        let payload: Value = serde_json::from_slice(&bytes)
            .map_err(|_| ProviderError::new("invalid provider model catalog"))?;
        let models = payload
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::new("invalid provider model catalog"))?;
        let mut result = models
            .iter()
            .filter_map(|entry| {
                let id = entry
                    .get("id")
                    .or_else(|| entry.get("name"))
                    .and_then(Value::as_str)?
                    .trim();
                if id.is_empty() {
                    return None;
                }
                let efforts = model_efforts(entry);
                Some(ProviderModel {
                    id: id.to_owned(),
                    efforts,
                })
            })
            .collect::<Vec<_>>();
        result.sort_by(|left, right| left.id.cmp(&right.id));
        result.dedup_by(|left, right| left.id == right.id);
        Ok(result)
    }

    /// Query the OpenAI-compatible model catalog for the configured model's
    /// context window. Providers that do not expose context metadata simply
    /// return `None`; this lookup is only used by the interactive statusline.
    pub(crate) fn context_window(&self) -> Option<usize> {
        let base_url = self.endpoint.strip_suffix("/chat/completions")?;
        let response = self
            .client
            .get(format!("{base_url}/models"))
            .bearer_auth(&self.api_key)
            .timeout(MODEL_METADATA_TIMEOUT)
            .send()
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bytes = response.bytes().ok()?;
        if bytes.len() > MAX_MODEL_METADATA_BYTES {
            return None;
        }
        let payload: Value = serde_json::from_slice(&bytes).ok()?;
        context_window_from_models(&payload, &self.model)
    }

    pub fn stream_chat(
        &self,
        messages: &[ChatMessage],
        on_text: &mut dyn FnMut(&str) -> io::Result<()>,
    ) -> Result<ProviderTurn, ProviderError> {
        let cancellation = CancellationToken::new();
        self.stream_chat_cancellable_with_options(messages, on_text, &cancellation, true, true)
    }

    /// Generate an internal compaction summary without exposing `cmd` to the
    /// summarization request or emitting its text as a normal assistant delta.
    pub(crate) fn summarize(
        &self,
        messages: &[ChatMessage],
        cancellation: &CancellationToken,
    ) -> Result<String, ProviderError> {
        let mut ignored = |_text: &str| Ok(());
        let turn = self.stream_chat_cancellable_with_options(
            messages,
            &mut ignored,
            cancellation,
            false,
            false,
        )?;
        if !turn.tool_calls.is_empty() {
            return Err(ProviderError::new(
                "compaction summary requested an unsupported tool",
            ));
        }
        if turn.content.trim().is_empty() {
            return Err(ProviderError::new("compaction summary was empty"));
        }
        Ok(turn.content)
    }

    /// Stream through an async response so cancellation can drop the pending
    /// socket read instead of waiting for the blocking client's timeout.
    #[allow(dead_code)]
    pub(crate) fn stream_chat_cancellable(
        &self,
        messages: &[ChatMessage],
        on_text: &mut dyn FnMut(&str) -> io::Result<()>,
        cancellation: &CancellationToken,
    ) -> Result<ProviderTurn, ProviderError> {
        self.stream_chat_cancellable_with_options(messages, on_text, cancellation, true, true)
    }

    pub(crate) fn stream_chat_cancellable_with_options(
        &self,
        messages: &[ChatMessage],
        on_text: &mut dyn FnMut(&str) -> io::Result<()>,
        cancellation: &CancellationToken,
        include_tools: bool,
        include_subagents: bool,
    ) -> Result<ProviderTurn, ProviderError> {
        let mut on_event = |event| match event {
            ProviderStreamEvent::ReasoningStarted => Ok(()),
            ProviderStreamEvent::Text(text) => on_text(&text),
        };
        self.stream_chat_cancellable_with_options_and_events(
            messages,
            &mut on_event,
            cancellation,
            include_tools,
            include_subagents,
        )
    }

    pub(crate) fn stream_chat_cancellable_with_options_and_events(
        &self,
        messages: &[ChatMessage],
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
        cancellation: &CancellationToken,
        include_tools: bool,
        include_subagents: bool,
    ) -> Result<ProviderTurn, ProviderError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ProviderError::new("unable to initialize provider runtime"))?;
        runtime.block_on(self.stream_chat_async_with_retries(
            messages,
            on_event,
            cancellation,
            include_tools,
            include_subagents,
        ))
    }

    async fn stream_chat_async_with_retries(
        &self,
        messages: &[ChatMessage],
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
        cancellation: &CancellationToken,
        include_tools: bool,
        include_subagents: bool,
    ) -> Result<ProviderTurn, ProviderError> {
        for attempt in 0..=PROVIDER_RETRY_COUNT {
            match self
                .stream_chat_async_once(
                    messages,
                    on_event,
                    cancellation,
                    include_tools,
                    include_subagents,
                )
                .await
            {
                Err(error) if error.is_retryable() && attempt < PROVIDER_RETRY_COUNT => {
                    if cancellation.is_cancelled() {
                        return Err(ProviderError::cancelled(empty_turn()));
                    }
                    tokio::time::sleep(PROVIDER_RETRY_BACKOFF).await;
                }
                result => return result,
            }
        }
        unreachable!("provider retry loop must return");
    }

    async fn stream_chat_async_once(
        &self,
        messages: &[ChatMessage],
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
        cancellation: &CancellationToken,
        include_tools: bool,
        include_subagents: bool,
    ) -> Result<ProviderTurn, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::cancelled(ProviderTurn {
                content: String::new(),
                tool_calls: Vec::new(),
                reasoning_details: Vec::new(),
            }));
        }
        let request = chat_request(
            &self.model,
            messages,
            &self.effort,
            include_tools,
            include_subagents,
        );
        let request = self
            .async_client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("accept", "text/event-stream")
            .json(&request)
            .send();
        let mut request = Box::pin(request);
        let mut response = loop {
            if cancellation.is_cancelled() {
                return Err(ProviderError::cancelled(ProviderTurn {
                    content: String::new(),
                    tool_calls: Vec::new(),
                    reasoning_details: Vec::new(),
                }));
            }
            match tokio::time::timeout(CANCELLATION_POLL_INTERVAL, request.as_mut()).await {
                Ok(response) => {
                    break response.map_err(|error| {
                        reqwest_failure("provider request failed", error, &self.api_key, true)
                    })?;
                }
                Err(_) => continue,
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error = if transient_http_status(status) {
                ProviderError::retryable(format!("provider returned HTTP status {status}"))
            } else {
                ProviderError::new(format!("provider returned HTTP status {status}"))
            };
            return Err(error);
        }

        let mut accumulator = ProviderAccumulator::default();
        let mut decoder = SseDecoder::default();
        loop {
            if cancellation.is_cancelled() {
                return Err(ProviderError::cancelled(accumulator.partial_turn()));
            }
            let chunk =
                match tokio::time::timeout(CANCELLATION_POLL_INTERVAL, response.chunk()).await {
                    Ok(chunk) => {
                        let retry_before_payload = !decoder.result.received_payload;
                        chunk.map_err(|error| {
                            reqwest_failure(
                                "provider stream read failed",
                                error,
                                &self.api_key,
                                retry_before_payload,
                            )
                        })?
                    }
                    Err(_) => continue,
                };
            let Some(chunk) = chunk else {
                break;
            };
            let done = decoder.feed(&chunk, &mut |data| {
                accumulator.on_data(data, &self.api_key, on_event)
            })?;
            if done {
                break;
            }
        }
        if cancellation.is_cancelled() {
            return Err(ProviderError::cancelled(accumulator.partial_turn()));
        }
        let parse_result =
            decoder.finish(&mut |data| accumulator.on_data(data, &self.api_key, on_event))?;
        if !parse_result.received_payload {
            return Err(ProviderError::new(
                "provider stream contained no valid payload",
            ));
        }
        if !parse_result.received_done {
            return Err(ProviderError::new("provider stream ended before [DONE]"));
        }
        accumulator.finish()
    }
}

#[derive(Debug, Clone, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct ProviderAccumulator {
    content: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    reasoning_details: Vec<Value>,
    reasoning_details_bytes: usize,
    tool_argument_bytes: usize,
    finish_reason: Option<String>,
    reasoning_started: bool,
}

impl ProviderAccumulator {
    fn on_data(
        &mut self,
        data: Value,
        api_key: &str,
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
    ) -> Result<(), ProviderError> {
        if let Some(message) = provider_error_message(&data) {
            return Err(ProviderError::new(format!(
                "provider stream error: {}",
                redact_secret(message, Some(api_key))
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
            self.finish_reason = Some(reason.to_owned());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        let received_reasoning = append_reasoning_details(
            &mut self.reasoning_details,
            &mut self.reasoning_details_bytes,
            delta,
        )?;
        if received_reasoning && !self.reasoning_started {
            self.reasoning_started = true;
            on_event(ProviderStreamEvent::ReasoningStarted)
                .map_err(|_| ProviderError::new("unable to emit reasoning state"))?;
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if self.content.len().saturating_add(text.len()) > MAX_PROVIDER_CONTENT_BYTES {
                return Err(ProviderError::new(
                    "provider assistant content exceeded the response limit",
                ));
            }
            self.content.push_str(text);
            on_event(ProviderStreamEvent::Text(text.to_owned()))
                .map_err(|_| ProviderError::new("unable to emit assistant delta"))?;
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map_or(position, |index| index as usize);
                let partial = self.tool_calls.entry(index).or_default();
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
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        if self.tool_argument_bytes.saturating_add(arguments.len())
                            > MAX_PROVIDER_TOOL_ARGUMENT_BYTES
                        {
                            return Err(ProviderError::new(
                                "provider tool arguments exceeded the response limit",
                            ));
                        }
                        self.tool_argument_bytes += arguments.len();
                        partial.arguments.push_str(arguments);
                    }
                }
            }
        }
        if let Some(function_call) = delta.get("function_call") {
            let partial = self.tool_calls.entry(0).or_default();
            if let Some(name) = function_call.get("name").and_then(Value::as_str) {
                append_provider_field(
                    &mut partial.name,
                    name,
                    MAX_PROVIDER_TOOL_NAME_BYTES,
                    "provider tool-call name exceeded the response limit",
                )?;
            }
            if let Some(arguments) = function_call.get("arguments").and_then(Value::as_str) {
                if self.tool_argument_bytes.saturating_add(arguments.len())
                    > MAX_PROVIDER_TOOL_ARGUMENT_BYTES
                {
                    return Err(ProviderError::new(
                        "provider tool arguments exceeded the response limit",
                    ));
                }
                self.tool_argument_bytes += arguments.len();
                partial.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    fn partial_turn(&self) -> ProviderTurn {
        ProviderTurn {
            content: self.content.clone(),
            tool_calls: self
                .tool_calls
                .iter()
                .map(|(index, partial)| ChatToolCall {
                    id: if partial.id.is_empty() {
                        format!("call_{index}")
                    } else {
                        partial.id.clone()
                    },
                    name: partial.name.clone(),
                    arguments: partial.arguments.clone(),
                })
                .collect(),
            reasoning_details: self.reasoning_details.clone(),
        }
    }

    fn finish(self) -> Result<ProviderTurn, ProviderError> {
        let tool_calls = self
            .tool_calls
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
        if let Some(reason) = self.finish_reason.as_deref() {
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
        if self.content.is_empty() && tool_calls.is_empty() {
            return Err(ProviderError::new(
                "provider stream contained no assistant content or tool calls",
            ));
        }
        Ok(ProviderTurn {
            content: self.content,
            tool_calls,
            reasoning_details: self.reasoning_details,
        })
    }
}

fn append_reasoning_details(
    target: &mut Vec<Value>,
    serialized_bytes: &mut usize,
    delta: &Value,
) -> Result<bool, ProviderError> {
    let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) else {
        return Ok(false);
    };
    if details.is_empty() {
        return Ok(false);
    }
    let serialized_delta = serde_json::to_vec(details)
        .map_err(|_| ProviderError::new("provider reasoning details could not be serialized"))?;
    let combined_bytes = if target.is_empty() {
        serialized_delta.len()
    } else {
        serialized_bytes
            .saturating_add(serialized_delta.len())
            .saturating_sub(1)
    };
    if combined_bytes > MAX_PROVIDER_REASONING_DETAILS_BYTES {
        return Err(ProviderError::new(
            "provider reasoning details exceeded the response limit",
        ));
    }
    target.extend(details.iter().cloned());
    *serialized_bytes = combined_bytes;
    Ok(true)
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

#[derive(Debug, Default)]
struct SseDecoder {
    line: Vec<u8>,
    data_lines: Vec<String>,
    data_event_bytes: usize,
    stream_bytes: usize,
    result: SseParseResult,
    done: bool,
}

impl SseDecoder {
    fn feed<F>(&mut self, bytes: &[u8], on_data: &mut F) -> Result<bool, ProviderError>
    where
        F: FnMut(Value) -> Result<(), ProviderError>,
    {
        if self.stream_bytes.saturating_add(bytes.len()) > MAX_SSE_STREAM_BYTES {
            return Err(ProviderError::new(
                "provider SSE stream exceeded the response limit",
            ));
        }
        self.stream_bytes += bytes.len();
        for byte in bytes {
            if self.done {
                break;
            }
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                if self.process_line(&line, on_data)? {
                    return Ok(true);
                }
            } else {
                self.line.push(*byte);
                if self.line.len() > MAX_SSE_LINE_BYTES {
                    return Err(ProviderError::new(
                        "provider SSE line exceeded the response limit",
                    ));
                }
            }
        }
        Ok(self.done)
    }

    fn finish<F>(&mut self, on_data: &mut F) -> Result<SseParseResult, ProviderError>
    where
        F: FnMut(Value) -> Result<(), ProviderError>,
    {
        if !self.line.is_empty() && !self.done {
            let line = std::mem::take(&mut self.line);
            self.process_line(&line, on_data)?;
        }
        if !self.done {
            self.dispatch_data(on_data)?;
        }
        Ok(self.result)
    }

    fn process_line<F>(&mut self, raw_line: &[u8], on_data: &mut F) -> Result<bool, ProviderError>
    where
        F: FnMut(Value) -> Result<(), ProviderError>,
    {
        let line = std::str::from_utf8(raw_line)
            .map_err(|_| ProviderError::new("provider stream contained invalid UTF-8"))?
            .trim_end_matches('\r');
        if line.is_empty() {
            return self.dispatch_data(on_data);
        }
        if line.starts_with(':') {
            return Ok(false);
        }
        let (field, value) = line
            .split_once(':')
            .map_or((line, ""), |(field, value)| (field, value));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            let separator_bytes = (!self.data_lines.is_empty()) as usize;
            let added_bytes = separator_bytes.saturating_add(value.len());
            if self.data_event_bytes.saturating_add(added_bytes) > MAX_SSE_EVENT_BYTES {
                return Err(ProviderError::new(
                    "provider SSE data event exceeded the response limit",
                ));
            }
            if self.data_lines.len() >= MAX_SSE_DATA_LINES {
                return Err(ProviderError::new(
                    "provider SSE data line count exceeded the response limit",
                ));
            }
            self.data_event_bytes += added_bytes;
            self.data_lines.push(value.to_owned());
        }
        Ok(false)
    }

    fn dispatch_data<F>(&mut self, on_data: &mut F) -> Result<bool, ProviderError>
    where
        F: FnMut(Value) -> Result<(), ProviderError>,
    {
        if self.data_lines.is_empty() {
            self.data_event_bytes = 0;
            return Ok(false);
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        self.data_event_bytes = 0;
        if data.trim().is_empty() {
            return Ok(false);
        }
        if data == "[DONE]" {
            self.result.received_done = true;
            self.done = true;
            return Ok(true);
        }
        let value: Value = serde_json::from_str(&data)
            .map_err(|_| ProviderError::new("provider sent malformed SSE data"))?;
        self.result.received_payload = true;
        on_data(value)?;
        Ok(false)
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
        let (has_line, line_bytes) = match read_sse_line(reader, &mut line) {
            Ok(result) => result,
            Err(mut error) => {
                if result.received_payload {
                    error.retryable = false;
                }
                return Err(error);
            }
        };
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
            .map_err(|_| ProviderError::new("provider stream contained invalid UTF-8"))?
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
        let buffer = reader.fill_buf().map_err(|error| {
            ProviderError::retryable(format!("provider stream read failed: {error}"))
        })?;
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
    use std::io::{BufRead, BufReader, Cursor, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn subagent_tool_descriptions_prefer_automatic_completion_over_polling() {
        let request = chat_request(
            "model",
            &[ChatMessage::user("hello".to_owned())],
            &None,
            true,
            true,
        );
        let tools = request["tools"].as_array().expect("model tools");
        let description = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["function"]["name"] == name)
                .and_then(|tool| tool["function"]["description"].as_str())
                .expect("tool description")
        };

        let spawn = description("spawn_subagent");
        assert!(spawn.contains("Continue your own work without waiting"));
        assert!(spawn.contains("resumes the attached logical turn"));
        assert!(spawn.contains("instead of creating user input or a separate user turn"));
        assert!(spawn.contains("Do not poll with check_subagent"));
        assert!(spawn.contains("always inherits the current session model and reasoning effort"));
        assert!(spawn.contains("cannot override either setting"));
        let spawn_properties = tools
            .iter()
            .find(|tool| tool["function"]["name"] == "spawn_subagent")
            .and_then(|tool| tool["function"]["parameters"]["properties"].as_object())
            .expect("spawn_subagent properties");
        assert_eq!(spawn_properties.keys().collect::<Vec<_>>(), vec!["task"]);

        let check = description("check_subagent");
        assert!(check.contains("Do not poll repeatedly"));
        assert!(check.contains("resumes the attached logical turn"));
        assert!(check.contains("continue your own work instead"));

        assert!(description("wait_subagent").contains("timeout only ends the wait"));
        assert!(description("send_subagent").contains("next safe provider boundary"));
        assert!(
            description("cancel_subagent").contains("nearest safe provider or command boundary")
        );
    }

    #[test]
    fn retry_policy_only_marks_transient_http_statuses() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(transient_http_status(status));
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!transient_http_status(status));
        }
    }

    #[test]
    fn compaction_request_does_not_include_tools() {
        let normal = chat_request(
            "model",
            &[ChatMessage::user("hello".to_owned())],
            &None,
            true,
            true,
        );
        let compact = chat_request(
            "model",
            &[ChatMessage::user("hello".to_owned())],
            &None,
            false,
            false,
        );

        assert!(normal.get("tools").is_some());
        assert!(normal.get("max_tokens").is_none());
        assert!(compact.get("tools").is_none());
        assert_eq!(compact["max_tokens"], COMPACTION_MAX_SUMMARY_TOKENS);
    }

    #[test]
    fn model_catalog_reads_nested_and_compatible_effort_metadata() {
        let openrouter = serde_json::json!({
            "reasoning": {
                "supported_efforts": ["max", "xhigh", "high", "medium", "low", "none"]
            }
        });
        assert_eq!(
            model_efforts(&openrouter),
            Some(vec![
                "max".to_owned(),
                "xhigh".to_owned(),
                "high".to_owned(),
                "medium".to_owned(),
                "low".to_owned(),
                "none".to_owned(),
            ])
        );

        let compatible = serde_json::json!({
            "supported_reasoning_efforts": ["light", "medium", "max", "light", ""]
        });
        assert_eq!(
            model_efforts(&compatible),
            Some(vec![
                "light".to_owned(),
                "medium".to_owned(),
                "max".to_owned()
            ])
        );
        assert_eq!(model_efforts(&serde_json::json!({})), None);
    }

    #[test]
    fn model_catalog_context_window_matches_configured_model() {
        let payload = serde_json::json!({
            "data": [
                {"id": "other", "context_length": 8_000},
                {"id": "provider/model", "context_length": 128_000}
            ]
        });

        assert_eq!(
            context_window_from_models(&payload, "provider/model"),
            Some(128_000)
        );
        assert_eq!(context_window_from_models(&payload, "missing"), None);
    }

    #[test]
    fn model_catalog_context_window_accepts_provider_fallback_fields() {
        let payload = serde_json::json!({
            "data": [{
                "id": "provider/model",
                "top_provider": {"context_length": 64_000}
            }]
        });

        assert_eq!(
            context_window_from_models(&payload, "provider/model"),
            Some(64_000)
        );
    }

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
    fn cancellable_accumulator_accepts_more_than_sixty_four_tool_calls() {
        let mut accumulator = ProviderAccumulator::default();
        let tool_calls = (0..65)
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "id": format!("call-{index}"),
                    "function": {
                        "name": "cmd",
                        "arguments": "{\"command\":\"true\"}"
                    }
                })
            })
            .collect::<Vec<_>>();
        accumulator
            .on_data(
                serde_json::json!({
                    "choices": [{
                        "delta": {"tool_calls": tool_calls},
                        "finish_reason": "tool_calls"
                    }]
                }),
                "provider-secret",
                &mut |_| Ok(()),
            )
            .expect("tool-call chunk");

        let turn = accumulator.finish().expect("provider turn");
        assert_eq!(turn.tool_calls.len(), 65);
    }

    #[test]
    fn reasoning_stream_event_is_emitted_once_before_assistant_text() {
        let mut accumulator = ProviderAccumulator::default();
        let mut events = Vec::new();
        let mut on_event = |event| {
            match event {
                ProviderStreamEvent::ReasoningStarted => events.push("started".to_owned()),
                ProviderStreamEvent::Text(text) => events.push(text),
            }
            Ok(())
        };

        accumulator
            .on_data(
                serde_json::json!({
                    "choices": [{
                        "delta": {
                            "reasoning_details": [{"type": "reasoning.text", "text": "thinking"}]
                        }
                    }]
                }),
                "provider-secret",
                &mut on_event,
            )
            .expect("reasoning chunk");
        accumulator
            .on_data(
                serde_json::json!({
                    "choices": [{"delta": {"content": "answer"}}]
                }),
                "provider-secret",
                &mut on_event,
            )
            .expect("answer chunk");

        assert_eq!(events, vec!["started".to_owned(), "answer".to_owned()]);
    }

    #[test]
    fn accumulates_reasoning_details_with_fragmented_tool_calls() {
        let mut accumulator = ProviderAccumulator::default();
        accumulator
            .on_data(
                serde_json::json!({
                    "choices": [{
                        "delta": {
                            "reasoning_details": [{
                                "type": "reasoning.text",
                                "text": "part one"
                            }]
                        }
                    }]
                }),
                "provider-secret",
                &mut |_| Ok(()),
            )
            .expect("first provider chunk");
        accumulator
            .on_data(
                serde_json::json!({
                    "choices": [{
                        "delta": {
                            "reasoning_details": [{
                                "type": "reasoning.text",
                                "text": "part two"
                            }],
                            "tool_calls": [{
                                "index": 0,
                                "id": "call-1",
                                "function": {
                                    "name": "cmd",
                                    "arguments": "{\"command\":\"true\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }]
                }),
                "provider-secret",
                &mut |_| Ok(()),
            )
            .expect("second provider chunk");

        let partial = accumulator.partial_turn();
        assert_eq!(partial.reasoning_details.len(), 2);
        let turn = accumulator.finish().expect("provider turn");
        assert_eq!(
            turn.reasoning_details,
            vec![
                json!({"type": "reasoning.text", "text": "part one"}),
                json!({"type": "reasoning.text", "text": "part two"}),
            ]
        );
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "cmd");
    }

    #[test]
    fn accumulates_many_small_reasoning_details_and_rejects_overflow_atomically() {
        const FRAGMENT_COUNT: usize = 4096;
        let mut details = Vec::new();
        let mut serialized_bytes = 0;
        let delta = serde_json::json!({
            "reasoning_details": [{
                "type": "reasoning.text",
                "text": "x".repeat(64)
            }]
        });
        for _ in 0..FRAGMENT_COUNT {
            append_reasoning_details(&mut details, &mut serialized_bytes, &delta)
                .expect("small reasoning detail");
        }
        assert_eq!(details.len(), FRAGMENT_COUNT);

        let first_chunk_delta = serde_json::json!({
            "reasoning_details": [{
                "type": "reasoning.text",
                "text": "x".repeat(500 * 1024)
            }]
        });
        let first_chunk_bytes = serde_json::to_vec(&first_chunk_delta["reasoning_details"])
            .expect("first reasoning detail chunk")
            .len();
        assert!(first_chunk_bytes < MAX_PROVIDER_REASONING_DETAILS_BYTES);
        append_reasoning_details(&mut details, &mut serialized_bytes, &first_chunk_delta)
            .expect("first individually bounded reasoning detail chunk");
        assert!(serialized_bytes <= MAX_PROVIDER_REASONING_DETAILS_BYTES);

        let second_chunk_delta = serde_json::json!({
            "reasoning_details": [{
                "type": "reasoning.text",
                "text": "x".repeat(200 * 1024)
            }]
        });
        let second_chunk_bytes = serde_json::to_vec(&second_chunk_delta["reasoning_details"])
            .expect("second reasoning detail chunk")
            .len();
        assert!(second_chunk_bytes < MAX_PROVIDER_REASONING_DETAILS_BYTES);
        assert!(
            serialized_bytes
                .saturating_add(second_chunk_bytes)
                .saturating_sub(1)
                > MAX_PROVIDER_REASONING_DETAILS_BYTES
        );

        let prior_details = details.clone();
        let prior_bytes = serialized_bytes;
        let error =
            append_reasoning_details(&mut details, &mut serialized_bytes, &second_chunk_delta)
                .expect_err("reasoning details limit");

        assert_eq!(
            error.to_string(),
            "provider reasoning details exceeded the response limit"
        );
        assert_eq!(details, prior_details);
        assert_eq!(serialized_bytes, prior_bytes);
        assert_eq!(
            serde_json::to_vec(&details)
                .expect("accumulated reasoning details")
                .len(),
            serialized_bytes
        );
        assert!(serialized_bytes <= MAX_PROVIDER_REASONING_DETAILS_BYTES);
    }

    #[test]
    fn rejects_reasoning_details_that_exceed_the_serialized_response_limit_before_retaining_them() {
        let mut accumulator = ProviderAccumulator::default();
        let retained = serde_json::json!({
            "choices": [{
                "delta": {
                    "reasoning_details": [{
                        "type": "reasoning.text",
                        "text": "retained"
                    }]
                }
            }]
        });
        accumulator
            .on_data(retained, "provider-secret", &mut |_| Ok(()))
            .expect("details within limit");

        let oversized = "x".repeat(MAX_PROVIDER_REASONING_DETAILS_BYTES);
        let error = accumulator
            .on_data(
                serde_json::json!({
                    "choices": [{
                        "delta": {
                            "reasoning_details": [{"text": oversized}]
                        }
                    }]
                }),
                "provider-secret",
                &mut |_| Ok(()),
            )
            .expect_err("reasoning details limit");
        assert_eq!(
            error.to_string(),
            "provider reasoning details exceeded the response limit"
        );
        assert_eq!(
            accumulator.reasoning_details,
            vec![serde_json::json!({
                "type": "reasoning.text",
                "text": "retained"
            })]
        );
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
                effort: None,
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
            effort: None,
        };
        assert!(Provider::new(&settings).is_ok());
        std::env::remove_var(environment);
    }

    #[test]
    fn accepts_a_configurable_effort() {
        let environment = format!("LUCY_PROVIDER_EFFORT_OK_{}", std::process::id());
        std::env::set_var(&environment, "provider-secret");
        let settings = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: environment.clone(),
            effort: Some("high".to_owned()),
        };
        assert!(Provider::new(&settings).is_ok());
        std::env::remove_var(environment);
    }

    #[test]
    fn empty_effort_is_rejected_without_echoing_the_key() {
        let environment = format!("LUCY_PROVIDER_EFFORT_EMPTY_{}", std::process::id());
        std::env::set_var(&environment, "provider-secret");
        for effort in ["", "   ", "\t"] {
            let settings = LlmSettings {
                base_url: "http://localhost".to_owned(),
                model: "model".to_owned(),
                api_key_env: environment.clone(),
                effort: Some(effort.to_owned()),
            };
            let error = match Provider::new(&settings) {
                Ok(_) => panic!("empty effort should be rejected: {effort:?}"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("llm.effort must not be empty"));
            assert!(!error.to_string().contains("provider-secret"));
        }
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
            effort: None,
        };
        let error = match Provider::new(&settings) {
            Ok(_) => panic!("missing key should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "missing provider API key");
        assert!(!error.to_string().contains(&environment));
    }

    #[test]
    fn cancellable_stream_stops_a_stalled_provider_without_waiting_for_timeout() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let (sent, sent_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let mut request = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                request.read_line(&mut line).expect("header");
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length:") {
                    content_length = value.trim().parse::<usize>().expect("length");
                }
            }
            let mut body = vec![0; content_length];
            request.read_exact(&mut body).expect("body");

            let payload = serde_json::json!({
                "choices": [{"delta": {"content": "partial"}, "finish_reason": null}]
            });
            let event = format!("data: {payload}\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n{:x}\r\n{}\r\n",
                event.len(), event
            );
            stream.write_all(response.as_bytes()).expect("response");
            stream.flush().expect("flush");
            sent.send(()).expect("body readiness");
            thread::sleep(Duration::from_millis(500));
        });

        let environment = format!("LUCY_PROVIDER_CANCEL_{}", std::process::id());
        std::env::set_var(&environment, "provider-secret");
        let provider = Provider::new(&LlmSettings {
            base_url: format!("http://{address}/v1"),
            model: "model".to_owned(),
            api_key_env: environment.clone(),
            effort: None,
        })
        .expect("provider");
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            let mut received = String::new();
            let result = provider.stream_chat_cancellable(
                &[ChatMessage::user("hello".to_owned())],
                &mut |text| {
                    received.push_str(text);
                    Ok(())
                },
                &worker_token,
            );
            (result, received)
        });
        sent_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("body was sent");
        let started = Instant::now();
        assert!(token.cancel());
        let (result, received) = worker.join().expect("provider worker");
        assert!(started.elapsed() < Duration::from_millis(400));
        let error = result.expect_err("cancellation");
        assert!(error.is_cancelled());
        assert!(received.is_empty() || received == "partial");
        server.join().expect("server");
        std::env::remove_var(environment);
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

    fn read_request_headers(stream: &TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone request"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("request header");
            if line == "\r\n" || line.is_empty() {
                return;
            }
        }
    }

    fn response_body(text: &str) -> String {
        let payload = serde_json::json!({
            "choices": [{"delta": {"content": text}, "finish_reason": null}]
        });
        let finish = serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}]
        });
        format!("data: {payload}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
    }

    fn provider_for(address: std::net::SocketAddr, read_timeout: Duration) -> (Provider, String) {
        let environment = format!(
            "LUCY_PROVIDER_STREAM_TEST_{}_{}",
            std::process::id(),
            address.port()
        );
        std::env::set_var(&environment, "provider-secret");
        let settings = LlmSettings {
            base_url: format!("http://{address}/v1"),
            model: "model".to_owned(),
            api_key_env: environment.clone(),
            effort: None,
        };
        let mut provider = Provider::new(&settings).expect("provider");
        provider.async_client = AsyncClient::builder()
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(read_timeout)
            .build()
            .expect("test async client");
        (provider, environment)
    }

    #[test]
    fn worker_stream_can_exceed_idle_interval_without_total_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let parts = (0..5)
            .map(|index| {
                let payload = serde_json::json!({
                    "choices": [{
                        "delta": {"content": format!("part-{index}")},
                        "finish_reason": null
                    }]
                });
                format!("data: {payload}\n\n")
            })
            .chain([
                format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "choices": [{"delta": {}, "finish_reason": "stop"}]
                    })
                ),
                "data: [DONE]\n\n".to_owned(),
            ])
            .collect::<Vec<_>>();
        let body = parts.concat();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            read_request_headers(&stream);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).expect("header");
            stream.flush().expect("header flush");
            for part in parts {
                stream.write_all(part.as_bytes()).expect("SSE part");
                stream.flush().expect("SSE flush");
                thread::sleep(Duration::from_millis(25));
            }
        });

        let (provider, environment) = provider_for(address, Duration::from_millis(80));
        let cancellation = CancellationToken::new();
        let started = Instant::now();
        let mut output = String::new();
        let turn = provider
            .stream_chat_cancellable_with_options(
                &[ChatMessage::user("worker task".to_owned())],
                &mut |text| {
                    output.push_str(text);
                    Ok(())
                },
                &cancellation,
                true,
                false,
            )
            .expect("long worker stream");

        assert!(started.elapsed() >= Duration::from_millis(80));
        assert_eq!(turn.content, output);
        assert!(output.contains("part-4"));
        server.join().expect("server");
        std::env::remove_var(environment);
    }

    #[test]
    fn retries_a_pre_payload_stream_failure_once_and_classifies_it() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("address");
        let body = response_body("retried");
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            for attempt in 0..2 {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "provider did not retry");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                read_request_headers(&stream);
                if attempt == 0 {
                    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100\r\nConnection: close\r\n\r\n";
                    stream.write_all(header.as_bytes()).expect("failed header");
                    stream.flush().expect("failed flush");
                } else {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(header.as_bytes()).expect("success header");
                    stream.write_all(body.as_bytes()).expect("success body");
                    stream.flush().expect("success flush");
                }
            }
        });

        let (provider, environment) = provider_for(address, Duration::from_secs(1));
        let cancellation = CancellationToken::new();
        let mut output = String::new();
        let turn = provider
            .stream_chat_cancellable_with_options(
                &[ChatMessage::user("retry".to_owned())],
                &mut |text| {
                    output.push_str(text);
                    Ok(())
                },
                &cancellation,
                true,
                false,
            )
            .expect("retry succeeds");

        assert_eq!(turn.content, "retried");
        assert_eq!(output, "retried");
        server.join().expect("server");
        std::env::remove_var(environment);
    }

    #[test]
    fn does_not_retry_after_partial_provider_output() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("address");
        let partial = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{"delta": {"content": "partial"}, "finish_reason": null}]
            })
        );
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "provider request missing");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            read_request_headers(&stream);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                partial.len() + 10
            );
            stream.write_all(header.as_bytes()).expect("partial header");
            stream.write_all(partial.as_bytes()).expect("partial body");
            stream.flush().expect("partial flush");
            thread::sleep(Duration::from_millis(150));
            assert!(matches!(
                listener.accept(),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
            ));
        });

        let (provider, environment) = provider_for(address, Duration::from_secs(1));
        let cancellation = CancellationToken::new();
        let mut output = String::new();
        let error = provider
            .stream_chat_cancellable_with_options(
                &[ChatMessage::user("partial".to_owned())],
                &mut |text| {
                    output.push_str(text);
                    Ok(())
                },
                &cancellation,
                true,
                false,
            )
            .expect_err("partial stream must fail");

        let message = error.to_string();
        assert!(message.contains("provider stream read failed"));
        assert!([
            "(timeout)",
            "(connection)",
            "(body)",
            "(decode)",
            "(request)",
            "(transport)",
        ]
        .iter()
        .any(|kind| message.contains(kind)));
        assert_eq!(output, "partial");
        server.join().expect("server");
        std::env::remove_var(environment);
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
