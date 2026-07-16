use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::command::execute;
use crate::config::{Config, DEFAULT_API_KEY_ENV};
use crate::context::resolve_boot_context_with_api_key_env;
use crate::model::{ChatMessage, ChatToolCall};
use crate::protocol::ProtocolWriter;
use crate::provider::Provider;
use crate::redaction::{is_structural_key, redact_secret, redaction_marker};
use crate::session::Session;

#[derive(Debug)]
struct CliOptions {
    session: Option<String>,
    list_sessions: bool,
}

#[derive(Debug, Deserialize)]
struct InputRecord {
    #[serde(rename = "type")]
    record_type: String,
    text: Option<String>,
}

const MAX_TOOL_ROUNDS: usize = 32;
const MAX_TOOL_CALLS_PER_MESSAGE: usize = 64;

pub fn run_cli<R, W, E>(args: &[String], input: R, output: W, diagnostics: E) -> i32
where
    R: BufRead,
    W: Write,
    E: Write,
{
    let home = match home_directory() {
        Ok(home) => home,
        Err(error) => {
            let mut diagnostics = diagnostics;
            write_diagnostic(&mut diagnostics, &error);
            return 1;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_error) => {
            let mut diagnostics = diagnostics;
            write_diagnostic(&mut diagnostics, "unable to resolve cwd");
            return 1;
        }
    };
    run_cli_at_home(args, input, output, diagnostics, &home, &cwd)
}

pub fn run_cli_at_home<R, W, E>(
    args: &[String],
    input: R,
    output: W,
    mut diagnostics: E,
    home: &Path,
    cwd: &Path,
) -> i32
where
    R: BufRead,
    W: Write,
    E: Write,
{
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            let mut diagnostics = diagnostics;
            write_diagnostic(&mut diagnostics, &error);
            return 2;
        }
    };

    let mut protocol = ProtocolWriter::new(output);
    if options.list_sessions {
        if let Err(error) = Config::ensure_exists(home) {
            write_diagnostic(&mut diagnostics, &error.to_string());
            return 1;
        }
        return match Session::list(home) {
            Ok(sessions) => {
                for session in sessions {
                    if let Err(error) = protocol.emit_serializable(&session) {
                        write_diagnostic(
                            &mut diagnostics,
                            &format!("unable to write session metadata: {error}"),
                        );
                        return 1;
                    }
                }
                0
            }
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                1
            }
        };
    }

    let (session, provider, resumed) = if let Some(id) = options.session {
        let session = match Session::resume(home, &id) {
            Ok(session) => session,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                return 1;
            }
        };
        let provider = match Provider::new(&session.llm) {
            Ok(provider) => provider,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                return 1;
            }
        };
        (session, provider, true)
    } else {
        let config = match Config::load_or_create(home) {
            Ok(config) => config,
            Err(error) => {
                write_diagnostic(&mut diagnostics, &error.to_string());
                return 1;
            }
        };
        let configured_secret = configured_api_key(&config);
        let api_key_env = configured_api_key_env(&config);
        let context = match resolve_boot_context_with_api_key_env(
            home,
            cwd,
            &config.system_prompt,
            api_key_env.as_deref(),
        ) {
            Ok(context) => context,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    configured_secret.as_deref(),
                );
                return 1;
            }
        };
        let llm = match config.resolved_llm() {
            Ok(llm) => llm,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    configured_secret.as_deref(),
                );
                return 1;
            }
        };
        let provider = match Provider::new(&llm) {
            Ok(provider) => provider,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    configured_secret.as_deref(),
                );
                return 1;
            }
        };
        let boot_system_prompt = redact_secret(&context.system_prompt, Some(provider.api_key()));
        let session = match Session::create_with_secret(
            home,
            cwd,
            boot_system_prompt,
            llm,
            Some(provider.api_key()),
        ) {
            Ok(session) => session,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &error.to_string(),
                    Some(provider.api_key()),
                );
                return 1;
            }
        };
        (session, provider, false)
    };

    let mut harness = Harness { session, provider };
    if let Err(error) = protocol.session(&harness.session.id, resumed) {
        write_diagnostic_safe(
            &mut diagnostics,
            &format!("unable to write session event: {error}"),
            Some(harness.provider.api_key()),
        );
        return 1;
    }

    for line in input.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &format!("unable to read stdin: {error}"),
                    Some(harness.provider.api_key()),
                );
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let text = match parse_input_message(&line) {
            Ok(text) => text,
            Err(error) => {
                let error = redact_secret(&error, Some(harness.provider.api_key()));
                if let Err(write_error) = protocol.error(&error) {
                    write_diagnostic_safe(
                        &mut diagnostics,
                        &format!("unable to write protocol error: {write_error}"),
                        Some(harness.provider.api_key()),
                    );
                    return 1;
                }
                continue;
            }
        };
        if let Err(error) = harness.handle_message(&text, &mut protocol) {
            let error = redact_secret(&error, Some(harness.provider.api_key()));
            if let Err(write_error) = protocol.error(&error) {
                write_diagnostic_safe(
                    &mut diagnostics,
                    &format!("unable to write protocol error: {write_error}"),
                    Some(harness.provider.api_key()),
                );
                return 1;
            }
        }
    }

    0
}

