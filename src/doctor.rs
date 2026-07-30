use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::auth::AuthStore;
use crate::command::{execute, COMMAND_OUTPUT_CAP, COMMAND_TIMEOUT};
use crate::config::{config_dir, config_path, AuthProvider, Config, CODEX_API_KEY_ENV_SENTINEL};
use crate::model::ChatMessage;
use crate::protocol::{PROTOCOL_CAPABILITIES, PROTOCOL_VERSION};
use crate::provider::Provider;
use crate::redaction::redact_secret;
use crate::session::sessions_dir;

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
}

pub fn run(home: &Path, cwd: &Path, live: bool) -> Report {
    let mut checks = Vec::new();
    let path = config_path(home);
    checks.push(pass(
        "config.path",
        format!("active configuration path: {}", path.display()),
    ));
    check_config_directory(home, &mut checks);
    check_config_path(&path, &mut checks);

    let config = match fs::read_to_string(&path)
        .ok()
        .and_then(|source| toml::from_str::<Config>(&source).ok())
    {
        Some(config) => {
            checks.push(pass("config.toml", "configuration TOML is valid"));
            Some(config)
        }
        None => {
            checks.push(fail(
                "config.toml",
                "unable to parse config.toml: invalid or unreadable TOML",
                "Run `lucy setup` to create or repair configuration.",
            ));
            None
        }
    };

    check_session_storage(home, &mut checks);
    check_shell(cwd, None, &mut checks);
    checks.push(
        pass(
            "protocol.v1",
            format!("JSONL protocol version {PROTOCOL_VERSION} is implemented"),
        )
        .details(
            serde_json::json!({"version": PROTOCOL_VERSION, "capabilities": PROTOCOL_CAPABILITIES}),
        ),
    );
    checks.push(pass(
        "terminal.stdio",
        format!(
            "stdin terminal: {}; stdout terminal: {}",
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal()
        ),
    ));

    if let Some(config) = config {
        check_provider(home, &config, live, &mut checks);
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

    Report {
        ok: !checks.iter().any(|check| check.status == Status::Fail),
        checks,
        version: 1,
    }
}

fn check_config_directory(home: &Path, checks: &mut Vec<Check>) {
    let directory = config_dir(home);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            checks.push(fail(
                "config.directory",
                "configuration directory is not a regular non-symlink directory",
                "Replace it with a private directory.",
            ))
        }
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    checks.push(fail(
                        "config.directory",
                        "configuration directory permissions are not private",
                        "Set the Lucy configuration directory permissions to 0700.",
                    ));
                    return;
                }
            }
            checks.push(pass(
                "config.directory",
                "configuration directory is private and non-symlinked",
            ));
        }
        Err(_) => checks.push(fail(
            "config.directory",
            "configuration directory does not exist or cannot be inspected",
            "Run `lucy setup` in a terminal.",
        )),
    }
}

fn check_config_path(path: &Path, checks: &mut Vec<Check>) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            checks.push(fail(
                "config.storage",
                "configuration target is not a regular non-symlink file",
                "Replace it with a private regular config.toml file.",
            ))
        }
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    checks.push(fail(
                        "config.storage",
                        "configuration file permissions are not private",
                        "Set config.toml permissions to 0600.",
                    ));
                    return;
                }
            }
            checks.push(pass(
                "config.storage",
                "configuration target is a private regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => checks.push(fail(
            "config.storage",
            "configuration file does not exist",
            "Run `lucy setup` in a terminal.",
        )),
        Err(_) => checks.push(fail(
            "config.storage",
            "unable to inspect configuration target",
            "Check the configuration path and its parent permissions.",
        )),
    }
}

