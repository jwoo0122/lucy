use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cancellation::CancellationToken;

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const COMMAND_OUTPUT_CAP: usize = 64 * 1024;
const CAPTURE_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CmdResult {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub canceled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CmdResult {
    fn error(arguments: &str, message: impl Into<String>) -> Self {
        Self {
            command: arguments.to_owned(),
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            canceled: false,
            error: Some(message.into()),
        }
    }

    pub(crate) fn canceled(command: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            canceled: true,
            error: Some(message.into()),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

pub fn execute(arguments: &str, cwd: &Path, api_key_env: &str, secret: Option<&str>) -> CmdResult {
    execute_with_cancellation(arguments, cwd, api_key_env, secret, None)
}

pub(crate) fn execute_with_cancellation(
    arguments: &str,
    cwd: &Path,
    api_key_env: &str,
    secret: Option<&str>,
    cancellation: Option<&CancellationToken>,
) -> CmdResult {
    let value: Value = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => return CmdResult::error("{}", "cmd arguments must be a JSON object"),
    };
    let Some(object) = value.as_object() else {
        return CmdResult::error("{}", "cmd arguments must be a JSON object");
    };
    if object.len() != 1 || !object.contains_key("command") {
        return CmdResult::error("{}", "cmd arguments must contain only command");
    }
    let Some(command) = object.get("command").and_then(Value::as_str) else {
        return CmdResult::error("{}", "cmd command must be a string");
    };
    if cancellation.is_some_and(|token| token.is_cancelled()) {
        return CmdResult::canceled(
            redact_secret(command, secret),
            "command canceled before execution",
        );
    }
    execute_command_with_cancellation(
        command,
        cwd,
        api_key_env,
        secret,
        COMMAND_TIMEOUT,
        COMMAND_OUTPUT_CAP,
        cancellation,
    )
}

pub fn execute_command(
    command: &str,
    cwd: &Path,
    api_key_env: &str,
    secret: Option<&str>,
    timeout: Duration,
    output_cap: usize,
) -> CmdResult {
    execute_command_with_cancellation(command, cwd, api_key_env, secret, timeout, output_cap, None)
}

pub(crate) fn execute_command_with_cancellation(
    command: &str,
    cwd: &Path,
    api_key_env: &str,
    secret: Option<&str>,
    timeout: Duration,
    output_cap: usize,
    cancellation: Option<&CancellationToken>,
) -> CmdResult {
    if cancellation.is_some_and(|token| token.is_cancelled()) {
        return CmdResult::canceled(
            redact_secret(command, secret),
            "command canceled before execution",
        );
    }

    let mut process = Command::new("/bin/sh");
    process
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .env_remove(api_key_env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Each command gets its own process group so a timed-out shell and its
        // finite descendants can be cleaned up together.
        unsafe {
            process.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(_) => {
            return CmdResult {
                command: redact_secret(command, secret),
                exit_code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                canceled: false,
                error: Some("unable to start command".to_owned()),
            }
        }
    };

    let capture_stop = Arc::new(AtomicBool::new(false));
    let stdout_reader = child
        .stdout
        .take()
        .map(|stdout| spawn_capture(stdout, output_cap, Arc::clone(&capture_stop)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|stderr| spawn_capture(stderr, output_cap, Arc::clone(&capture_stop)));

    let child_id = child.id();
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut canceled = false;
    let status = loop {
        if cancellation.is_some_and(|token| token.is_cancelled()) {
            canceled = true;
            kill_process_group(child_id);
            let _ = child.kill();
            break child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if cancellation.is_some_and(|token| token.is_cancelled()) {
                    canceled = true;
                }
                break Some(status);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_process_group(child_id);
                    let _ = child.kill();
                    break child.wait().ok();
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                canceled = cancellation.is_some_and(|token| token.is_cancelled());
                kill_process_group(child_id);
                let _ = child.kill();
                break child.wait().ok();
            }
        }
    };

    if !timed_out {
        // A shell can finish while a background child remains in its process
        // group. Lucy has no background-process API, so clean that group too.
        kill_process_group(child_id);
    }

    capture_stop.store(true, Ordering::Release);
    let stdout_capture = join_capture(stdout_reader);
    let stderr_capture = join_capture(stderr_reader);
    let (stdout, stdout_truncated) = bounded_output(&stdout_capture, output_cap, secret);
    let (stderr, stderr_truncated) = bounded_output(&stderr_capture, output_cap, secret);

    CmdResult {
        command: redact_secret(command, secret),
        exit_code: (!canceled)
            .then(|| status.and_then(|status| status.code()))
            .flatten(),
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        canceled,
        error: canceled.then_some("command canceled".to_owned()),
    }
}

#[cfg(unix)]
fn spawn_capture<R>(mut reader: R, cap: usize, stop: Arc<AtomicBool>) -> JoinHandle<CapturedOutput>
where
    R: Read + Send + AsRawFd + 'static,
{
    let _ = set_nonblocking(reader.as_raw_fd());
    thread::spawn(move || capture_output(&mut reader, cap, &stop))
}

#[cfg(not(unix))]
fn spawn_capture<R>(mut reader: R, cap: usize, stop: Arc<AtomicBool>) -> JoinHandle<CapturedOutput>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || capture_output(&mut reader, cap, &stop))
}

fn capture_output<R>(reader: &mut R, cap: usize, stop: &AtomicBool) -> CapturedOutput
where
    R: Read,
{
    let mut bytes = Vec::with_capacity(cap.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    let mut shutdown_incomplete = false;
    let mut shutdown_deadline = None;
    loop {
        if stop.load(Ordering::Acquire) {
            shutdown_deadline.get_or_insert_with(|| Instant::now() + CAPTURE_SHUTDOWN_GRACE);
            if shutdown_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                shutdown_incomplete = true;
                break;
            }
        }

        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let remaining = cap.saturating_sub(bytes.len());
                if remaining > 0 {
                    bytes.extend_from_slice(&buffer[..read.min(remaining)]);
                }
                if read > remaining {
                    truncated = true;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(_) => break,
        }
    }
    CapturedOutput {
        bytes,
        truncated: truncated || shutdown_incomplete,
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: std::os::fd::RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn bounded_output(
    captured: &CapturedOutput,
    output_cap: usize,
    secret: Option<&str>,
) -> (String, bool) {
    let text = redact_secret(&String::from_utf8_lossy(&captured.bytes), secret);
    let truncated =
        captured.truncated || captured.bytes.len() > output_cap || text.len() > output_cap;
    let mut end = text.len().min(output_cap);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), truncated)
}

fn join_capture(reader: Option<JoinHandle<CapturedOutput>>) -> CapturedOutput {
    let Some(reader) = reader else {
        return CapturedOutput {
            bytes: Vec::new(),
            truncated: false,
        };
    };

    let deadline = Instant::now() + CAPTURE_SHUTDOWN_GRACE;
    while !reader.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    if reader.is_finished() {
        reader.join().unwrap_or(CapturedOutput {
            bytes: Vec::new(),
            truncated: false,
        })
    } else {
        // Non-blocking capture normally exits after the shutdown grace. If a
        // platform refuses that setup, detach rather than blocking the command
        // harness on a descendant-owned pipe forever.
        CapturedOutput {
            bytes: Vec::new(),
            truncated: true,
        }
    }
}

fn kill_process_group(child_id: u32) {
    #[cfg(unix)]
    {
        // The child is the process-group leader created above. Ignore errors:
        // it may already have exited, and child.wait below remains authoritative.
        unsafe {
            let _ = libc::kill(-(child_id as libc::pid_t), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = child_id;
}

pub fn redact_secret(text: &str, secret: Option<&str>) -> String {
    crate::redaction::redact_secret(text, secret)
}

pub(crate) fn canceled_result(arguments: &str, secret: &str) -> CmdResult {
    let command = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .filter(|object| object.len() == 1)
                .and_then(|object| object.get("command"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(|command| redact_secret(&command, Some(secret)))
        .unwrap_or_else(|| "{}".to_owned());
    CmdResult::canceled(command, "command canceled before execution")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory() -> std::path::PathBuf {
        loop {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lucy-command-{stamp}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temp directory: {error}"),
            }
        }
    }

    #[test]
    fn captures_nonzero_exit_and_both_streams() {
        let cwd = temporary_directory();
        let result = execute_command(
            "printf out; printf err >&2; exit 7",
            &cwd,
            "LUCY_API_KEY",
            None,
            Duration::from_secs(2),
            COMMAND_OUTPUT_CAP,
        );
        assert_eq!(result.exit_code, Some(7));
        assert!(!result.timed_out);
        assert_eq!(result.stdout, "out");
        assert_eq!(result.stderr, "err");
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[test]
    fn caps_streams_independently_and_marks_truncation() {
        let cwd = temporary_directory();
        let result = execute_command(
            "printf 123456789; printf abcdefghij >&2",
            &cwd,
            "LUCY_API_KEY",
            None,
            Duration::from_secs(2),
            4,
        );
        assert_eq!(result.stdout, "1234");
        assert_eq!(result.stderr, "abcd");
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[cfg(unix)]
    #[test]
    fn bounds_lossy_invalid_utf8_output_and_marks_truncation() {
        let cwd = temporary_directory();
        let result = execute_command(
            r"printf '\377\376\375\374'; printf '\377\376\375\374' >&2",
            &cwd,
            "LUCY_API_KEY",
            None,
            Duration::from_secs(2),
            4,
        );
        assert!(result.stdout.len() <= 4);
        assert!(result.stderr.len() <= 4);
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[test]
    fn timeout_kills_the_command_group() {
        let cwd = temporary_directory();
        let result = execute_command(
            "sleep 30",
            &cwd,
            "LUCY_API_KEY",
            None,
            Duration::from_millis(80),
            COMMAND_OUTPUT_CAP,
        );
        assert!(result.timed_out);
        assert!(result.exit_code.is_none() || result.exit_code != Some(0));
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[test]
    fn cancellation_kills_a_running_command_group() {
        let cwd = temporary_directory();
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let worker_cwd = cwd.clone();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            execute_command_with_cancellation(
                "sleep 30",
                &worker_cwd,
                "LUCY_API_KEY",
                None,
                Duration::from_secs(30),
                COMMAND_OUTPUT_CAP,
                Some(&worker_token),
            )
        });
        thread::sleep(Duration::from_millis(80));
        token.cancel();
        let result = worker.join().expect("command worker");
        assert!(result.canceled);
        assert_eq!(result.error.as_deref(), Some("command canceled"));
        assert!(started.elapsed() < Duration::from_secs(2));
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_capture_returns_when_a_descendant_escapes_the_process_group() {
        let python_available = Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !python_available {
            return;
        }

        let cwd = temporary_directory();
        let started = Instant::now();
        let result = execute_command(
            "python3 -c 'import os,time; os.setsid(); open(\"ready\",\"w\").close(); time.sleep(1)' & while [ ! -f ready ]; do sleep 0.01; done; sleep 2",
            &cwd,
            "LUCY_API_KEY",
            None,
            Duration::from_millis(300),
            COMMAND_OUTPUT_CAP,
        );
        assert!(result.timed_out);
        assert!(result.stdout_truncated);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "capture cleanup exceeded the bounded grace period: {:?}",
            started.elapsed()
        );
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[test]
    fn output_redacts_the_provider_key() {
        let cwd = temporary_directory();
        let result = execute_command(
            "printf secret-key",
            &cwd,
            "LUCY_API_KEY",
            Some("secret-key"),
            Duration::from_secs(2),
            COMMAND_OUTPUT_CAP,
        );
        assert!(!result.stdout.contains("secret-key"));
        assert_eq!(result.stdout, "[REDACTED]");
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[test]
    fn redaction_stays_within_the_capture_byte_bound() {
        let cwd = temporary_directory();
        let result = execute_command(
            "printf x",
            &cwd,
            "LUCY_API_KEY",
            Some("x"),
            Duration::from_secs(2),
            1,
        );
        assert_eq!(result.stdout.len(), 1);
        assert!(!result.stdout.contains('x'));
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[test]
    fn collision_markers_do_not_reintroduce_the_provider_key() {
        let cwd = temporary_directory();
        for secret in ["REDACTED", "[REDACTED]"] {
            let command = format!("printf '{secret}'");
            let result = execute_command(
                &command,
                &cwd,
                "LUCY_API_KEY",
                Some(secret),
                Duration::from_secs(2),
                COMMAND_OUTPUT_CAP,
            );
            assert!(!result.stdout.contains(secret));
            assert!(!result.command.contains(secret));
        }
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }

    #[test]
    fn rejects_extra_command_arguments() {
        let cwd = temporary_directory();
        let result = execute(
            r#"{"command":"pwd","extra":true}"#,
            &cwd,
            "LUCY_API_KEY",
            None,
        );
        assert_eq!(
            result.error.as_deref(),
            Some("cmd arguments must contain only command")
        );
        fs::remove_dir_all(cwd).expect("remove temp directory");
    }
}