struct Harness {
    session: Session,
    provider: Provider,
}

impl Harness {
    fn handle_message<W: Write>(
        &mut self,
        text: &str,
        protocol: &mut ProtocolWriter<W>,
    ) -> Result<(), String> {
        let secret = self.provider.api_key().to_owned();
        let safe_text = redact_secret(text, Some(&secret));
        self.session
            .append_message(ChatMessage::user(safe_text))
            .map_err(|error| error.to_string())?;

        let mut tool_rounds = 0;
        let mut tool_calls: usize = 0;
        loop {
            if tool_rounds >= MAX_TOOL_ROUNDS {
                return Err(format!(
                    "tool loop exceeded maximum of {MAX_TOOL_ROUNDS} provider rounds"
                ));
            }
            let messages = self.session.provider_messages();
            let mut raw_content = String::new();
            let mut redactor = SecretRedactor::new(&secret);
            let stream_result = {
                let mut on_text = |delta: &str| {
                    raw_content.push_str(delta);
                    redactor.push(delta, |safe_delta| protocol.assistant_delta(safe_delta))
                };
                self.provider.stream_chat(&messages, &mut on_text)
            };
            redactor
                .finish(|safe_delta| protocol.assistant_delta(safe_delta))
                .map_err(|error| format!("unable to write assistant delta: {error}"))?;
            let turn = stream_result.map_err(|error| error.to_string())?;

            if turn.tool_calls.iter().any(|call| call.name != "cmd") {
                return Err("provider requested an unsupported tool".to_owned());
            }
            if tool_calls.saturating_add(turn.tool_calls.len()) > MAX_TOOL_CALLS_PER_MESSAGE {
                return Err(format!(
                    "tool call budget exceeded maximum of {MAX_TOOL_CALLS_PER_MESSAGE} calls per input message"
                ));
            }

            let safe_tool_calls = turn
                .tool_calls
                .iter()
                .map(|call| ChatToolCall {
                    id: redact_secret(&call.id, Some(&secret)),
                    name: redact_secret(&call.name, Some(&secret)),
                    arguments: redact_tool_arguments(&call.arguments, &secret),
                })
                .collect::<Vec<_>>();
            let assistant = ChatMessage::assistant(
                redact_secret(&raw_content, Some(&secret)),
                safe_tool_calls.clone(),
            );
            self.session
                .append_message(assistant)
                .map_err(|error| error.to_string())?;

            if safe_tool_calls.is_empty() {
                protocol
                    .turn_end()
                    .map_err(|error| format!("unable to write turn end: {error}"))?;
                return Ok(());
            }

            tool_rounds += 1;
            tool_calls += safe_tool_calls.len();
            for (raw_call, safe_call) in turn.tool_calls.iter().zip(safe_tool_calls.iter()) {
                protocol
                    .tool_call(&safe_call.id, &safe_call.name, &safe_call.arguments)
                    .map_err(|error| format!("unable to write tool call: {error}"))?;

                let result = redact_json_value(
                    serde_json::to_value(execute(
                        &raw_call.arguments,
                        &self.session.cwd,
                        self.provider.api_key_env(),
                        Some(&secret),
                    ))
                    .map_err(|error| format!("unable to encode cmd result: {error}"))?,
                    &secret,
                );
                let tool_content = serde_json::to_string(&result)
                    .map_err(|error| format!("unable to encode tool result: {error}"))?;
                self.session
                    .append_message(ChatMessage::tool(
                        safe_call.id.clone(),
                        safe_call.name.clone(),
                        redact_secret(&tool_content, Some(&secret)),
                    ))
                    .map_err(|error| error.to_string())?;
                protocol
                    .tool_result(&safe_call.id, &safe_call.name, result)
                    .map_err(|error| format!("unable to write tool result: {error}"))?;
            }
        }
    }
}

