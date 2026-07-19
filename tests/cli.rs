use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const MOCK_SERVER_TIMEOUT: Duration = Duration::from_secs(30);
const MOCK_SERVER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const LUCY_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_TERMINATION_TIMEOUT: Duration = Duration::from_secs(1);
const DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MIN_SOCKET_TIMEOUT: Duration = Duration::from_millis(1);

struct MockServer {
    base_url: String,
    requests: Option<JoinHandle<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
}

impl MockServer {
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock listener");
        listener
            .set_nonblocking(true)
            .expect("mock listener nonblocking");
        let address = listener.local_addr().expect("mock address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let requests = thread::spawn(move || {
            let deadline = Instant::now() + MOCK_SERVER_TIMEOUT;
            let mut request_bodies = Vec::new();
            for response in responses {
                let Some(stream) = accept_with_deadline(&listener, deadline, &server_shutdown)
                else {
                    break;
                };
                let (mut stream, body) = read_request(stream, deadline);
                request_bodies.push(body);
                if !write_response(&mut stream, &response, deadline) {
                    break;
                }
            }
            request_bodies
        });
        Self {
            base_url: format!("http://{address}/v1"),
            requests: Some(requests),
            shutdown,
        }
    }

    fn join(mut self) -> Vec<String> {
        let requests = self.requests.take().expect("mock server handle");
        let deadline = Instant::now() + MOCK_SERVER_JOIN_TIMEOUT;
        while !requests.is_finished() {
            if Instant::now() >= deadline {
                self.shutdown.store(true, Ordering::Release);
                panic!("mock server thread did not shut down before the deadline");
            }
            thread::sleep(DEADLINE_POLL_INTERVAL);
        }
        requests.join().expect("mock server")
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn accept_with_deadline(
    listener: &TcpListener,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> Option<TcpStream> {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return None;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("mock connection blocking");
                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                sleep_until_deadline(deadline, "accept");
            }
            Err(error) => panic!("mock server accept failed: {error}"),
        }
    }
}

fn sleep_until_deadline(deadline: Instant, operation: &str) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        panic!("mock server {operation} timed out before the deadline");
    }
    thread::sleep(DEADLINE_POLL_INTERVAL.min(remaining));
}

fn deadline_timeout(deadline: Instant, operation: &str) -> Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        panic!("mock server {operation} timed out before the deadline");
    }
    remaining.max(MIN_SOCKET_TIMEOUT)
}

fn set_read_deadline(stream: &TcpStream, deadline: Instant, operation: &str) {
    stream
        .set_read_timeout(Some(deadline_timeout(deadline, operation)))
        .unwrap_or_else(|error| panic!("mock server {operation} deadline failed: {error}"));
}

fn set_write_deadline(
    stream: &TcpStream,
    deadline: Instant,
    operation: &str,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(deadline_timeout(deadline, operation)))
}

fn read_request(stream: TcpStream, deadline: Instant) -> (TcpStream, String) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone request stream"));
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        set_read_deadline(reader.get_ref(), deadline, "request header");
        let bytes_read = reader
            .read_line(&mut line)
            .unwrap_or_else(|error| panic!("mock server request header read failed: {error}"));
        if bytes_read == 0 {
            panic!("mock server request header ended at EOF before the terminator");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers.push(line);
    }
    let content_length = headers
        .iter()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse::<usize>().ok())
        })
        .flatten()
        .expect("content length");
    let mut body = vec![0_u8; content_length];
    set_read_deadline(reader.get_ref(), deadline, "request body");
    reader
        .read_exact(&mut body)
        .unwrap_or_else(|error| panic!("mock server request body read failed: {error}"));
    (stream, String::from_utf8(body).expect("UTF-8 request body"))
}

fn response_step(result: std::io::Result<()>, operation: &str) -> bool {
    match result {
        Ok(()) => true,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::InvalidInput
                    | std::io::ErrorKind::NotConnected
            ) =>
        {
            false
        }
        Err(error) => panic!("mock server {operation} failed: {error}"),
    }
}

fn write_response(stream: &mut TcpStream, body: &str, deadline: Instant) -> bool {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if !response_step(
        set_write_deadline(stream, deadline, "response header"),
        "response header deadline",
    ) || !response_step(stream.write_all(header.as_bytes()), "response header write")
    {
        return false;
    }
    for chunk in body.as_bytes().chunks(7) {
        if !response_step(
            set_write_deadline(stream, deadline, "response body"),
            "response body deadline",
        ) || !response_step(stream.write_all(chunk), "response body write")
            || !response_step(
                set_write_deadline(stream, deadline, "response flush"),
                "response flush deadline",
            )
            || !response_step(stream.flush(), "response flush")
        {
            return false;
        }
        thread::sleep(
            Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    true
}

fn normal_response(text: &str) -> String {
    let chunk = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    });
    format!("data: {chunk}\n\n: keep-alive\n\ndata: [DONE]\n\n")
}

fn tool_response_with_arguments(arguments: String, call_id: &str) -> String {
    tool_response_with_finish_reason(arguments, call_id, "tool_calls")
}

fn tool_response_with_reasoning_details(arguments: String, call_id: &str) -> String {
    let reasoning_first = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "text": "private reasoning one"
                }]
            },
            "finish_reason": null
        }]
    });
    let reasoning_and_tool = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "text": "private reasoning two"
                }],
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": "cmd", "arguments": arguments}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {reasoning_first}\n\ndata: {reasoning_and_tool}\n\ndata: [DONE]\n\n")
}

fn tool_response_with_finish_reason(
    arguments: String,
    call_id: &str,
    finish_reason: &str,
) -> String {
    let tool = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": "cmd", "arguments": arguments}
                }]
            },
            "finish_reason": finish_reason
        }]
    });
    format!("data: {tool}\n\ndata: [DONE]\n\n")
}

fn tool_response_with_calls(count: usize) -> String {
    let calls = (0..count)
        .map(|index| {
            json!({
                "index": index,
                "id": format!("call-{index}"),
                "type": "function",
                "function": {
                    "name": "cmd",
                    "arguments": json!({"command": "true"}).to_string()
                }
            })
        })
        .collect::<Vec<_>>();
    let tool = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": calls},
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {tool}\n\ndata: [DONE]\n\n")
}

