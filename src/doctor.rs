use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::json;

use crate::auth::AuthStore;
use crate::command::{execute_command, COMMAND_OUTPUT_CAP, COMMAND_TIMEOUT};
use crate::config::{config_dir, config_path, AuthProvider, Config, CODEX_API_KEY_ENV_SENTINEL};
use crate::model::ChatMessage;
use crate::protocol::{PROTOCOL_CAPABILITIES, PROTOCOL_VERSION};
use crate::provider::{DiagnosticFailureKind, Provider};
use crate::redaction::redact_secret;
use crate::session::sessions_dir;

static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);
const REPORT_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Warning,
    Fail,
    Skipped,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub status: Status,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub version: u8,
    pub ok: bool,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn exit_code(&self) -> i32 {
        if self.ok {
            0
        } else {
            1
        }
    }

    pub fn write_human(&self, output: &mut impl Write) -> io::Result<()> {
        for check in &self.checks {
            let label = match check.status {
                Status::Pass => "PASS",
                Status::Warning => "WARN",
                Status::Fail => "FAIL",
                Status::Skipped => "SKIP",
            };
            writeln!(output, "[{label}] {}: {}", check.id, check.message)?;
            if let Some(remediation) = check.remediation {
                writeln!(output, "  remediation: {remediation}")?;
            }
        }
        Ok(())
    }
}

pub fn run(
    home: &Path,
    cwd: &Path,
    live: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    query_terminal: bool,
) -> Report {
    let mut checks = Vec::new();
    let path = config_path(home);
    checks.push(
        pass("config.path", "active configuration path resolved").details(json!({"path": path})),
    );
    let directory_safe = inspect_path(
        "config.directory",
        &config_dir(home),
        true,
        0o077,
        &mut checks,
    );
    let file_safe = inspect_path("config.file", &path, false, 0o077, &mut checks);
    check_legacy(home, &path, &mut checks);

    let config = if directory_safe && file_safe {
        read_config(&path, &mut checks)
    } else {
        checks.push(fail(
            "config.toml",
            "configuration was not read because its protected path is unsafe",
            "Repair configuration ownership, permissions, and path types.",
        ));
        None
    };
    check_session_storage(home, &mut checks);
    check_shell_base(cwd, &mut checks);
    check_terminal(stdin_is_tty, stdout_is_tty, query_terminal, &mut checks);
    checks.push(
        pass("protocol.v1", "public JSONL protocol is available").details(json!({
            "version": PROTOCOL_VERSION, "capabilities": PROTOCOL_CAPABILITIES
        })),
    );

    let secret = config
        .as_ref()
        .and_then(|config| diagnostic_secret(home, config));
    if let Some(config) = config {
        check_provider(home, cwd, &config, live, &mut checks);
        if !checks.iter().any(|check| check.id == "provider.metadata") {
            checks.push(skipped(
                "provider.metadata",
                "provider metadata requires valid authentication and settings",
            ));
        }
        if live && !checks.iter().any(|check| check.id == "provider.live") {
            checks.push(skipped(
                "provider.live",
                "live probe requires all provider prerequisites",
            ));
        }
    } else {
        checks.push(skipped(
            "provider.auth",
            "provider checks require valid configuration",
        ));
        checks.push(skipped(
            "provider.metadata",
            "provider checks require valid configuration",
        ));
        if live {
            checks.push(skipped(
                "provider.live",
                "live probe requires valid configuration",
            ));
        }
    }
    if let Some(secret) = secret.as_deref() {
        scrub_checks(&mut checks, secret);
    }
    Report {
        version: REPORT_VERSION,
        ok: !checks.iter().any(|c| c.status == Status::Fail),
        checks,
    }
}

fn diagnostic_secret(home: &Path, config: &Config) -> Option<String> {
    let auth = config.resolved_auth().ok()?;
    match auth.provider {
        AuthProvider::Openrouter => std::env::var(auth.api_key_env?)
            .ok()
            .filter(|value| !value.is_empty()),
        AuthProvider::CodexSubscription => AuthStore::for_home(home)
            .load()
            .ok()
            .flatten()
            .map(|credentials| credentials.access),
    }
}