fn check_session_storage(home: &Path, checks: &mut Vec<Check>) {
    let directory = sessions_dir(home);
    if [home.join(".lucy"), directory.clone()].iter().any(|path| {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    }) {
        checks.push(fail(
            "session.storage",
            "session storage path contains a symlink",
            "Replace it with private regular directories.",
        ));
        return;
    }
    if let Err(error) = fs::create_dir_all(&directory) {
        checks.push(fail(
            "session.storage",
            format!("session directory is not writable: {error}"),
            "Check ~/.lucy permissions.",
        ));
        return;
    }
    let probe = directory.join(format!(".doctor-{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options
        .open(&probe)
        .and_then(|mut file| file.write_all(b"doctor").and_then(|_| file.sync_all()))
    {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            checks.push(pass(
                "session.storage",
                format!(
                    "session directory is safely writable: {}",
                    directory.display()
                ),
            ));
        }
        Err(_) => checks.push(fail(
            "session.storage",
            "session directory is not safely writable",
            "Check ~/.lucy permissions and remove unsafe path types.",
        )),
    }
}

fn check_shell(cwd: &Path, api_key_env: Option<&str>, checks: &mut Vec<Check>) {
    let shell = std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    if !cwd.is_dir() {
        checks.push(fail(
            "shell.cwd",
            "starting cwd is not a directory",
            "Start Lucy from an accessible directory.",
        ));
    } else {
        checks.push(pass(
            "shell.cwd",
            format!("starting cwd is accessible: {}", cwd.display()),
        ));
    }
    checks.push(pass(
        "shell.contract",
        format!(
            "commands use {} -lc with closed stdin, {}s timeout, and {} byte output caps",
            shell.display(),
            COMMAND_TIMEOUT.as_secs(),
            COMMAND_OUTPUT_CAP
        ),
    ));
    if let Some(api_key_env) = api_key_env {
        std::env::set_var("LUCY_DOCTOR_INHERITANCE_PROBE", "present");
        let command = format!(
            "test \"$LUCY_DOCTOR_INHERITANCE_PROBE\" = present && test -z \"${{{api_key_env}}}\""
        );
        let result = execute(&command, cwd, api_key_env, None);
        std::env::remove_var("LUCY_DOCTOR_INHERITANCE_PROBE");
        if result.exit_code == Some(0) {
            checks.push(pass("shell.environment", "ordinary environment inheritance works and the active provider credential is removed"));
        } else {
            checks.push(warning(
                "shell.environment",
                "direct credential removal could not be proven after shell startup",
                "Check shell startup files; they may independently restore provider credentials.",
            ));
        }
    }
}

fn check_provider(home: &Path, config: &Config, live: bool, checks: &mut Vec<Check>) {
    let auth = match config.resolved_auth() {
        Ok(auth) => auth,
        Err(error) => {
            checks.push(fail(
                "provider.auth",
                error.to_string(),
                "Run `lucy setup` to select one authentication mode.",
            ));
            return;
        }
    };
    let settings = match config.resolved_llm() {
        Ok(settings) if !settings.model.is_empty() => settings,
        Ok(_) => {
            checks.push(fail(
                "provider.settings",
                "no model is selected",
                "Run `lucy setup` and select a model.",
            ));
            return;
        }
        Err(error) => {
            checks.push(fail(
                "provider.settings",
                error.to_string(),
                "Run `lucy setup` to repair provider settings.",
            ));
            return;
        }
    };
    check_shell(
        std::env::current_dir().as_deref().unwrap_or(home),
        Some(&settings.api_key_env),
        checks,
    );
    if !reqwest::Url::parse(&settings.base_url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    {
        checks.push(fail(
            "provider.endpoint",
            "provider endpoint is not a valid URL",
            "Correct the endpoint with `lucy setup`.",
        ));
        return;
    }
    checks.push(pass(
        "provider.endpoint",
        "provider endpoint and model are configured",
    ));

    let secret = match auth.provider {
        AuthProvider::Openrouter => match std::env::var(&settings.api_key_env) {
            Ok(value) if !value.is_empty() => {
                checks.push(pass(
                    "provider.auth",
                    "configured API-key environment variable is present",
                ));
                Some(value)
            }
            _ => {
                checks.push(fail(
                    "provider.auth",
                    "configured API-key environment variable is absent",
                    "Export the configured provider key, then rerun `lucy doctor`.",
                ));
                None
            }
        },
        AuthProvider::CodexSubscription => match AuthStore::for_home(home).load() {
            Ok(Some(credentials)) => {
                checks.push(pass(
                    "provider.auth",
                    "Codex credential store is present and valid",
                ));
                Some(credentials.access)
            }
            Ok(None) => {
                checks.push(fail(
                    "provider.auth",
                    "Codex is not logged in",
                    "Run `lucy setup` or `lucy codex login`.",
                ));
                None
            }
            Err(error) => {
                checks.push(fail(
                    "provider.auth",
                    error.to_string(),
                    "Run `lucy codex login` to replace invalid credentials.",
                ));
                None
            }
        },
    };
    let Some(secret) = secret else {
        return;
    };
    let provider = if settings.api_key_env == CODEX_API_KEY_ENV_SENTINEL {
        Provider::new_codex(home, &settings)
    } else {
        Provider::new(&settings)
    };
    let provider = match provider {
        Ok(provider) => provider,
        Err(error) => {
            checks.push(fail(
                "provider.construct",
                redact_secret(&error.to_string(), Some(&secret)),
                "Check authentication and provider settings.",
            ));
            return;
        }
    };
    checks.push(pass(
        "provider.construct",
        "provider initialized without creating a session",
    ));
    match provider.models() {
        Ok(models) => {
            let selected = models.iter().find(|model| model.id == settings.model);
            checks.push(pass("provider.metadata", format!("provider model metadata loaded ({} models)", models.len())).details(serde_json::json!({"selected_model": settings.model, "supported_efforts": selected.and_then(|model| model.efforts.clone()), "context_window": provider.context_window()})));
        }
        Err(error) => checks.push(warning(
            "provider.metadata",
            redact_secret(&error.to_string(), Some(&secret)),
            "Optional model metadata may be unsupported; verify the model identifier manually.",
        )),
    }
    if live {
        let messages = [
            ChatMessage::system("Reply with exactly OK. Do not call tools.".to_owned()),
            ChatMessage::user("diagnostic probe".to_owned()),
        ];
        match provider.stream_chat(&messages, &mut |_| Ok(())) {
            Ok(_) => checks.push(pass(
                "provider.live",
                "bounded provider stream completed and accepted Lucy's cmd schema",
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
fn warning(id: &'static str, message: impl Into<String>, remediation: &'static str) -> Check {
    check(id, Status::Warning, message, Some(remediation))
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
        let json = serde_json::to_string(&report).expect("JSON");
        assert!(!json.contains('\u{1b}'));
        assert!(json.contains("\"status\":\"pass\""));
    }
}
