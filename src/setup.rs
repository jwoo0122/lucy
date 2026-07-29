use std::io::{BufRead, Write};
use std::path::Path;

use crate::auth::AuthStore;
use crate::codex_provider::CODEX_ENDPOINT;
use crate::config::{
    config_path, AuthProvider, Config, SetupSelection, DEFAULT_BASE_URL, GENERATED_API_KEY_ENV,
};
use crate::provider::{Provider, ProviderModel};

const CANCEL_WORDS: [&str; 2] = ["cancel", "q"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupOutcome {
    Saved,
    Cancelled,
}

pub fn configuration_is_complete(home: &Path, config: &Config) -> bool {
    let Ok(auth) = config.resolved_auth() else {
        return false;
    };
    let Ok(settings) = config.resolved_llm() else {
        return false;
    };
    if settings.model.is_empty() {
        return false;
    }
    match auth.provider {
        AuthProvider::Openrouter => !settings.api_key_env.trim().is_empty(),
        AuthProvider::CodexSubscription => AuthStore::for_home(home)
            .load()
            .is_ok_and(|value| value.is_some()),
    }
}

pub fn run<R: BufRead, W: Write>(
    home: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<SetupOutcome, String> {
    let path = config_path(home);
    let legacy_path = home.join(".lucy/config.toml");
    let original = std::fs::read(&path).ok();
    let legacy_original = std::fs::read(&legacy_path).ok();
    let result = run_inner(home, input, output);
    if !matches!(result, Ok(SetupOutcome::Saved)) {
        match original {
            Some(bytes) => {
                let _ = std::fs::write(&path, bytes);
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        if let Some(bytes) = legacy_original {
            if let Some(parent) = legacy_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&legacy_path, bytes);
        }
    }
    result
}

fn run_inner<R: BufRead, W: Write>(
    home: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<SetupOutcome, String> {
    Config::ensure_exists(home).map_err(|error| error.to_string())?;
    let current = Config::load_from_path(&config_path(home)).map_err(|error| error.to_string())?;
    writeln!(
        output,
        "Lucy setup (type 'cancel' at any prompt to leave configuration unchanged)"
    )
    .map_err(io_error)?;

    let current_provider = if current.auth.provider == AuthProvider::CodexSubscription {
        "2"
    } else {
        "1"
    };
    let provider = prompt(
        input,
        output,
        "Connection: 1) API key  2) Codex subscription",
        Some(current_provider),
    )?;
    let Some(provider) = provider else {
        return Ok(SetupOutcome::Cancelled);
    };
    let selection = match provider.as_str() {
        "1" => api_key_selection(&current, input, output)?,
        "2" => codex_selection(home, &current, input, output)?,
        _ => return Err("connection must be 1 or 2".to_owned()),
    };
    let Some(selection) = selection else {
        return Ok(SetupOutcome::Cancelled);
    };

    writeln!(output, "\nReview").map_err(io_error)?;
    writeln!(output, "  Provider: {}", selection.provider_name()).map_err(io_error)?;
    writeln!(output, "  Endpoint: {}", selection.endpoint_summary()).map_err(io_error)?;
    if let Some(environment) = selection.api_key_env.as_deref() {
        writeln!(
            output,
            "  Credential source: environment variable {environment}"
        )
        .map_err(io_error)?;
    } else {
        writeln!(
            output,
            "  Credential source: Lucy private Codex credential store"
        )
        .map_err(io_error)?;
    }
    writeln!(output, "  Model: {}", selection.model).map_err(io_error)?;
    writeln!(
        output,
        "  Effort: {}",
        selection.effort.as_deref().unwrap_or("none")
    )
    .map_err(io_error)?;
    writeln!(output, "  Config: {}", config_path(home).display()).map_err(io_error)?;
    let save = prompt(input, output, "Save configuration? [y/N]", Some("n"))?;
    if !save
        .as_deref()
        .is_some_and(|answer| matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
    {
        writeln!(output, "Setup cancelled; configuration was not changed.").map_err(io_error)?;
        return Ok(SetupOutcome::Cancelled);
    }
    Config::save_setup(home, &selection).map_err(|error| error.to_string())?;
    writeln!(
        output,
        "Configuration saved to {}",
        config_path(home).display()
    )
    .map_err(io_error)?;
    Ok(SetupOutcome::Saved)
}

fn api_key_selection<R: BufRead, W: Write>(
    current: &Config,
    input: &mut R,
    output: &mut W,
) -> Result<Option<SetupSelection>, String> {
    let base = prompt(
        input,
        output,
        "Endpoint/base URL",
        Some(current.llm.base_url.trim())
            .filter(|v| !v.is_empty())
            .or(Some(DEFAULT_BASE_URL)),
    )?;
    let Some(base_url) = base else {
        return Ok(None);
    };
    validate_url(&base_url)?;
    let current_env = current
        .auth
        .api_key_env
        .as_deref()
        .or(current.llm.api_key_env.as_deref())
        .unwrap_or(GENERATED_API_KEY_ENV);
    let environment = prompt(
        input,
        output,
        "API-key environment variable",
        Some(current_env),
    )?;
    let Some(environment) = environment else {
        return Ok(None);
    };
    if !valid_environment_name(&environment) {
        return Err("API-key environment variable must be a shell identifier".to_owned());
    }
    if std::env::var(&environment).is_err() {
        writeln!(output, "Warning: {environment} is not currently set; Lucy will require it when starting a session.").map_err(io_error)?;
    }
    let model = prompt(
        input,
        output,
        "Model identifier",
        Some(current.llm.model.trim()).filter(|v| !v.is_empty()),
    )?;
    let Some(model) = model else { return Ok(None) };
    if model.trim().is_empty() {
        return Err("model identifier must not be empty".to_owned());
    }
    let effort = prompt(
        input,
        output,
        "Reasoning effort (optional; '-' for none)",
        current.llm.effort.as_deref(),
    )?;
    let Some(effort) = effort else {
        return Ok(None);
    };
    if let Ok(secret) = std::env::var(&environment) {
        if !secret.is_empty() && effort.contains(&secret) {
            return Err("reasoning effort contains the active provider credential".to_owned());
        }
    }
    Ok(Some(SetupSelection {
        provider: AuthProvider::Openrouter,
        base_url: Some(base_url),
        api_key_env: Some(environment),
        model,
        effort: optional_effort(&effort),
    }))
}

fn codex_selection<R: BufRead, W: Write>(
    home: &Path,
    current: &Config,
    input: &mut R,
    output: &mut W,
) -> Result<Option<SetupSelection>, String> {
    if AuthStore::for_home(home)
        .load()
        .map_err(|error| error.to_string())?
        .is_none()
    {
        writeln!(output, "Opening browser for Codex sign-in...").map_err(io_error)?;
        crate::auth::login(home).map_err(|error| error.to_string())?;
    }
    let candidate = crate::config::LlmSettings {
        base_url: DEFAULT_BASE_URL.to_owned(),
        model: if current.llm.model.trim().is_empty() {
            "gpt-5.3-codex".to_owned()
        } else {
            current.llm.model.trim().to_owned()
        },
        api_key_env: crate::codex_provider::CODEX_ENV_SENTINEL.to_owned(),
        effort: None,
    };
    let models = Provider::new_codex(home, &candidate)
        .map_err(|error| error.to_string())?
        .models()
        .map_err(|error| error.to_string())?;
    print_models(output, &models)?;
    let default_model = current.llm.model.trim();
    let model = prompt(
        input,
        output,
        "Codex model",
        Some(default_model)
            .filter(|value| models.iter().any(|entry| entry.id == *value))
            .or_else(|| models.first().map(|m| m.id.as_str())),
    )?;
    let Some(model) = model else { return Ok(None) };
    let metadata = models.iter().find(|entry| entry.id == model);
    if metadata.is_none() {
        return Err("Codex model must be selected from the available catalog".to_owned());
    }
    if let Some(efforts) = metadata.and_then(|entry| entry.efforts.as_ref()) {
        writeln!(output, "Supported efforts: {}", efforts.join(", ")).map_err(io_error)?;
    }
    let effort = prompt(
        input,
        output,
        "Reasoning effort (optional; '-' for none)",
        current.llm.effort.as_deref(),
    )?;
    let Some(effort) = effort else {
        return Ok(None);
    };
    let effort = optional_effort(&effort);
    if let (Some(value), Some(supported)) = (
        effort.as_ref(),
        metadata.and_then(|entry| entry.efforts.as_ref()),
    ) {
        if !supported.contains(value) {
            return Err("reasoning effort is not supported by the selected Codex model".to_owned());
        }
    }
    Ok(Some(SetupSelection {
        provider: AuthProvider::CodexSubscription,
        base_url: None,
        api_key_env: None,
        model,
        effort,
    }))
}

fn print_models<W: Write>(output: &mut W, models: &[ProviderModel]) -> Result<(), String> {
    writeln!(output, "Available Codex models:").map_err(io_error)?;
    for model in models {
        writeln!(output, "  {}", model.id).map_err(io_error)?;
    }
    Ok(())
}

fn prompt<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    label: &str,
    default: Option<&str>,
) -> Result<Option<String>, String> {
    match default {
        Some(default) => write!(output, "{label} [{default}]: "),
        None => write!(output, "{label}: "),
    }
    .map_err(io_error)?;
    output.flush().map_err(io_error)?;
    let mut line = String::new();
    if input.read_line(&mut line).map_err(io_error)? == 0 {
        return Ok(None);
    }
    let value = line.trim();
    if CANCEL_WORDS
        .iter()
        .any(|word| value.eq_ignore_ascii_case(word))
    {
        return Ok(None);
    }
    Ok(Some(if value.is_empty() {
        default.unwrap_or_default().to_owned()
    } else {
        value.to_owned()
    }))
}

fn validate_url(value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| "endpoint/base URL must be a valid HTTP(S) URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("endpoint/base URL must be a valid HTTP(S) URL".to_owned());
    }
    Ok(())
}
fn valid_environment_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
fn optional_effort(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value != "-").then(|| value.to_owned())
}
fn io_error(error: std::io::Error) -> String {
    format!("setup terminal error: {error}")
}

impl SetupSelection {
    fn provider_name(&self) -> &'static str {
        if self.provider == AuthProvider::CodexSubscription {
            "Codex subscription"
        } else {
            "OpenAI-compatible API key"
        }
    }
    fn endpoint_summary(&self) -> &str {
        self.base_url.as_deref().unwrap_or(CODEX_ENDPOINT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn home() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lucy-setup-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("temporary home");
        path
    }

    #[test]
    fn fresh_api_key_setup_saves_selection_without_secret_or_session() {
        let home = home();
        let secret = "setup-provider-secret-value";
        let input = b"1\nhttps://example.test/v1\nLUCY_SETUP_TEST_KEY\nprovider/model\nhigh\ny\n";
        let mut input = Cursor::new(input);
        let mut output = Vec::new();
        run(&home, &mut input, &mut output).expect("setup");

        let config = fs::read_to_string(config_path(&home)).expect("config");
        assert!(config.contains("https://example.test/v1"));
        assert!(config.contains("LUCY_SETUP_TEST_KEY"));
        assert!(config.contains("provider/model"));
        assert!(!config.contains(secret));
        assert!(!home.join(".lucy/sessions").exists());
        assert!(!String::from_utf8(output).expect("output").contains(secret));
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn cancelling_fresh_setup_leaves_no_config_or_session() {
        let home = home();
        let mut input = Cursor::new(b"cancel\n".as_slice());
        assert_eq!(
            run(&home, &mut input, &mut Vec::new()).expect("cancel"),
            SetupOutcome::Cancelled
        );
        assert!(!config_path(&home).exists());
        assert!(!home.join(".lucy/sessions").exists());
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn rerun_preserves_comments_and_unrelated_content() {
        let home = home();
        Config::ensure_exists(&home).expect("config");
        let path = config_path(&home);
        fs::write(
            &path,
            "# keep this comment\ncustom = \"value\"\n\n[auth]\nprovider = \"openrouter\"\napi_key_env = \"OLD_KEY\"\n\n[llm]\nbase_url = \"https://old.test/v1\"\nmodel = \"old\"\n\n[execution]\npolicy = \"policy.sh\"\n",
        )
        .expect("existing config");
        let mut input =
            Cursor::new(b"1\nhttps://new.test/v1\nNEW_KEY\nnew-model\nmedium\ny\n".as_slice());
        run(&home, &mut input, &mut Vec::new()).expect("setup");
        let updated = fs::read_to_string(path).expect("updated");
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("custom = \"value\""));
        assert!(updated.contains("policy = \"policy.sh\""));
        assert!(updated.contains("NEW_KEY"));
        assert!(!updated.contains("OLD_KEY"));
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn cancel_and_validation_failure_leave_valid_config_byte_identical() {
        let home = home();
        Config::ensure_exists(&home).expect("config");
        let path = config_path(&home);
        let original = fs::read(&path).expect("original");

        let mut cancel = Cursor::new(b"cancel\n".as_slice());
        assert_eq!(
            run(&home, &mut cancel, &mut Vec::new()).expect("cancel"),
            SetupOutcome::Cancelled
        );
        assert_eq!(fs::read(&path).expect("after cancel"), original);

        let mut invalid = Cursor::new(b"1\nnot-a-url\n".as_slice());
        assert!(run(&home, &mut invalid, &mut Vec::new()).is_err());
        assert_eq!(fs::read(&path).expect("after validation"), original);
        fs::remove_dir_all(home).expect("cleanup");
    }
}