struct SecretRedactor {
    secret_text: String,
    secret: Vec<char>,
    marker: String,
    pending: String,
}

impl SecretRedactor {
    fn new(secret: &str) -> Self {
        Self {
            secret_text: secret.to_owned(),
            secret: secret.chars().collect(),
            marker: redaction_marker(secret).unwrap_or_default(),
            pending: String::new(),
        }
    }

    fn push<F>(&mut self, text: &str, mut emit: F) -> io::Result<()>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        if self.secret.is_empty() {
            return emit(text);
        }

        let mut output = String::new();
        for character in text.chars() {
            self.pending.push(character);
            if self.pending.chars().eq(self.secret.iter().copied()) {
                self.pending.clear();
                output.push_str(&self.marker);
                continue;
            }
            if self.pending_is_secret_prefix() {
                continue;
            }

            let pending = self.pending.chars().collect::<Vec<_>>();
            let suffix_len = (1..pending.len())
                .rev()
                .find(|length| {
                    pending[pending.len() - length..].iter().copied().eq(self
                        .secret
                        .iter()
                        .copied()
                        .take(*length))
                })
                .unwrap_or(0);
            let safe_len = pending.len() - suffix_len;
            output.extend(pending[..safe_len].iter());
            self.pending = pending[safe_len..].iter().collect();
        }

        if output.is_empty() {
            Ok(())
        } else {
            let safe_output = redact_secret(&output, Some(&self.secret_text));
            emit(&safe_output)
        }
    }

    fn finish<F>(&mut self, mut emit: F) -> io::Result<()>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return Ok(());
        }
        let safe_pending = redact_secret(&pending, Some(&self.secret_text));
        emit(&safe_pending)
    }

    fn pending_is_secret_prefix(&self) -> bool {
        let length = self.pending.chars().count();
        length < self.secret.len()
            && self
                .pending
                .chars()
                .zip(self.secret.iter().copied())
                .all(|(pending, secret)| pending == secret)
    }
}

fn redact_tool_arguments(arguments: &str, secret: &str) -> String {
    let fallback = || "{}".to_owned();
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        // Do not preserve malformed provider JSON: an escaped key could be
        // decoded later by a protocol or session consumer.
        return fallback();
    };
    let Some(object) = value.as_object() else {
        return fallback();
    };
    if object.len() != 1 || !object.get("command").is_some_and(Value::is_string) {
        return fallback();
    }
    let redacted = redact_json_value(value, secret);
    serde_json::to_string(&redacted).unwrap_or_else(|_| fallback())
}

fn redact_json_value(value: Value, secret: &str) -> Value {
    match value {
        Value::String(text) => Value::String(redact_secret(&text, Some(secret))),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_json_value(value, secret))
                .collect(),
        ),
        Value::Object(object) => {
            let marker = redaction_marker(secret).unwrap_or_default();
            let mut redacted = Map::new();
            for (key, value) in object {
                let mut safe_key = if is_structural_key(&key) {
                    key
                } else {
                    redact_secret(&key, Some(secret))
                };
                if redacted.contains_key(&safe_key) {
                    if marker.is_empty() {
                        continue;
                    }
                    while redacted.contains_key(&safe_key) {
                        safe_key.push_str(&marker);
                    }
                }
                redacted.insert(safe_key, redact_json_value(value, secret));
            }
            Value::Object(redacted)
        }
        value => value,
    }
}

fn parse_args(args: &[String]) -> Result<CliOptions, String> {
    let mut session = None;
    let mut list_sessions = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--session" => {
                if list_sessions || session.is_some() {
                    return Err("--session cannot be combined or repeated".to_owned());
                }
                index += 1;
                let Some(id) = args.get(index) else {
                    return Err("--session requires an id".to_owned());
                };
                session = Some(id.clone());
            }
            "--list-sessions" => {
                if session.is_some() || list_sessions {
                    return Err("--list-sessions cannot be combined or repeated".to_owned());
                }
                list_sessions = true;
            }
            "--help" | "-h" => {
                return Err("usage: lucy [--session <id>] [--list-sessions]".to_owned());
            }
            _ => return Err("unknown argument".to_owned()),
        }
        index += 1;
    }
    Ok(CliOptions {
        session,
        list_sessions,
    })
}