fn unicode_escape(text: &str) -> String {
    text.chars()
        .map(|character| format!(r#"\u{:04x}"#, character as u32))
        .collect()
}

fn unicode_escaped_tool_response(secret: &str) -> String {
    let escaped = unicode_escape(secret);
    let arguments = format!(r#"{{"{escaped}":"{escaped}","command":"printf '{escaped}'"}}"#);
    tool_response_with_arguments(arguments, "call-unicode")
}

fn response_without_done(text: &str) -> String {
    let chunk = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    });
    format!("data: {chunk}\n\n")
}

fn unknown_tool_response() -> String {
    let chunk = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call-unknown",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

fn provider_environment_tool_response() -> String {
    let command_arguments = json!({
        "command": "printf \"$LUCY_API_KEY\"; printf \"$LUCY_TEST_VALUE\" >&2"
    })
    .to_string();
    let tool = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call-environment",
                    "type": "function",
                    "function": {
                        "name": "cmd",
                        "arguments": command_arguments
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {tool}\n\ndata: [DONE]\n\n")
}

fn tool_response() -> String {
    let text = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": "checking "}, "finish_reason": null}]
    });
    let command_arguments = json!({
        "command": "printf \"$LUCY_TEST_KEY\""
    })
    .to_string();
    let tool_first = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "cmd",
                            "arguments": command_arguments
                        }
                    }
                ]
            },
            "finish_reason": null
        }]
    });
    let tool_fragment = json!({
        "id": "provider-id",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{"index": 0, "function": {"arguments": ""}}]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!(
        "data: {text}\n\n: keep-alive\n\ndata: {tool_first}\n\ndata: {tool_fragment}\n\n data: [DONE]\n\n"
    )
    .replace(" data:", "data:")
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temporary_tree(name: &str) -> (PathBuf, PathBuf) {
    let home = loop {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lucy-{name}-{stamp}-{}-{counter}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("temp tree: {error}"),
        }
    };
    let project = home.join("project");
    fs::create_dir_all(&project).expect("project");
    let status = Command::new("git")
        .args(["-C"])
        .arg(&project)
        .args(["init", "-q"])
        .status()
        .expect("git init");
    assert!(status.success());
    (home, project)
}

fn write_config(home: &Path, base_url: &str, prompt: &str, model: &str) {
    write_config_with_api_key_env(home, base_url, prompt, model, "LUCY_API_KEY");
}

fn write_config_with_api_key_env(
    home: &Path,
    base_url: &str,
    prompt: &str,
    model: &str,
    api_key_env: &str,
) {
    fs::create_dir_all(home.join(".config/lucy")).expect("Lucy config directory");
    let escaped_prompt = prompt.replace('"', "\\\"");
    fs::write(
        home.join(".config/lucy/config.toml"),
        format!(
            "system_prompt = \"{escaped_prompt}\"\n\n[llm]\nbase_url = \"{base_url}\"\nmodel = \"{model}\"\napi_key_env = \"{api_key_env}\"\n"
        ),
    )
    .expect("config");
}

fn write_config_with_effort(home: &Path, base_url: &str, prompt: &str, model: &str, effort: &str) {
    fs::create_dir_all(home.join(".config/lucy")).expect("Lucy config directory");
    let escaped_prompt = prompt.replace('"', "\\\"");
    fs::write(
        home.join(".config/lucy/config.toml"),
        format!(
            "system_prompt = \"{escaped_prompt}\"\n\n[llm]\nbase_url = \"{base_url}\"\nmodel = \"{model}\"\napi_key_env = \"LUCY_API_KEY\"\neffort = \"{effort}\"\n"
        ),
    )
    .expect("config");
}

fn run_lucy(home: &Path, cwd: &Path, args: &[&str], input: &str) -> std::process::Output {
    run_lucy_with_key(home, cwd, args, input, "provider-secret")
}

fn run_lucy_with_key(
    home: &Path,
    cwd: &Path,
    args: &[&str],
    input: &str,
    api_key: &str,
) -> std::process::Output {
    run_lucy_with_key_env(home, cwd, args, input, "LUCY_API_KEY", api_key)
}

fn run_lucy_with_key_env(
    home: &Path,
    cwd: &Path,
    args: &[&str],
    input: &str,
    api_key_env: &str,
    api_key: &str,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lucy"));
    command
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env("LUCY_API_KEY", api_key)
        .env(api_key_env, api_key)
        .env("LUCY_TEST_KEY", api_key)
        .env("LUCY_TEST_VALUE", "ordinary-value")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("Lucy process");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("input");
    wait_for_lucy(child)
}

fn terminate_lucy(child: &mut std::process::Child) {
    let _ = child.kill();
    let deadline = Instant::now() + CHILD_TERMINATION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return;
                }
                thread::sleep(DEADLINE_POLL_INTERVAL.min(remaining));
            }
        }
    }
}

fn wait_for_lucy(mut child: std::process::Child) -> std::process::Output {
    let deadline = Instant::now() + LUCY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().expect("Lucy output"),
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    terminate_lucy(&mut child);
                    panic!("Lucy process exceeded the {LUCY_TIMEOUT:?} execution deadline");
                }
                thread::sleep(DEADLINE_POLL_INTERVAL.min(remaining));
            }
            Err(error) => {
                terminate_lucy(&mut child);
                panic!("Lucy process wait failed: {error}");
            }
        }
    }
}

fn parse_lines(output: &[u8]) -> Vec<Value> {
    String::from_utf8(output.to_vec())
        .expect("UTF-8 stdout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL output"))
        .collect()
}

