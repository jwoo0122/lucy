use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
pub const GENERATED_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
pub const DEFAULT_SYSTEM_PROMPT: &str = "You can access computer resources. Use the provided tools to achieve the user's requirements. When needed, use cmd to read a relevant skill's SKILL.md.";

const GENERATED_CONFIG: &str = r#"system_prompt = "You can access computer resources. Use the provided tools to achieve the user's requirements. When needed, use cmd to read a relevant skill's SKILL.md."

[llm]
base_url = "https://openrouter.ai/api/v1"
model = ""
api_key_env = "OPENROUTER_API_KEY"
# Optional reasoning effort sent as the OpenAI Chat Completions "reasoning_effort"
# field, e.g. "low", "medium", "high". Omit or leave unset to send no effort.
# Use a value your provider and model support; an unsupported value fails at runtime.
# effort = "medium"
"#;

#[derive(Debug)]
pub struct ConfigError(String);

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(_error: io::Error) -> Self {
        Self::new("configuration file error")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    #[serde(default)]
    pub llm: LlmConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LlmConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LlmSettings {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_owned(),
            llm: LlmConfig::default(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: String::new(),
            api_key_env: None,
            effort: None,
        }
    }
}

fn default_system_prompt() -> String {
    DEFAULT_SYSTEM_PROMPT.to_owned()
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

impl Config {
    pub fn load_or_create(home: &Path) -> Result<Self, ConfigError> {
        Self::ensure_exists(home)?;
        Self::load_from_path(&config_path(home))
    }

    pub fn ensure_exists(home: &Path) -> Result<(), ConfigError> {
        let path = config_path(home);
        ensure_private_dir(&lucy_dir(home))?;
        ensure_not_symlink(&path)?;

        if !path.exists() && generated_config_contains_active_key() {
            return Err(ConfigError::new("configuration bootstrap rejected"));
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(GENERATED_CONFIG.as_bytes())?;
                file.flush()?;
                ensure_private_file(&path)?;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                ensure_private_file(&path)?;
                Ok(())
            }
            Err(_error) => Err(ConfigError::new("unable to create config.toml")),
        }
    }

    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        ensure_not_symlink(path)
            .map_err(|_error| ConfigError::new("unable to secure config.toml"))?;
        let bytes = fs::read_to_string(path)
            .map_err(|_error| ConfigError::new("unable to read config.toml"))?;
        ensure_private_file(path)
            .map_err(|_error| ConfigError::new("unable to secure config.toml"))?;
        toml::from_str(&bytes)
            .map_err(|_| ConfigError::new("unable to parse config.toml: invalid TOML"))
    }

    pub fn resolved_llm(&self) -> Result<LlmSettings, ConfigError> {
        let base_url = self.llm.base_url.trim().to_owned();
        if base_url.is_empty() {
            return Err(ConfigError::new("llm.base_url must not be empty"));
        }

        let api_key_env = self
            .llm
            .api_key_env
            .as_deref()
            .unwrap_or(DEFAULT_API_KEY_ENV)
            .trim()
            .to_owned();
        if api_key_env.is_empty() {
            return Err(ConfigError::new("llm.api_key_env must not be empty"));
        }

        let effort = self.llm.effort.as_deref().map(str::trim).map(str::to_owned);

        Ok(LlmSettings {
            base_url,
            model: self.llm.model.trim().to_owned(),
            api_key_env,
            effort,
        })
    }
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".lucy").join("config.toml")
}

pub fn lucy_dir(home: &Path) -> PathBuf {
    home.join(".lucy")
}

fn generated_config_contains_active_key() -> bool {
    std::env::var(GENERATED_API_KEY_ENV)
        .ok()
        .filter(|secret| !secret.is_empty())
        .is_some_and(|secret| GENERATED_CONFIG.contains(&secret))
}