fn scrub_checks(checks: &mut [Check], secret: &str) {
    fn scrub_value(value: &mut serde_json::Value, secret: &str) {
        match value {
            serde_json::Value::String(text) => *text = redact_secret(text, Some(secret)),
            serde_json::Value::Array(values) => values
                .iter_mut()
                .for_each(|value| scrub_value(value, secret)),
            serde_json::Value::Object(values) => values
                .values_mut()
                .for_each(|value| scrub_value(value, secret)),
            _ => {}
        }
    }
    for check in checks {
        check.message = redact_secret(&check.message, Some(secret));
        if let Some(details) = &mut check.details {
            scrub_value(details, secret);
        }
    }
}

fn inspect_path(
    id: &'static str,
    path: &Path,
    directory: bool,
    unsafe_bits: u32,
    checks: &mut Vec<Check>,
) -> bool {
    let result: Result<(), (&str, &str)> = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err((
            "protected path is a symlink",
            "Replace the symlink with a private regular path.",
        )),
        Ok(meta) if directory != meta.is_dir() || (!directory && !meta.is_file()) => Err((
            "protected path has an unsafe file type",
            "Replace it with the expected regular path type.",
        )),
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if meta.uid() != unsafe { libc::geteuid() } {
                    checks.push(fail(
                        id,
                        "protected path is not owned by the current user",
                        "Change ownership to the current user.",
                    ));
                    return false;
                }
                if meta.permissions().mode() & unsafe_bits != 0 {
                    checks.push(fail(
                        id,
                        "protected path permissions are not private",
                        if directory {
                            "Set directory permissions to 0700."
                        } else {
                            "Set file permissions to 0600."
                        },
                    ));
                    return false;
                }
            }
            checks.push(pass(
                id,
                "protected path type, ownership, and permissions are valid",
            ));
            return true;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err((
            "protected path does not exist",
            "Run `lucy setup` in a terminal.",
        )),
        Err(_) => Err((
            "protected path cannot be inspected",
            "Check path ownership and permissions.",
        )),
    };
    let (message, remediation) = result.expect_err("non-success path inspection");
    checks.push(fail(id, message, remediation));
    false
}

fn open_config_no_follow(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration is not a regular file",
        ));
    }
    Ok(file)
}

fn read_config(path: &Path, checks: &mut Vec<Check>) -> Option<Config> {
    if !fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    {
        checks.push(fail(
            "config.toml",
            "configuration cannot be read from an unsafe path type",
            "Replace config.toml with a private regular file.",
        ));
        return None;
    }
    let source = match open_config_no_follow(path).and_then(|mut file| {
        let mut source = String::new();
        file.read_to_string(&mut source)?;
        Ok(source)
    }) {
        Ok(source) => source,
        Err(_) => {
            checks.push(fail(
                "config.toml",
                "configuration cannot be read",
                "Run `lucy setup` or repair config.toml.",
            ));
            return None;
        }
    };
    match toml::from_str(&source) {
        Ok(config) => {
            checks.push(pass("config.toml", "configuration TOML is valid"));
            Some(config)
        }
        Err(_) => {
            checks.push(fail(
                "config.toml",
                "configuration TOML is malformed",
                "Repair config.toml, then rerun `lucy doctor`.",
            ));
            None
        }
    }
}

fn check_legacy(home: &Path, active: &Path, checks: &mut Vec<Check>) {
    let legacy = home.join(".lucy/config.toml");
    let message = if active.exists() && legacy.exists() {
        "active and legacy configuration files both exist; active XDG configuration wins"
    } else if legacy.exists() {
        "legacy configuration exists and has not been migrated"
    } else {
        "no pending legacy configuration migration detected"
    };
    let status = if legacy.exists() && !active.exists() {
        Status::Warning
    } else {
        Status::Pass
    };
    checks.push(check("config.legacy", status, message, None));
}