#[test]
fn cmd_removes_provider_key_but_inherits_other_environment_values() {
    let server = MockServer::start(vec![
        provider_environment_tool_response(),
        normal_response("finished"),
    ]);
    let (home, project) = temporary_tree("provider-environment");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"inspect the environment\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);

    let protocol = String::from_utf8_lossy(&output.stdout);
    assert!(!protocol.contains("provider-secret"));
    let records = parse_lines(&output.stdout);
    let tool_result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("tool result event");
    assert_eq!(tool_result["result"]["stdout"], "");
    assert_eq!(tool_result["result"]["stderr"], "ordinary-value");
    assert!(!serde_json::to_string(tool_result)
        .expect("tool result JSON")
        .contains("provider-secret"));

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].contains("provider-secret"));
    assert!(requests[1].contains("ordinary-value"));

    let session_file = fs::read_dir(home.join(".lucy/sessions"))
        .expect("sessions")
        .next()
        .expect("session entry")
        .expect("session file")
        .path();
    let session = fs::read_to_string(session_file).expect("session contents");
    assert!(!session.contains("provider-secret"));
    assert!(session.contains("ordinary-value"));

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn streams_normalized_events_runs_cmd_loop_and_keeps_stdout_pure() {
    let server = MockServer::start(vec![tool_response(), normal_response("finished")]);
    let (home, project) = temporary_tree("tool-loop");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"inspect the environment\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);

    let records = parse_lines(&output.stdout);
    assert_eq!(records[0]["type"], "session");
    assert!(records
        .iter()
        .any(|record| record["type"] == "assistant_delta"));
    let tool_call = records
        .iter()
        .find(|record| record["type"] == "tool_call")
        .expect("tool call event");
    assert_eq!(tool_call["name"], "cmd");
    let tool_arguments = tool_call["arguments"].as_str().expect("arguments");
    let tool_arguments: Value = serde_json::from_str(tool_arguments).expect("tool arguments");
    assert_eq!(tool_arguments["command"], "printf \"$LUCY_TEST_KEY\"");
    let tool_result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("tool result event");
    assert_eq!(
        tool_result["result"]["command"],
        "printf \"$LUCY_TEST_KEY\""
    );
    assert_eq!(tool_result["result"]["exit_code"], 0);
    assert_eq!(tool_result["result"]["timed_out"], false);
    assert_eq!(tool_result["result"]["stdout"], "[REDACTED]");
    assert_eq!(tool_result["result"]["stderr"], "");
    assert_eq!(tool_result["result"]["stdout_truncated"], false);
    assert_eq!(tool_result["result"]["stderr_truncated"], false);
    assert!(records.iter().any(|record| record["type"] == "turn_end"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("choices"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("provider-secret"));

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("\"stream\":true"));
    assert!(requests[1].contains("\"role\":\"tool\""));
    assert!(!requests
        .iter()
        .any(|request| request.contains("provider-secret")));

    let session_file = fs::read_dir(home.join(".lucy/sessions"))
        .expect("sessions")
        .next()
        .expect("session entry")
        .expect("session file")
        .path();
    let session_bytes = fs::read_to_string(session_file).expect("session contents");
    assert!(!session_bytes.contains("provider-secret"));
    assert!(session_bytes.contains("base prompt"));

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn reasoning_details_survive_tool_follow_up_and_session_resume_without_public_output() {
    let server = MockServer::start(vec![
        tool_response_with_reasoning_details(
            json!({"command": "true"}).to_string(),
            "call-reasoning",
        ),
        normal_response("finished"),
        normal_response("resumed"),
    ]);
    let (home, project) = temporary_tree("reasoning-details");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let first = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"inspect\"}\n",
    );
    assert!(first.status.success(), "stderr: {:?}", first.stderr);
    assert!(first.stderr.is_empty(), "stderr: {:?}", first.stderr);
    let first_output = String::from_utf8_lossy(&first.stdout);
    assert!(!first_output.contains("reasoning_details"));
    assert!(!first_output.contains("private reasoning"));
    let session_id = parse_lines(&first.stdout)[0]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let resumed = run_lucy(
        &home,
        &project,
        &["--session", &session_id],
        "{\"type\":\"message\",\"text\":\"resume\"}\n",
    );
    assert!(resumed.status.success(), "stderr: {:?}", resumed.stderr);
    assert!(resumed.stderr.is_empty(), "stderr: {:?}", resumed.stderr);
    let resumed_output = String::from_utf8_lossy(&resumed.stdout);
    assert!(!resumed_output.contains("reasoning_details"));
    assert!(!resumed_output.contains("private reasoning"));

    let requests = server.join();
    assert_eq!(requests.len(), 3);
    let expected_details = json!([
        {"type": "reasoning.text", "text": "private reasoning one"},
        {"type": "reasoning.text", "text": "private reasoning two"}
    ]);
    for request_body in [&requests[1], &requests[2]] {
        let request: Value = serde_json::from_str(request_body).expect("provider request JSON");
        let assistant = request["messages"]
            .as_array()
            .expect("provider messages")
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("assistant tool-call message");
        assert_eq!(assistant["reasoning_details"], expected_details);
    }

    let session_file = fs::read_dir(home.join(".lucy/sessions"))
        .expect("sessions")
        .next()
        .expect("session entry")
        .expect("session file")
        .path();
    let session_bytes = fs::read_to_string(session_file).expect("session contents");
    assert!(session_bytes.contains("reasoning_details"));
    assert!(session_bytes.contains("private reasoning one"));
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn collision_safe_markers_redact_streams_and_tool_results() {
    for (index, api_key) in ["REDACTED"].into_iter().enumerate() {
        let server = MockServer::start(vec![tool_response(), normal_response("finished")]);
        let (home, project) = temporary_tree(&format!("redaction-collision-{index}"));
        write_config(&home, &server.base_url, "base prompt", "mock-model");

        let output = run_lucy_with_key(
            &home,
            &project,
            &[],
            "{\"type\":\"message\",\"text\":\"inspect\"}\n",
            api_key,
        );
        assert!(output.status.success(), "stderr: {:?}", output.stderr);
        let records = parse_lines(&output.stdout);
        let tool_result = records
            .iter()
            .find(|record| record["type"] == "tool_result")
            .expect("tool result event");
        let stdout = tool_result["result"]["stdout"].as_str().expect("stdout");
        assert!(!stdout.contains(api_key));
        assert_eq!(stdout.chars().count(), 1);
        assert!(!String::from_utf8_lossy(&output.stdout).contains(api_key));
        assert!(records.iter().any(|record| record["type"] == "turn_end"));

        let requests = server.join();
        assert_eq!(requests.len(), 2);
        assert!(!requests.iter().any(|request| request.contains(api_key)));
        fs::remove_dir_all(home).expect("cleanup");
    }
}

#[test]
fn fixed_literal_key_collisions_are_rejected_before_session_output() {
    for (index, api_key) in [
        "session",
        "tool",
        "cmd",
        "command",
        "0",
        "[REDACTED]",
        "type\":\"session",
    ]
    .into_iter()
    .enumerate()
    {
        let (home, project) = temporary_tree(&format!("fixed-literal-{index}"));
        write_config(&home, "http://127.0.0.1:1/v1", "base prompt", "mock-model");

        let output = run_lucy_with_key(
            &home,
            &project,
            &[],
            "{\"type\":\"message\",\"text\":\"hello\"}\n",
            api_key,
        );
        assert!(
            !output.status.success(),
            "key should be rejected: {api_key}"
        );
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("structured output"), "stderr: {stderr}");
        assert!(!stderr.contains(api_key), "stderr: {stderr}");
        assert!(!home.join(".lucy/sessions").exists());
        fs::remove_dir_all(home).expect("cleanup");
    }
}

#[test]
fn list_omits_existing_sessions_when_the_current_key_collides_with_literals() {
    let server = MockServer::start(vec![normal_response("finished")]);
    let (home, project) = temporary_tree("list-collision");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let created = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(created.status.success(), "stderr: {:?}", created.stderr);
    assert_eq!(server.join().len(), 1);

    let listed = run_lucy_with_key(&home, &project, &["--list-sessions"], "", "session");
    assert!(listed.status.success(), "stderr: {:?}", listed.stderr);
    assert!(listed.stderr.is_empty());
    assert!(listed.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn git_context_discovery_removes_the_configured_provider_environment() {
    let server = MockServer::start(vec![normal_response("finished")]);
    let (home, project) = temporary_tree("git-provider-environment");
    let nested = project.join("nested");
    fs::create_dir(&nested).expect("nested cwd");
    fs::write(project.join("AGENTS.md"), "root instructions").expect("root instructions");
    write_config_with_api_key_env(
        &home,
        &server.base_url,
        "base prompt",
        "mock-model",
        "GIT_DIR",
    );

    let output = run_lucy_with_key_env(
        &home,
        &nested,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
        "GIT_DIR",
        "provider-secret",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("root instructions"));

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn new_session_rejects_key_bearing_cwd_and_provider_metadata() {
    let secret = "provider-secret";
    let cases = [
        (
            "cwd",
            "http://127.0.0.1:1/v1",
            "mock-model",
            "LUCY_API_KEY",
            true,
        ),
        (
            "base_url",
            "http://provider-secret.invalid/v1",
            "mock-model",
            "LUCY_API_KEY",
            false,
        ),
        (
            "model",
            "http://127.0.0.1:1/v1",
            "provider-secret",
            "LUCY_API_KEY",
            false,
        ),
        (
            "api_key_env",
            "http://127.0.0.1:1/v1",
            "mock-model",
            "LUCY_provider-secret_ENV",
            false,
        ),
    ];

    for (index, (field, base_url, model, api_key_env, key_in_cwd)) in cases.into_iter().enumerate()
    {
        let tree_name = if key_in_cwd {
            format!("key-bearing-cwd-{secret}")
        } else {
            format!("header-metadata-{index}")
        };
        let (home, project) = temporary_tree(&tree_name);
        write_config_with_api_key_env(&home, base_url, "base prompt", model, api_key_env);

        let output = run_lucy_with_key_env(
            &home,
            &project,
            &[],
            "{\"type\":\"message\",\"text\":\"hello\"}\n",
            api_key_env,
            secret,
        );
        assert!(
            !output.status.success(),
            "field should be rejected: {field}"
        );
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("session header"), "stderr: {stderr}");
        assert!(!stderr.contains(secret), "stderr: {stderr}");
        let session_files = home.join(".lucy/sessions");
        if let Ok(entries) = fs::read_dir(session_files) {
            assert_eq!(entries.count(), 0);
        }
        fs::remove_dir_all(home).expect("cleanup");
    }
}

#[test]
fn missing_provider_key_diagnostic_is_generic_and_does_not_echo_environment_name() {
    let (home, project) = temporary_tree("missing-provider-key");
    let api_key_env = format!("LUCY_MISSING_PROVIDER_KEY_{}", std::process::id());
    write_config_with_api_key_env(
        &home,
        "http://127.0.0.1:1/v1",
        "base prompt",
        "mock-model",
        &api_key_env,
    );

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "!: missing provider API key\n"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&api_key_env));
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn unsafe_session_is_rejected_on_resume_and_omitted_from_list() {
    let (home, project) = temporary_tree("unsafe-session");
    let sessions = home.join(".lucy/sessions");
    fs::create_dir_all(&sessions).expect("sessions");
    let id = "unsafe-session";
    let header = json!({
        "record": "session",
        "version": 1,
        "session_id": id,
        "created_at": 1,
        "cwd": project.display().to_string(),
        "boot_system_prompt": "provider-secret",
        "llm": {
            "base_url": "http://127.0.0.1:1/v1",
            "model": "mock-model",
            "api_key_env": "LUCY_API_KEY"
        }
    });
    fs::write(sessions.join(format!("{id}.jsonl")), format!("{header}\n")).expect("unsafe session");

    let resumed = run_lucy(&home, &project, &["--session", id], "");
    assert!(!resumed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&resumed.stderr),
        "!: session header rejected\n"
    );
    assert!(resumed.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&resumed.stderr).contains("provider-secret"));

    let listed = run_lucy(&home, &project, &["--list-sessions"], "");
    assert!(listed.status.success(), "stderr: {:?}", listed.stderr);
    assert!(listed.stderr.is_empty());
    assert!(listed.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn resumed_sessions_reject_inner_escaped_tool_secrets() {
    let secret = "provider-secret";
    let (home, project) = temporary_tree("escaped-session-secret");
    let sessions = home.join(".lucy/sessions");
    fs::create_dir_all(&sessions).expect("sessions");
    let id = "escaped-session";
    let escaped = unicode_escape(secret);
    let arguments = format!(r#"{{"command":"{escaped}"}}"#);
    let header = json!({
        "record": "session",
        "version": 1,
        "session_id": id,
        "created_at": 1,
        "cwd": project.display().to_string(),
        "boot_system_prompt": "safe",
        "llm": {
            "base_url": "http://127.0.0.1:1/v1",
            "model": "mock-model",
            "api_key_env": "LUCY_API_KEY"
        }
    });
    let message = json!({
        "record": "message",
        "timestamp": 2,
        "message": {
            "role": "assistant",
            "tool_calls": [{
                "id": "call-escaped",
                "name": "cmd",
                "arguments": arguments
            }]
        }
    });
    fs::write(
        sessions.join(format!("{id}.jsonl")),
        format!("{header}\n{message}\n"),
    )
    .expect("escaped session");

    let resumed = run_lucy(&home, &project, &["--session", id], "");
    assert!(!resumed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&resumed.stderr),
        "!: session header rejected\n"
    );
    assert!(resumed.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn resumed_sessions_reject_duplicate_fields_before_last_key_wins_discards_escaped_secrets() {
    let secret = "provider-secret";
    let (home, project) = temporary_tree("duplicate-session-secret");
    let sessions = home.join(".lucy/sessions");
    fs::create_dir_all(&sessions).expect("sessions");
    let id = "duplicate-session";
    let header = json!({
        "record": "session",
        "version": 1,
        "session_id": id,
        "created_at": 1,
        "cwd": project.display().to_string(),
        "boot_system_prompt": "safe",
        "llm": {
            "base_url": "http://127.0.0.1:1/v1",
            "model": "mock-model",
            "api_key_env": "LUCY_API_KEY"
        }
    });
    let escaped = unicode_escape(secret);
    let message = format!(
        r#"{{"record":"message","timestamp":2,"message":{{"role":"user","content":"{escaped}","content":"safe"}}}}"#
    );
    fs::write(
        sessions.join(format!("{id}.jsonl")),
        format!("{header}\n{message}\n"),
    )
    .expect("duplicate session");

    let resumed = run_lucy(&home, &project, &["--session", id], "");
    assert!(!resumed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&resumed.stderr),
        "!: invalid session record at line 2\n"
    );
    assert!(resumed.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&resumed.stderr).contains(secret));

    let listed = run_lucy(&home, &project, &["--list-sessions"], "");
    assert!(listed.status.success(), "stderr: {:?}", listed.stderr);
    assert!(listed.stderr.is_empty());
    assert!(listed.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn resumed_sessions_reject_secrets_in_unknown_nested_fields() {
    let secret = "provider-secret";
    let (home, project) = temporary_tree("unknown-session-secret");
    let sessions = home.join(".lucy/sessions");
    fs::create_dir_all(&sessions).expect("sessions");
    let id = "unknown-session";
    let escaped = unicode_escape(secret);
    let header = json!({
        "record": "session",
        "version": 1,
        "session_id": id,
        "created_at": 1,
        "cwd": project.display().to_string(),
        "boot_system_prompt": "safe",
        "llm": {
            "base_url": "http://127.0.0.1:1/v1",
            "model": "mock-model",
            "api_key_env": "LUCY_API_KEY"
        }
    });
    let message = format!(
        r#"{{"record":"message","timestamp":2,"message":{{"role":"user","content":"safe"}},"unknown":{{"nested":"{escaped}"}}}}"#
    );
    fs::write(
        sessions.join(format!("{id}.jsonl")),
        format!("{header}\n{message}\n"),
    )
    .expect("unknown session field");

    let resumed = run_lucy(&home, &project, &["--session", id], "");
    assert!(!resumed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&resumed.stderr),
        "!: session header rejected\n"
    );
    assert!(resumed.stdout.is_empty());

    let listed = run_lucy(&home, &project, &["--list-sessions"], "");
    assert!(listed.status.success(), "stderr: {:?}", listed.stderr);
    assert!(listed.stderr.is_empty());
    assert!(listed.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn incomplete_tool_finish_reasons_do_not_execute_or_continue() {
    for (index, reason) in ["length", "content_filter"].into_iter().enumerate() {
        let marker = format!("incomplete-finish-marker-{index}");
        let response = tool_response_with_finish_reason(
            json!({"command": format!("touch {marker}")}).to_string(),
            "call-incomplete",
            reason,
        );
        let server = MockServer::start(vec![response]);
        let (home, project) = temporary_tree(&format!("incomplete-finish-{index}"));
        write_config(&home, &server.base_url, "base prompt", "mock-model");

        let output = run_lucy(
            &home,
            &project,
            &[],
            "{\"type\":\"message\",\"text\":\"run\"}\n",
        );
        assert!(output.status.success(), "stderr: {:?}", output.stderr);
        let records = parse_lines(&output.stdout);
        assert!(records.iter().any(|record| record["type"] == "error"));
        assert!(!records.iter().any(|record| record["type"] == "tool_call"));
        assert!(!records.iter().any(|record| record["type"] == "tool_result"));
        assert!(!records.iter().any(|record| record["type"] == "turn_end"));
        assert!(!project.join(&marker).exists());
        assert_eq!(server.join().len(), 1);
        fs::remove_dir_all(home).expect("cleanup");
    }
}

#[test]
fn tool_loop_has_no_round_limit() {
    let mut responses = (0..33)
        .map(|index| {
            tool_response_with_arguments(
                json!({"command": "true"}).to_string(),
                &format!("call-{index}"),
            )
        })
        .collect::<Vec<_>>();
    responses.push(normal_response("finished"));
    let server = MockServer::start(responses);
    let (home, project) = temporary_tree("tool-loop-unbounded");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"loop\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    assert!(!records.iter().any(|record| record["type"] == "error"));
    assert!(records.iter().any(|record| record["type"] == "turn_end"));

    let requests = server.join();
    assert_eq!(requests.len(), 34);
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn provider_and_message_tool_calls_have_no_count_limit() {
    let server = MockServer::start(vec![
        tool_response_with_calls(65),
        normal_response("finished"),
    ]);
    let (home, project) = temporary_tree("tool-call-unbounded");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"loop\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    assert!(!records.iter().any(|record| record["type"] == "error"));
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "tool_call")
            .count(),
        65
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "tool_result")
            .count(),
        65
    );
    assert!(records.iter().any(|record| record["type"] == "turn_end"));

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn malformed_tool_arguments_use_valid_json_and_return_a_safe_error_result() {
    let (home, project) = temporary_tree("malformed-tool-arguments");
    let marker = project.join("raw-malformed-command-ran");
    let malformed_arguments = format!("touch {}", marker.display());
    let server = MockServer::start(vec![
        tool_response_with_arguments(malformed_arguments, "call-malformed"),
        normal_response("finished"),
    ]);
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"run\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    let tool_call = records
        .iter()
        .find(|record| record["type"] == "tool_call")
        .expect("tool call event");
    let arguments = tool_call["arguments"].as_str().expect("arguments");
    assert_eq!(
        serde_json::from_str::<Value>(arguments).expect("JSON arguments"),
        json!({})
    );

    let tool_result = records
        .iter()
        .find(|record| record["type"] == "tool_result")
        .expect("tool result event");
    assert_eq!(
        tool_result["result"]["error"],
        "cmd arguments must be a JSON object"
    );
    assert!(!marker.exists(), "raw malformed command was executed");

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    let follow_up: Value = serde_json::from_str(&requests[1]).expect("provider request JSON");
    let assistant_arguments = follow_up["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant message")["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("assistant arguments");
    assert_eq!(assistant_arguments, "{}");

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn structured_tool_arguments_redact_decoded_secret_in_protocol_and_session() {
    let secret = "provider-secret";
    let server = MockServer::start(vec![
        unicode_escaped_tool_response(secret),
        normal_response("finished"),
    ]);
    let (home, project) = temporary_tree("structured-redaction");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"inspect\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    let tool_call = records
        .iter()
        .find(|record| record["type"] == "tool_call")
        .expect("tool call event");
    let arguments = tool_call["arguments"].as_str().expect("arguments");
    let parsed_arguments: Value = serde_json::from_str(arguments).expect("JSON arguments");
    assert!(!serde_json::to_string(&parsed_arguments)
        .expect("serialized arguments")
        .contains(secret));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));

    let session_file = fs::read_dir(home.join(".lucy/sessions"))
        .expect("sessions")
        .next()
        .expect("session entry")
        .expect("session file")
        .path();
    for line in fs::read_to_string(session_file)
        .expect("session contents")
        .lines()
    {
        let record: Value = serde_json::from_str(line).expect("session JSONL");
        assert!(!serde_json::to_string(&record)
            .expect("serialized session record")
            .contains(secret));
    }

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert!(!requests.iter().any(|request| request.contains(secret)));
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn unknown_tool_is_rejected_without_a_public_tool_event_or_followup() {
    let server = MockServer::start(vec![unknown_tool_response()]);
    let (home, project) = temporary_tree("unknown-tool");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"use another tool\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    assert!(records.iter().any(|record| record["type"] == "error"));
    assert!(!records.iter().any(|record| record["type"] == "tool_call"));
    assert!(!records.iter().any(|record| record["type"] == "tool_result"));
    assert!(!records.iter().any(|record| record["type"] == "turn_end"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("read_file"));

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].contains("read_file"));

    let session_file = fs::read_dir(home.join(".lucy/sessions"))
        .expect("sessions")
        .next()
        .expect("session entry")
        .expect("session file")
        .path();
    let session_bytes = fs::read_to_string(session_file).expect("session contents");
    assert!(!session_bytes.contains("read_file"));
    fs::remove_dir_all(home).expect("cleanup");
}

fn assert_provider_error_without_turn_end(response: String, name: &str) {
    let server = MockServer::start(vec![response]);
    let (home, project) = temporary_tree(name);
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"provider response\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    assert!(records.iter().any(|record| record["type"] == "error"));
    assert!(!records.iter().any(|record| record["type"] == "turn_end"));

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn streaming_redaction_handles_json_escape_collisions() {
    let secret = "n0";
    let server = MockServer::start(vec![normal_response("\n0")]);
    let (home, project) = temporary_tree("streaming-json-escape");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy_with_key(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
        secret,
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    let session_file = fs::read_dir(home.join(".lucy/sessions"))
        .expect("sessions")
        .next()
        .expect("session entry")
        .expect("session file")
        .path();
    assert!(!fs::read_to_string(session_file)
        .expect("session contents")
        .contains(secret));
    assert_eq!(server.join().len(), 1);
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn provider_error_does_not_leak_the_active_key() {
    let secret = "provider-secret";
    let response = format!("data: {}\n\n", json!({"error": {"message": secret}}));
    let server = MockServer::start(vec![response]);
    let (home, project) = temporary_tree("provider-error-redaction");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"provider response\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(parse_lines(&output.stdout)
        .iter()
        .any(|record| record["type"] == "error"));
    assert_eq!(server.join().len(), 1);
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn malformed_input_is_redacted_even_after_provider_startup() {
    let (home, project) = temporary_tree("malformed-redaction");
    write_config(&home, "http://127.0.0.1:1/v1", "base prompt", "mock-model");

    let output = run_lucy_with_key(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":}\n",
        "input",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("input"));
    assert!(parse_lines(&output.stdout)
        .iter()
        .any(|record| record["type"] == "error"));
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn empty_non_sse_malformed_and_incomplete_successes_are_errors() {
    assert_provider_error_without_turn_end(String::new(), "empty-provider");
    assert_provider_error_without_turn_end(
        "data: [DONE]\n\n".to_owned(),
        "done-without-payload-provider",
    );
    assert_provider_error_without_turn_end("not an SSE response\n".to_owned(), "non-sse-provider");
    assert_provider_error_without_turn_end("data: not-json\n\n".to_owned(), "malformed-provider");
    assert_provider_error_without_turn_end(response_without_done("partial"), "incomplete-provider");
}

#[test]
fn malformed_config_diagnostic_does_not_echo_source_or_api_key() {
    let (home, project) = temporary_tree("malformed-config-provider-secret");
    fs::create_dir_all(home.join(".config/lucy")).expect("Lucy config directory");
    fs::write(
        home.join(".config/lucy/config.toml"),
        "system_prompt = \"provider-secret\n[llm]\nmodel = [\n",
    )
    .expect("malformed config");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid TOML"), "stderr: {stderr}");
    assert!(!stderr.contains("provider-secret"), "stderr: {stderr}");
    assert!(
        !stderr.contains(&home.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("system_prompt"), "stderr: {stderr}");
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn version_prints_without_bootstrapping_configuration() {
    let (home, project) = temporary_tree("version");
    let output = run_lucy(&home, &project, &["--version"], "");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!("lucy ", env!("CARGO_PKG_VERSION"), "\n")
    );
    assert!(output.stderr.is_empty());
    assert!(!home.join(".config/lucy/config.toml").exists());

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn forced_tui_fails_clearly_without_terminal_stdio() {
    let (home, project) = temporary_tree("forced-tui-non-terminal");
    let output = run_lucy(&home, &project, &["--tui"], "");
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "!: --tui requires a terminal on stdin and stdout\n"
    );
    assert!(output.stdout.is_empty());
    assert!(!home.join(".lucy").exists());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn forced_jsonl_keeps_the_machine_protocol() {
    let server = MockServer::start(vec![normal_response("forced")]);
    let (home, project) = temporary_tree("forced-jsonl");
    write_config(&home, &server.base_url, "base prompt", "mock-model");
    let output = run_lucy(
        &home,
        &project,
        &["--jsonl"],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    assert_eq!(records[0]["type"], "session");
    assert!(records.iter().any(|record| record["type"] == "turn_end"));
    assert_eq!(server.join().len(), 1);
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn list_sessions_bootstraps_config_without_validating_provider_settings() {
    let (home, project) = temporary_tree("list-bootstrap");
    let config_path = home.join(".config/lucy/config.toml");
    assert!(!config_path.exists());

    let output = run_lucy(&home, &project, &["--list-sessions"], "");
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
    assert!(output.stdout.is_empty());
    let config = fs::read_to_string(config_path).expect("bootstrapped config");
    assert!(config.contains("model = \"\""));
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn generated_config_is_not_written_when_it_contains_the_active_key() {
    let (home, project) = temporary_tree("unsafe-generated-config");
    let output = run_lucy_with_key_env(
        &home,
        &project,
        &["--list-sessions"],
        "",
        "OPENROUTER_API_KEY",
        "OPENROUTER_API_KEY",
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("OPENROUTER_API_KEY"));
    assert!(!home.join(".config/lucy/config.toml").exists());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn configured_effort_is_sent_as_reasoning_effort() {
    let server = MockServer::start(vec![normal_response("finished")]);
    let (home, project) = temporary_tree("effort-sent");
    write_config_with_effort(&home, &server.base_url, "base prompt", "mock-model", "high");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_str(&requests[0]).expect("provider request JSON");
    assert_eq!(request["reasoning_effort"], "high");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("provider-secret"));
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn omitted_effort_sends_no_reasoning_effort_field() {
    let server = MockServer::start(vec![normal_response("finished")]);
    let (home, project) = temporary_tree("effort-omitted");
    write_config(&home, &server.base_url, "base prompt", "mock-model");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_str(&requests[0]).expect("provider request JSON");
    assert!(request.get("reasoning_effort").is_none());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn empty_effort_fails_boot_without_echoing_the_key() {
    let (home, project) = temporary_tree("effort-empty");
    write_config_with_effort(
        &home,
        "http://127.0.0.1:1/v1",
        "base prompt",
        "mock-model",
        "",
    );

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("llm.effort must not be empty"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("provider-secret"), "stderr: {stderr}");
    assert!(!home.join(".lucy/sessions").exists());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn effort_containing_the_active_key_is_rejected_as_a_session_header() {
    let (home, project) = temporary_tree("effort-key-collision");
    write_config_with_effort(
        &home,
        "http://127.0.0.1:1/v1",
        "base prompt",
        "mock-model",
        "provider-secret",
    );

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"hello\"}\n",
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session header"), "stderr: {stderr}");
    assert!(!stderr.contains("provider-secret"), "stderr: {stderr}");
    let session_files = home.join(".lucy/sessions");
    if let Ok(entries) = fs::read_dir(session_files) {
        assert_eq!(entries.count(), 0);
    }
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn resume_reloads_model_and_effort_from_config() {
    let server = MockServer::start(vec![normal_response("first"), normal_response("resumed")]);
    let (home, project) = temporary_tree("effort-resume");
    write_config_with_effort(
        &home,
        &server.base_url,
        "original prompt",
        "original-model",
        "high",
    );

    let first = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"first message\"}\n",
    );
    assert!(first.status.success(), "stderr: {:?}", first.stderr);
    let first_records = parse_lines(&first.stdout);
    let session_id = first_records[0]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // Change config to prove resume reloads the current source of truth.
    write_config(&home, &server.base_url, "changed prompt", "changed-model");

    let resumed = run_lucy(
        &home,
        &project,
        &["--session", &session_id],
        "{\"type\":\"message\",\"text\":\"second message\"}\n",
    );
    assert!(resumed.status.success(), "stderr: {:?}", resumed.stderr);

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    let resumed_request: Value = serde_json::from_str(&requests[1]).expect("resumed request JSON");
    assert!(resumed_request.get("reasoning_effort").is_none());
    assert_eq!(resumed_request["model"], "changed-model");

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn resume_rejects_malformed_current_config_without_leaking_secrets() {
    let server = MockServer::start(vec![normal_response("first")]);
    let (home, project) = temporary_tree("resume");
    write_config(&home, &server.base_url, "original prompt", "original-model");
    fs::write(project.join("AGENTS.md"), "original instructions").expect("instructions");

    let first = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"first message\"}\n",
    );
    assert!(first.status.success(), "stderr: {:?}", first.stderr);
    let first_records = parse_lines(&first.stdout);
    let session_id = first_records[0]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    fs::write(
        home.join(".config/lucy/config.toml"),
        "system_prompt = \"malformed provider-secret\n[llm]\nmodel = [\n",
    )
    .expect("malformed changed config");
    fs::write(project.join("AGENTS.md"), "changed instructions").expect("changed instructions");
    let resumed = run_lucy(
        &home,
        &project,
        &["--session", &session_id],
        "{\"type\":\"message\",\"text\":\"second message\"}\n",
    );
    assert!(!resumed.status.success());
    assert!(!String::from_utf8_lossy(&resumed.stderr).contains("provider-secret"));

    let list = run_lucy(&home, &project, &["--list-sessions"], "");
    assert!(list.status.success(), "stderr: {:?}", list.stderr);
    assert!(list.stderr.is_empty(), "stderr: {:?}", list.stderr);
    let metadata = parse_lines(&list.stdout);
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0]["type"], "session_metadata");
    assert_eq!(metadata[0]["session_id"], session_id);
    assert!(metadata[0]["first_message"]
        .as_str()
        .expect("first summary")
        .contains("first message"));

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("original prompt"));
    assert!(requests[0].contains("original instructions"));
    assert!(requests[0].contains("original-model"));

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn malformed_session_diagnostic_does_not_echo_path_or_id() {
    let (home, project) = temporary_tree("malformed-session-provider-secret");
    let sessions = home.join(".lucy/sessions");
    fs::create_dir_all(&sessions).expect("sessions");
    let id = "provider-secret-session";
    let session_path = sessions.join(format!("{id}.jsonl"));
    fs::write(&session_path, "not valid JSON\n").expect("malformed session");

    let output = run_lucy(&home, &project, &["--session", id], "");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("!: invalid session record at line "),
        "stderr: {stderr}"
    );
    assert!(stderr.ends_with('\n'));
    assert!(!stderr.contains("provider-secret"));
    assert!(!stderr.contains(&session_path.display().to_string()));
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn missing_resume_is_a_stderr_failure_without_protocol_leak() {
    let (home, project) = temporary_tree("missing-resume");
    let output = run_lucy(&home, &project, &["--session", "provider-secret"], "");
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "!: session not found\n"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("provider-secret"));
    assert!(output.stdout.is_empty());
    assert!(!home.join(".config/lucy/config.toml").exists());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn unknown_cli_argument_diagnostic_does_not_echo_value_before_bootstrap() {
    let (home, project) = temporary_tree("unknown-argument");
    let output = run_lucy(&home, &project, &["--unknown=provider-secret"], "");
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "!: unknown argument\n"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("provider-secret"));
    assert!(output.stdout.is_empty());
    assert!(!home.join(".config/lucy/config.toml").exists());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn early_diagnostics_redact_environment_keys_before_config_loading() {
    for (index, secret) in ["unknown", "config.toml", "n0"].into_iter().enumerate() {
        let (home, project) = temporary_tree(&format!("early-diagnostic-{index}"));
        let output = run_lucy_with_key(&home, &project, &["--unknown"], "", secret);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
        assert!(!home.join(".config/lucy/config.toml").exists());
        fs::remove_dir_all(home).expect("cleanup");
    }
}

#[test]
fn skill_commands_discover_recursively_and_inject_a_snapshot_with_arguments() {
    let server = MockServer::start(vec![normal_response("skill complete")]);
    let (home, project) = temporary_tree("skill-command");
    write_config(&home, &server.base_url, "base prompt", "mock-model");
    let skill = project.join(".agents/skills/writing/release-notes/SKILL.md");
    fs::create_dir_all(skill.parent().expect("skill parent")).expect("skill directories");
    fs::write(
        &skill,
        "---\nname: release-notes\ndescription: Write concise release notes.\n---\n# Release Notes\nUse the release template.\n",
    )
    .expect("skill");

    let output = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"/release-notes v1.2.0\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let requests = server.join();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_str(&requests[0]).expect("provider request");
    let messages = request["messages"].as_array().expect("messages");
    assert!(messages[0]["content"]
        .as_str()
        .expect("system prompt")
        .contains("<name>release-notes</name>"));
    let skill_message = messages.last().expect("skill message")["content"]
        .as_str()
        .expect("skill contents");
    assert!(skill_message.contains("# Release Notes"));
    assert!(skill_message.contains("User: v1.2.0"));

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn resumed_skill_commands_use_the_immutable_discovered_snapshot() {
    let server = MockServer::start(vec![normal_response("first"), normal_response("second")]);
    let (home, project) = temporary_tree("skill-resume");
    write_config(&home, &server.base_url, "base prompt", "mock-model");
    let skill = project.join(".agents/skills/release-notes/SKILL.md");
    fs::create_dir_all(skill.parent().expect("skill parent")).expect("skill directories");
    fs::write(
        &skill,
        "---\nname: release-notes\ndescription: Write release notes.\n---\noriginal instructions\n",
    )
    .expect("original skill");

    let first = run_lucy(
        &home,
        &project,
        &[],
        "{\"type\":\"message\",\"text\":\"start session\"}\n",
    );
    assert!(first.status.success(), "stderr: {:?}", first.stderr);
    let session_id = parse_lines(&first.stdout)[0]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    fs::write(
        &skill,
        "---\nname: release-notes\ndescription: Write release notes.\n---\nchanged instructions\n",
    )
    .expect("changed skill");

    let resumed = run_lucy(
        &home,
        &project,
        &["--session", &session_id],
        "{\"type\":\"message\",\"text\":\"/release-notes\"}\n",
    );
    assert!(resumed.status.success(), "stderr: {:?}", resumed.stderr);
    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("original instructions"));
    assert!(!requests[1].contains("changed instructions"));

    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn spawn_subagent_queues_immediately_and_automatically_delivers_completion() {
    let arguments = json!({"task": "inspect only this task"}).to_string();
    let tool = json!({"id":"provider-id","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"delegate-1","type":"function","function":{"name":"spawn_subagent","arguments":arguments}}]},"finish_reason":"tool_calls"}]});
    let response = format!("data: {tool}\n\ndata: [DONE]\n\n");
    let server = MockServer::start(vec![
        response,
        normal_response("parent continued without waiting"),
        normal_response("worker result"),
        normal_response("completion acknowledged"),
    ]);
    let (home, project) = temporary_tree("queued-subagent");
    write_config_with_effort(
        &home,
        &server.base_url,
        "base prompt",
        "parent-model",
        "high",
    );
    let output = run_lucy(
        &home,
        &project,
        &["--jsonl"],
        "{\"type\":\"message\",\"text\":\"delegate this\"}\n",
    );
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    let records = parse_lines(&output.stdout);
    let queued = records
        .iter()
        .find(|record| record["type"] == "tool_result" && record["name"] == "spawn_subagent")
        .expect("queued result");
    assert_eq!(queued["result"]["status"], "queued");
    assert!(queued["result"]["task_id"]
        .as_str()
        .unwrap_or_default()
        .starts_with("subagent-"));
    assert!(records
        .iter()
        .any(|record| record["type"] == "assistant_delta"
            && record["text"] == "completion acknowledged"));

    let requests = server.join();
    assert_eq!(requests.len(), 4);
    let worker: Value = requests
        .iter()
        .map(|request| serde_json::from_str(request).expect("request JSON"))
        .find(|request: &Value| {
            request["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["content"] == "inspect only this task")
            })
        })
        .expect("worker request");
    assert_eq!(worker["model"], "parent-model");
    assert_eq!(worker["reasoning_effort"], "high");
    assert_eq!(
        worker["messages"]
            .as_array()
            .expect("worker messages")
            .len(),
        2
    );
    assert_eq!(worker["messages"][1]["content"], "inspect only this task");
    assert!(worker["tools"]
        .as_array()
        .expect("worker tools")
        .iter()
        .all(|tool| tool["function"]["name"] != "spawn_subagent"));
    let lifecycle = records
        .iter()
        .filter(|record| {
            matches!(
                record["type"].as_str(),
                Some("background_result_pending" | "background_result_delivered")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle.len(), 2);
    assert_eq!(lifecycle[0]["type"], "background_result_pending");
    assert_eq!(lifecycle[1]["type"], "background_result_delivered");
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "turn_end")
            .count(),
        1
    );
    let resumed_parent = requests
        .iter()
        .map(|request| serde_json::from_str::<Value>(request).expect("request JSON"))
        .find(|request| {
            request["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "tool" && message["name"] == "background_result"
                })
            })
        })
        .expect("same-turn resumed parent request");
    let messages = resumed_parent["messages"].as_array().expect("messages");
    let synthetic = messages
        .iter()
        .position(|message| {
            message["role"] == "assistant"
                && message["tool_calls"][0]["function"]["name"] == "background_result"
        })
        .expect("synthetic assistant call");
    assert_eq!(messages[synthetic + 1]["role"], "tool");
    assert_eq!(messages[synthetic + 1]["name"], "background_result");
    assert!(messages.iter().all(|message| {
        message["role"] != "user"
            || !message["content"]
                .as_str()
                .is_some_and(|text| text.contains("worker result"))
    }));
    assert!(resumed_parent["tools"]
        .as_array()
        .expect("model tools")
        .iter()
        .all(|tool| tool["function"]["name"] != "background_result"));
    fs::remove_dir_all(home).expect("cleanup");
}