pub(crate) fn ensure_not_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symlinks are not allowed for protected paths",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    ensure_not_symlink(path)?;
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "protected path is not a directory",
        ));
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(crate) fn ensure_private_file(path: &Path) -> io::Result<()> {
    ensure_not_symlink(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temporary_home() -> PathBuf {
        loop {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lucy-config-{stamp}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temp home: {error}"),
            }
        }
    }

    #[test]
    fn bootstraps_config_without_overwriting_existing_bytes() {
        let home = temporary_home();
        let first = Config::load_or_create(&home).expect("create config");
        assert_eq!(first.llm.model, "");
        assert_eq!(first.llm.base_url, DEFAULT_BASE_URL);
        assert_eq!(
            first.llm.api_key_env.as_deref(),
            Some(GENERATED_API_KEY_ENV)
        );

        let path = config_path(&home);
        let generated = fs::read(&path).expect("generated bytes");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path)
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(lucy_dir(&home))
                .expect("Lucy directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let custom = b"system_prompt = \"custom\"\n[llm]\nmodel = \"local\"\n";
        fs::write(&path, custom).expect("custom config");
        let loaded = Config::load_or_create(&home).expect("load custom config");
        assert_eq!(loaded.system_prompt, "custom");
        assert_eq!(loaded.llm.model, "local");
        assert_ne!(generated, custom);
        assert_eq!(fs::read(path).expect("bytes after load"), custom);

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_config_files_and_directories() {
        let home = temporary_home();
        let lucy = home.join(".lucy");
        fs::create_dir(&lucy).expect("Lucy directory");
        let target = home.join("config-target.toml");
        fs::write(&target, "system_prompt = \"target\"\n").expect("target config");
        let path = config_path(&home);
        symlink(&target, &path).expect("config symlink");
        assert!(Config::load_or_create(&home).is_err());
        assert!(Config::load_from_path(&path).is_err());
        fs::remove_file(path).expect("remove config symlink");
        fs::remove_dir(lucy).expect("remove Lucy directory");
        fs::remove_file(target).expect("remove target config");
        fs::remove_dir(&home).expect("remove temp home");

        let home = temporary_home();
        let target = home.join("lucy-target");
        fs::create_dir(&target).expect("target directory");
        symlink(&target, home.join(".lucy")).expect("Lucy directory symlink");
        assert!(Config::ensure_exists(&home).is_err());
        fs::remove_file(home.join(".lucy")).expect("remove Lucy directory symlink");
        fs::remove_dir(target).expect("remove target directory");
        fs::remove_dir(home).expect("remove temp home");
    }

    #[test]
    fn malformed_toml_error_does_not_include_source_details() {
        let home = temporary_home();
        let path = config_path(&home);
        fs::create_dir_all(path.parent().expect("config parent")).expect("config parent");
        fs::write(
            &path,
            "system_prompt = \"provider-secret\n[llm]\nmodel = [\n",
        )
        .expect("malformed config");

        let error = Config::load_from_path(&path).expect_err("malformed TOML");
        let message = error.to_string();
        assert!(message.contains("invalid TOML"));
        assert!(!message.contains("provider-secret"));
        assert!(!message.contains("system_prompt"));
        assert!(!message.contains(&path.display().to_string()));
        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[test]
    fn omitted_api_key_environment_uses_openai_default() {
        let config = Config {
            system_prompt: "prompt".to_owned(),
            llm: LlmConfig {
                base_url: "http://localhost".to_owned(),
                model: "model".to_owned(),
                api_key_env: None,
                effort: None,
            },
        };
        assert_eq!(
            config.resolved_llm().expect("settings").api_key_env,
            DEFAULT_API_KEY_ENV
        );
    }

    #[test]
    fn resolved_effort_passes_through_and_trims() {
        let config = |effort: Option<&str>| Config {
            system_prompt: "prompt".to_owned(),
            llm: LlmConfig {
                base_url: "http://localhost".to_owned(),
                model: "model".to_owned(),
                api_key_env: Some("LUCY_KEY".to_owned()),
                effort: effort.map(str::to_owned),
            },
        };
        assert_eq!(config(None).resolved_llm().expect("none").effort, None);
        assert_eq!(
            config(Some("high"))
                .resolved_llm()
                .expect("set")
                .effort
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            config(Some("  medium  "))
                .resolved_llm()
                .expect("trim")
                .effort
                .as_deref(),
            Some("medium")
        );
    }
}