fn check_session_storage(home: &Path, checks: &mut Vec<Check>) {
    let lucy = home.join(".lucy");
    let directory = sessions_dir(home);
    for path in [&lucy, &directory] {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                checks.push(fail(
                    "session.storage",
                    "session storage cannot be inspected",
                    "Check ~/.lucy ownership and permissions.",
                ));
                return;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            checks.push(fail(
                "session.storage",
                "session storage contains an unsafe path type",
                "Replace it with private regular directories.",
            ));
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0
            {
                checks.push(fail(
                    "session.storage",
                    "session storage ownership or permissions are unsafe",
                    "Set directory ownership to the current user and permissions to 0700.",
                ));
                return;
            }
        }
    }
    let made_lucy = !lucy.exists();
    let made_sessions = !directory.exists();
    let result = (|| -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&directory)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(&directory)?;
        let probe = directory.join(format!(
            ".doctor-{}-{}.tmp",
            std::process::id(),
            PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let write = options
            .open(&probe)
            .and_then(|mut f| f.write_all(b"doctor").and_then(|_| f.sync_all()));
        let remove = fs::remove_file(&probe);
        write.and(remove)
    })();
    if made_sessions {
        let _ = fs::remove_dir(&directory);
    }
    if made_lucy {
        let _ = fs::remove_dir(&lucy);
    }
    match result {
        Ok(()) => checks.push(pass(
            "session.storage",
            "session directory is safely writable and the temporary probe was removed",
        )),
        Err(_) => checks.push(fail(
            "session.storage",
            "session directory is not safely writable or the temporary probe could not be removed",
            "Check ~/.lucy ownership, path types, and permissions.",
        )),
    }
}

fn check_shell_base(cwd: &Path, checks: &mut Vec<Check>) {
    let shell = std::env::var_os("SHELL")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| "/bin/sh".into());
    if cwd.is_dir() {
        checks.push(pass("shell.cwd", "starting cwd is accessible").details(json!({"path": cwd})));
    } else {
        checks.push(fail(
            "shell.cwd",
            "starting cwd is not accessible",
            "Start Lucy from an accessible directory.",
        ));
    }
    checks.push(
        pass("shell.contract", "command execution bounds are configured").details(json!({
            "shell": shell, "fallback": "/bin/sh", "timeout_seconds": COMMAND_TIMEOUT.as_secs(),
            "stdout_cap_bytes": COMMAND_OUTPUT_CAP, "stderr_cap_bytes": COMMAND_OUTPUT_CAP
        })),
    );
}

fn check_shell_environment(cwd: &Path, api_key_env: &str, checks: &mut Vec<Check>) {
    let command = format!("test \"${{PATH+x}}\" = x && test -z \"${{{api_key_env}+x}}\"");
    let result = execute_command(
        &command,
        cwd,
        api_key_env,
        None,
        COMMAND_TIMEOUT,
        COMMAND_OUTPUT_CAP,
    );
    if result.exit_code == Some(0) {
        checks.push(pass(
            "shell.environment",
            "ordinary environment inheritance works and the provider variable is absent",
        ));
    } else {
        checks.push(fail(
            "shell.environment",
            "command-child environment boundary probe failed",
            "Check the configured shell and shell startup files.",
        ));
    }
}

fn check_terminal(
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    query_terminal: bool,
    checks: &mut Vec<Check>,
) {
    checks.push(
        check(
            "terminal.stdio",
            if stdin_is_tty && stdout_is_tty {
                Status::Pass
            } else {
                Status::Warning
            },
            if stdin_is_tty && stdout_is_tty {
                "stdin and stdout are attached to terminals"
            } else {
                "stdin or stdout is not attached to a terminal"
            },
            None,
        )
        .details(json!({"stdin_tty": stdin_is_tty, "stdout_tty": stdout_is_tty})),
    );
    let caps = crate::tui::diagnostic_capabilities(query_terminal && stdin_is_tty && stdout_is_tty);
    checks.push(
        check(
            "terminal.background",
            if caps.background.is_some() {
                Status::Pass
            } else {
                Status::Warning
            },
            if caps.background.is_some() {
                "terminal background color was detected"
            } else {
                "terminal background color is unavailable"
            },
            None,
        )
        .details(json!({"color": caps.background})),
    );
    checks.push(
        pass(
            "terminal.keyboard",
            "keyboard enhancement support was detected without enabling it",
        )
        .details(json!({"supported": caps.keyboard_enhancement})),
    );
}