fn parse_input_message(line: &str) -> Result<String, String> {
    let record: InputRecord = serde_json::from_str(line)
        .map_err(|_| "input must be a JSONL message record".to_owned())?;
    if record.record_type != "message" {
        return Err("input record type must be message".to_owned());
    }
    record
        .text
        .ok_or_else(|| "message record requires a text string".to_owned())
}

fn home_directory() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; Lucy needs a user home directory".to_owned())
}

fn configured_api_key_env(config: &Config) -> Option<String> {
    let api_key_env = config
        .llm
        .api_key_env
        .as_deref()
        .unwrap_or(DEFAULT_API_KEY_ENV)
        .trim();
    (!api_key_env.is_empty()).then(|| api_key_env.to_owned())
}

fn configured_api_key(config: &Config) -> Option<String> {
    configured_api_key_env(config)
        .and_then(|api_key_env| std::env::var(api_key_env).ok())
        .filter(|secret| !secret.is_empty())
}

fn write_diagnostic_safe<W: Write>(diagnostics: &mut W, message: &str, secret: Option<&str>) {
    write_diagnostic_safe_with_environment(
        diagnostics,
        message,
        secret,
        std::env::vars().map(|(_, value)| value),
    );
}

fn write_diagnostic_safe_with_environment<W, I>(
    diagnostics: &mut W,
    message: &str,
    secret: Option<&str>,
    environment_values: I,
) where
    W: Write,
    I: IntoIterator<Item = String>,
{
    let mut safe_line = format!("lucy: {message}");
    safe_line = redact_secret(&safe_line, secret);
    let mut environment_secrets = environment_values
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    environment_secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for environment_secret in environment_secrets {
        safe_line = redact_secret(&safe_line, Some(&environment_secret));
    }
    let _ = writeln!(diagnostics, "{safe_line}");
}

fn write_diagnostic<W: Write>(diagnostics: &mut W, message: &str) {
    write_diagnostic_safe(diagnostics, message, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_only_message_records() {
        assert_eq!(
            parse_input_message(r#"{"type":"message","text":"hello"}"#).expect("message"),
            "hello"
        );
        assert!(parse_input_message(r#"{"type":"event","text":"hello"}"#).is_err());
        assert_eq!(
            parse_input_message(r#"{"type":"message","text":""}"#).expect("empty message"),
            ""
        );
    }

    #[test]
    fn redactor_does_not_leak_a_secret_across_deltas() {
        let mut redactor = SecretRedactor::new("secret");
        let mut output = Vec::new();
        redactor
            .push("prefix sec", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("push");
        redactor
            .push("ret suffix", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("push");
        redactor
            .finish(|text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("finish");
        let output = output.join("");
        assert_eq!(
            output,
            format!("prefix {} suffix", redaction_marker("secret").unwrap())
        );
        assert!(!output.contains("secret"));
    }

    #[test]
    fn redactor_handles_secrets_introduced_by_protocol_json_escaping() {
        let mut redactor = SecretRedactor::new("n0");
        let mut output = String::new();
        redactor
            .push("\n0", |text| {
                output.push_str(text);
                Ok(())
            })
            .expect("push");
        redactor
            .finish(|text| {
                output.push_str(text);
                Ok(())
            })
            .expect("finish");
        assert!(!output.contains("n0"));
        assert_eq!(output, redaction_marker("n0").unwrap());
    }

    #[test]
    fn redactor_does_not_emit_a_secret_when_it_completes_at_a_delta_boundary() {
        let mut redactor = SecretRedactor::new("secret");
        let mut output = Vec::new();
        redactor
            .push("xsecre", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("first delta");
        redactor
            .push("t", |text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("second delta");
        redactor
            .finish(|text| {
                output.push(text.to_owned());
                Ok(())
            })
            .expect("finish");

        let output = output.join("");
        assert_eq!(output, format!("x{}", redaction_marker("secret").unwrap()));
        assert!(!output.contains("secret"));
    }

    #[test]
    fn streaming_redaction_handles_marker_collision_keys_at_delta_boundaries() {
        for secret in ["REDACTED", "[REDACTED]"] {
            let mut redactor = SecretRedactor::new(secret);
            let split = secret.len() / 2;
            let (first, second) = secret.split_at(split);
            let mut output = String::new();
            redactor
                .push(first, |text| {
                    output.push_str(text);
                    Ok(())
                })
                .expect("first delta");
            redactor
                .push(second, |text| {
                    output.push_str(text);
                    Ok(())
                })
                .expect("second delta");
            redactor
                .finish(|text| {
                    output.push_str(text);
                    Ok(())
                })
                .expect("finish");
            assert!(!output.contains(secret));
            assert!(output.len() <= secret.len());
        }
    }

    #[test]
    fn malformed_tool_arguments_use_a_safe_copy() {
        let secret = "provider-secret";
        let escaped = secret
            .chars()
            .map(|character| format!(r#"\u{:04x}"#, character as u32))
            .collect::<String>();
        let arguments = format!(r#"{{"command":"{escaped}""#);
        let safe = redact_tool_arguments(&arguments, secret);
        assert_eq!(safe, "{}");
        serde_json::from_str::<Value>(&safe).expect("safe arguments JSON");
        assert!(!safe.contains(secret));
        assert!(!safe.contains(&escaped));
        for invalid in ["[]", "{\"command\":1}", "{\"other\":\"value\"}"] {
            assert_eq!(redact_tool_arguments(invalid, secret), "{}");
        }
    }

    #[test]
    fn structured_redaction_preserves_tool_and_result_schema_keys() {
        let secret = "provider-secret";
        let value = serde_json::json!({
            "command": "printf provider-secret",
            "stdout": "provider-secret",
            "stderr": "ordinary",
            "exit_code": 0,
            "timed_out": false,
            "stdout_truncated": false,
            "stderr_truncated": false,
            "unknown-provider-secret": "provider-secret"
        });
        let redacted = redact_json_value(value, secret);
        for key in [
            "command",
            "stdout",
            "stderr",
            "exit_code",
            "timed_out",
            "stdout_truncated",
            "stderr_truncated",
        ] {
            assert!(redacted.get(key).is_some(), "missing schema key: {key}");
        }
        let encoded = serde_json::to_string(&redacted).expect("redacted JSON");
        assert!(!encoded.contains(secret));
        assert!(redacted.get("unknown-provider-secret").is_none());
    }

    #[test]
    fn structured_redaction_preserves_typed_values_even_for_a_pathological_key() {
        let value = serde_json::json!({
            "exit_code": 0,
            "timed_out": false,
            "stdout_truncated": true,
            "error": null,
        });
        let redacted = redact_json_value(value, "0");
        assert!(redacted["exit_code"].is_number());
        assert!(redacted["timed_out"].is_boolean());
        assert!(redacted["stdout_truncated"].is_boolean());
        assert!(redacted["error"].is_null());
    }

    #[test]
    fn malformed_input_error_does_not_echo_secret_bearing_input() {
        let error =
            parse_input_message(r#"{"type":"message","text":"provider-secret","unexpected":}"#)
                .expect_err("invalid input");
        assert!(!error.contains("provider-secret"));
    }

    #[test]
    fn malformed_input_is_an_error_event_and_not_diagnostic_json() {
        let mut output = Vec::new();
        // This test only exercises the parser path without starting a provider.
        let error = parse_input_message("not json").expect_err("invalid input");
        let mut protocol = ProtocolWriter::new(&mut output);
        protocol.error(&error).expect("error event");
        assert_eq!(String::from_utf8_lossy(&output).lines().count(), 1);
        let _ = Cursor::new("");
    }

    #[test]
    fn early_diagnostic_scrubbing_removes_short_values_from_the_complete_line() {
        let secret = "lucy";
        let mut diagnostics = Vec::new();
        write_diagnostic_safe_with_environment(
            &mut diagnostics,
            secret,
            None,
            vec![secret.to_owned()],
        );

        let diagnostics = String::from_utf8(diagnostics).expect("diagnostic UTF-8");
        assert!(!diagnostics.contains(secret));
    }
}