fn valid_environment_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn check_provider(home: &Path, cwd: &Path, config: &Config, live: bool, checks: &mut Vec<Check>) {
    let auth = match config.resolved_auth() {
        Ok(v) => v,
        Err(_) => {
            checks.push(fail(
                "provider.auth",
                "authentication configuration is invalid",
                "Run `lucy setup`.",
            ));
            return;
        }
    };
    let settings = match config.resolved_llm() {
        Ok(v) if !v.model.is_empty() => v,
        _ => {
            checks.push(fail(
                "provider.settings",
                "provider endpoint, model, or effort is invalid",
                "Run `lucy setup`.",
            ));
            return;
        }
    };
    if !reqwest::Url::parse(&settings.base_url)
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
    {
        checks.push(fail(
            "provider.endpoint",
            "provider endpoint is not a valid HTTP(S) URL",
            "Correct the endpoint with `lucy setup`.",
        ));
        return;
    }
    checks.push(
        pass(
            "provider.settings",
            "provider endpoint and selected model are valid",
        )
        .details(json!({"model": settings.model, "effort": settings.effort})),
    );

    let secret = match auth.provider {
        AuthProvider::Openrouter => {
            let name = auth.api_key_env.as_deref().unwrap_or_default();
            if !valid_environment_name(name) {
                checks.push(fail(
                    "provider.auth",
                    "API-key environment-variable name is invalid",
                    "Use a shell identifier in config.toml.",
                ));
                return;
            }
            check_shell_environment(cwd, name, checks);
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => {
                    checks.push(
                        pass(
                            "provider.auth",
                            "configured API-key environment variable is present",
                        )
                        .details(json!({"environment": name})),
                    );
                    v
                }
                _ => {
                    checks.push(fail(
                        "provider.auth",
                        "configured API-key environment variable is absent",
                        "Export the configured key and rerun `lucy doctor`.",
                    ));
                    return;
                }
            }
        }
        AuthProvider::CodexSubscription => {
            checks.push(skipped(
                "shell.environment",
                "Codex credentials are not stored in the process environment",
            ));
            match AuthStore::for_home(home).load() {
                Ok(Some(c)) => {
                    checks.push(pass(
                        "provider.auth",
                        "Codex credential store is present and valid",
                    ));
                    c.access
                }
                Ok(None) => {
                    checks.push(fail(
                        "provider.auth",
                        "Codex login is absent",
                        "Run `lucy codex login`.",
                    ));
                    return;
                }
                Err(_) => {
                    checks.push(fail(
                        "provider.auth",
                        "Codex credential store is invalid or unsafe",
                        "Run `lucy codex login` to replace it.",
                    ));
                    return;
                }
            }
        }
    };
    let provider = match if settings.api_key_env == CODEX_API_KEY_ENV_SENTINEL {
        Provider::new_codex(home, &settings)
    } else {
        Provider::new(&settings)
    } {
        Ok(p) => p,
        Err(e) => {
            checks.push(fail(
                "provider.construct",
                redact_secret(&e.to_string(), Some(&secret)),
                "Check authentication and provider settings.",
            ));
            return;
        }
    };
    checks.push(pass(
        "provider.construct",
        "provider initialized without creating a session",
    ));
    match provider.diagnostic_metadata() {
        Ok(metadata) => {
            let selected = metadata.models.iter().find(|m| m.id == settings.model);
            if selected.is_none() {
                checks.push(fail(
                    "provider.metadata",
                    "selected model is absent from the provider catalog",
                    "Select a model returned by the provider.",
                ));
            } else if settings.effort.as_deref().is_some_and(|effort| {
                selected
                    .and_then(|model| model.efforts.as_ref())
                    .is_some_and(|efforts| !efforts.iter().any(|supported| supported == effort))
            }) {
                checks.push(fail(
                    "provider.metadata",
                    "configured reasoning effort is unsupported by the selected model",
                    "Select an effort advertised by the provider.",
                ));
            } else {
                checks.push(pass("provider.metadata", "selected model metadata loaded").details(json!({
                "context_window": metadata.selected_context_window, "supported_efforts": selected.and_then(|m| m.efforts.clone())
            })));
            }
        }
        Err(error) => {
            let (status, message) = match error.kind {
                DiagnosticFailureKind::Unsupported => (
                    Status::Warning,
                    "optional provider model catalog is unsupported",
                ),
                DiagnosticFailureKind::Transport => {
                    (Status::Fail, "provider metadata transport failed")
                }
                DiagnosticFailureKind::Http => {
                    (Status::Fail, "provider metadata returned an HTTP failure")
                }
                DiagnosticFailureKind::Parse => (
                    Status::Fail,
                    "provider metadata response could not be parsed",
                ),
                DiagnosticFailureKind::Login => (Status::Fail, "Codex login is absent or invalid"),
                DiagnosticFailureKind::Refresh => (Status::Fail, "Codex credential refresh failed"),
                DiagnosticFailureKind::Catalog => (Status::Fail, "Codex model catalog failed"),
            };
            checks.push(
                check(
                    "provider.metadata",
                    status,
                    message,
                    Some("Check provider connectivity, authentication, and catalog support."),
                )
                .details(json!({"http_status": error.http_status})),
            );
        }
    }
    if live {
        let messages = [
            ChatMessage::system("Reply with exactly OK. Do not call tools.".to_owned()),
            ChatMessage::user("diagnostic probe".to_owned()),
        ];
        match provider.diagnostic_live_once(&messages) {
            Ok(turn) if turn.tool_calls.is_empty() => checks.push(pass(
                "provider.live",
                "one provider stream completed and accepted Lucy's cmd schema",
            )),
            Ok(_) => checks.push(pass(
                "provider.live",
                "one provider stream completed, returned a cmd call, and Lucy did not execute it",
            )),
            Err(error) => checks.push(fail(
                "provider.live",
                redact_secret(&error.to_string(), Some(&secret)),
                "Check provider connectivity, model access, and tool compatibility.",
            )),
        }
    }
}

trait WithDetails {
    fn details(self, details: serde_json::Value) -> Self;
}
impl WithDetails for Check {
    fn details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}
fn check(
    id: &'static str,
    status: Status,
    message: impl Into<String>,
    remediation: Option<&'static str>,
) -> Check {
    Check {
        id,
        status,
        message: message.into(),
        remediation,
        details: None,
    }
}
fn pass(id: &'static str, message: impl Into<String>) -> Check {
    check(id, Status::Pass, message, None)
}
fn fail(id: &'static str, message: impl Into<String>, remediation: &'static str) -> Check {
    check(id, Status::Fail, message, Some(remediation))
}
fn skipped(id: &'static str, message: impl Into<String>) -> Check {
    check(id, Status::Skipped, message, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_report_has_stable_statuses_and_no_terminal_sequences() {
        let report = Report {
            version: 1,
            ok: true,
            checks: vec![pass("example", "safe")],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains('\u{1b}'));
        assert!(json.contains("\"status\":\"pass\""));
    }
    #[test]
    fn environment_names_are_shell_identifiers() {
        assert!(valid_environment_name("OPENAI_API_KEY"));
        assert!(!valid_environment_name("BAD-NAME"));
        assert!(!valid_environment_name("1BAD"));
    }
}
