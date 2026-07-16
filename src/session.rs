use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    ensure_not_symlink, ensure_private_dir, ensure_private_file, lucy_dir, LlmSettings,
};
use crate::model::ChatMessage;
use crate::redaction::{conflicts_with_protected_literal, redact_secret};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct SessionError(String);

impl SessionError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SessionError {}

impl From<io::Error> for SessionError {
    fn from(_error: io::Error) -> Self {
        Self::new("session storage error")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record")]
enum SessionRecord {
    #[serde(rename = "session")]
    Session {
        version: u8,
        session_id: String,
        created_at: u64,
        cwd: String,
        boot_system_prompt: String,
        llm: LlmSettings,
    },
    #[serde(rename = "message")]
    Message {
        timestamp: u64,
        message: ChatMessage,
    },
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub boot_system_prompt: String,
    pub llm: LlmSettings,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<ChatMessage>,
    secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionMetadata {
    #[serde(rename = "type")]
    pub record_type: &'static str,
    pub session_id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub first_message: Option<String>,
    pub last_message: Option<String>,
}

impl Session {
    pub fn create(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
    ) -> Result<Self, SessionError> {
        let secret = std::env::var(&llm.api_key_env).ok();
        Self::create_with_secret(home, cwd, boot_system_prompt, llm, secret.as_deref())
    }

    pub fn create_with_secret(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
        secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        let cwd = fs::canonicalize(cwd)
            .map_err(|_error| SessionError::new("unable to resolve session cwd"))?;
        let sessions_directory = sessions_dir(home);
        ensure_private_dir(&lucy_dir(home))?;
        ensure_private_dir(&sessions_directory)?;
        let created_at = now();

        if let Some(secret) = secret {
            if conflicts_with_protected_literal(secret) {
                return Err(session_header_rejected(secret));
            }
        }

        for _ in 0..16 {
            let id = new_session_id();
            let path = sessions_directory.join(format!("{id}.jsonl"));
            let record = SessionRecord::Session {
                version: 1,
                session_id: id.clone(),
                created_at,
                cwd: cwd.display().to_string(),
                boot_system_prompt: boot_system_prompt.clone(),
                llm: llm.clone(),
            };
            if let Some(secret) = secret {
                if record_contains_secret(&record, secret) {
                    return Err(session_header_rejected(secret));
                }
            }

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    ensure_private_file(&path)?;
                    write_record(&mut file, &record)?;
                    return Ok(Self {
                        id,
                        path,
                        cwd,
                        boot_system_prompt,
                        llm,
                        created_at,
                        updated_at: created_at,
                        messages: Vec::new(),
                        secret: secret.map(str::to_owned),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_error) => return Err(SessionError::new("unable to create session file")),
            }
        }
        Err(SessionError::new("unable to allocate a unique session id"))
    }

    pub fn resume(home: &Path, id: &str) -> Result<Self, SessionError> {
        validate_session_id(id)?;
        let directory = sessions_dir(home);
        let lucy_directory = lucy_dir(home);
        ensure_not_symlink(&lucy_directory)?;
        if lucy_directory.is_dir() {
            ensure_private_dir(&lucy_directory)?;
        }
        ensure_not_symlink(&directory)?;
        if directory.is_dir() {
            ensure_private_dir(&directory)?;
        }
        let path = directory.join(format!("{id}.jsonl"));
        ensure_not_symlink(&path)?;
        if !path.is_file() {
            return Err(SessionError::new("session not found"));
        }

        ensure_private_file(&path)?;
        let raw =
            fs::read(&path).map_err(|_error| SessionError::new("unable to read session file"))?;
        let active_secret = session_header_secret(&raw);
        if let Some(secret) = active_secret.as_deref() {
            if conflicts_with_protected_literal(secret) || bytes_contain_secret(&raw, secret) {
                return Err(session_header_rejected(secret));
            }
        }

        let reader = BufReader::new(raw.as_slice());
        let mut header = None;
        let mut messages = Vec::new();
        let mut updated_at = None;

        for (line_number, line) in reader.lines().enumerate() {
            let line = line.map_err(|_error| {
                session_error("unable to read session file", active_secret.as_deref())
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let value = parse_json_value(&line).map_err(|_error| {
                session_error(
                    format!("invalid session record at line {}", line_number + 1),
                    active_secret.as_deref(),
                )
            })?;
            if let Some(secret) = active_secret.as_deref() {
                if json_value_contains_secret(&value, secret) {
                    return Err(session_header_rejected(secret));
                }
            }
            let record: SessionRecord = serde_json::from_value(value).map_err(|_error| {
                session_error(
                    format!("invalid session record at line {}", line_number + 1),
                    active_secret.as_deref(),
                )
            })?;
            if let Some(secret) = active_secret.as_deref() {
                if record_contains_secret(&record, secret) {
                    return Err(session_header_rejected(secret));
                }
            }
            match record {
                SessionRecord::Session {
                    version,
                    session_id,
                    created_at,
                    cwd,
                    boot_system_prompt,
                    llm,
                } => {
                    if version != 1 || session_id != id || header.is_some() {
                        return Err(session_error(
                            "invalid session header",
                            active_secret.as_deref(),
                        ));
                    }
                    header = Some((created_at, cwd, boot_system_prompt, llm));
                }
                SessionRecord::Message { timestamp, message } => {
                    if header.is_none() {
                        return Err(session_error(
                            "session message precedes header",
                            active_secret.as_deref(),
                        ));
                    }
                    updated_at = Some(timestamp);
                    messages.push(message);
                }
            }
        }

        let Some((created_at, cwd, boot_system_prompt, llm)) = header else {
            return Err(session_error(
                "session has no header",
                active_secret.as_deref(),
            ));
        };
        let cwd = PathBuf::from(cwd);
        Ok(Self {
            id: id.to_owned(),
            path,
            cwd,
            boot_system_prompt,
            llm,
            created_at,
            updated_at: updated_at.unwrap_or(created_at),
            messages,
            secret: active_secret,
        })
    }

    pub fn append_message(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        let timestamp = now();
        let record = SessionRecord::Message {
            timestamp,
            message: message.clone(),
        };
        if let Some(secret) = self.secret.as_deref() {
            if record_contains_secret(&record, secret) {
                return Err(session_record_rejected(secret));
            }
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.messages.push(message);
        self.updated_at = timestamp;
        Ok(())
    }

    pub fn provider_messages(&self) -> Vec<ChatMessage> {
        let mut messages = Vec::with_capacity(self.messages.len() + 1);
        messages.push(ChatMessage::system(self.boot_system_prompt.clone()));
        messages.extend(self.messages.clone());
        messages
    }

    pub fn list(home: &Path) -> Result<Vec<SessionMetadata>, SessionError> {
        let directory = sessions_dir(home);
        let lucy_directory = lucy_dir(home);
        ensure_not_symlink(&lucy_directory)?;
        if lucy_directory.is_dir() {
            ensure_private_dir(&lucy_directory)?;
        }
        ensure_not_symlink(&directory)?;
        if directory.is_dir() {
            ensure_private_dir(&directory)?;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_error) => return Err(SessionError::new("unable to list sessions")),
        };

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
                && metadata.is_file()
            {
                paths.push(path);
            }
        }
        paths.sort();

        let mut metadata = Vec::new();
        for path in paths {
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(session) = Self::resume(home, id) else {
                continue;
            };
            metadata.push(SessionMetadata {
                record_type: "session_metadata",
                session_id: session.id,
                created_at: session.created_at,
                updated_at: session.updated_at,
                first_message: session.messages.first().map(safe_message_summary),
                last_message: session.messages.last().map(safe_message_summary),
            });
        }
        Ok(metadata)
    }
}

struct DuplicateKeyValue(Value);

impl<'de> Deserialize<'de> for DuplicateKeyValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer
            .deserialize_any(DuplicateKeyValueVisitor)
            .map(Self)
    }
}

struct DuplicateKeyValueSeed;

impl<'de> DeserializeSeed<'de> for DuplicateKeyValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyValueVisitor)
    }
}

struct DuplicateKeyValueVisitor;

impl<'de> Visitor<'de> for DuplicateKeyValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a valid JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_i128(value)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("JSON number out of range"))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_u128(value)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("JSON number out of range"))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(DuplicateKeyValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate object key"));
            }
            let value = access.next_value_seed(DuplicateKeyValueSeed)?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

fn parse_json_value(line: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str::<DuplicateKeyValue>(line).map(|value| value.0)
}

fn session_header_secret(raw: &[u8]) -> Option<String> {
    let line = raw
        .split(|byte| *byte == b'\n')
        .find(|line| !line.iter().all(|byte| byte.is_ascii_whitespace()))?;
    let value = parse_json_value(std::str::from_utf8(line).ok()?).ok()?;
    let api_key_env = value
        .get("llm")
        .and_then(|llm| llm.get("api_key_env"))
        .and_then(Value::as_str)?;
    let secret = std::env::var(api_key_env).ok()?;
    (!secret.is_empty()).then_some(secret)
}

fn bytes_contain_secret(raw: &[u8], secret: &str) -> bool {
    let secret = secret.as_bytes();
    !secret.is_empty() && raw.windows(secret.len()).any(|window| window == secret)
}

fn record_contains_secret(record: &SessionRecord, secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    if serde_json::to_vec(record)
        .ok()
        .is_some_and(|serialized| bytes_contain_secret(&serialized, secret))
    {
        return true;
    }
    match record {
        SessionRecord::Session {
            version,
            session_id,
            created_at,
            cwd,
            boot_system_prompt,
            llm,
        } => {
            version.to_string().contains(secret)
                || session_id.contains(secret)
                || created_at.to_string().contains(secret)
                || cwd.contains(secret)
                || boot_system_prompt.contains(secret)
                || llm.base_url.contains(secret)
                || llm.model.contains(secret)
                || llm.api_key_env.contains(secret)
        }
        SessionRecord::Message { timestamp, message } => {
            timestamp.to_string().contains(secret) || message_contains_secret(message, secret)
        }
    }
}

fn message_contains_secret(message: &ChatMessage, secret: &str) -> bool {
    message.role.contains(secret)
        || message
            .content
            .as_deref()
            .is_some_and(|content| content.contains(secret))
        || message
            .name
            .as_deref()
            .is_some_and(|name| name.contains(secret))
        || message
            .tool_call_id
            .as_deref()
            .is_some_and(|id| id.contains(secret))
        || message.tool_calls.iter().any(|call| {
            call.id.contains(secret)
                || call.name.contains(secret)
                || call.arguments.contains(secret)
                || tool_arguments_contain_secret(&call.arguments, secret)
        })
}

fn tool_arguments_contain_secret(arguments: &str, secret: &str) -> bool {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .is_some_and(|value| json_value_contains_secret(&value, secret))
}

fn json_value_contains_secret(value: &Value, secret: &str) -> bool {
    match value {
        Value::String(text) => text.contains(secret),
        Value::Array(values) => values
            .iter()
            .any(|value| json_value_contains_secret(value, secret)),
        Value::Object(object) => {
            object.keys().any(|key| key.contains(secret))
                || object
                    .values()
                    .any(|value| json_value_contains_secret(value, secret))
        }
        Value::Number(number) => number.to_string().contains(secret),
        Value::Bool(_) | Value::Null => false,
    }
}

fn session_error(message: impl Into<String>, secret: Option<&str>) -> SessionError {
    let message = message.into();
    SessionError::new(redact_secret(&message, secret))
}

fn session_header_rejected(secret: &str) -> SessionError {
    session_error("session header rejected", Some(secret))
}

fn session_record_rejected(secret: &str) -> SessionError {
    session_error("session record rejected", Some(secret))
}

fn open_session_for_append(path: &Path) -> Result<File, SessionError> {
    let mut options = OpenOptions::new();
    options.write(true).append(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(not(unix))]
    ensure_not_symlink(path)?;

    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SessionError::new(
            "session file is not a regular private file",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(SessionError::new(
            "session file is not a regular private file",
        ));
    }
    Ok(file)
}

fn write_record(file: &mut File, record: &SessionRecord) -> Result<(), SessionError> {
    let line = serde_json::to_string(record)
        .map_err(|error| SessionError::new(format!("unable to encode session record: {error}")))?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn safe_message_summary(message: &ChatMessage) -> String {
    let role = message.role.as_str();
    let text = message
        .content
        .as_deref()
        .or_else(|| message.tool_calls.first().map(|call| call.name.as_str()))
        .unwrap_or("");
    let mut summary = text.chars().take(120).collect::<String>();
    if text.chars().count() > 120 {
        summary.push('…');
    }
    format!("{role}: {summary}")
}

pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join(".lucy").join("sessions")
}

pub fn validate_session_id(id: &str) -> Result<(), SessionError> {
    if id.is_empty()
        || !id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(SessionError::new("session id contains invalid characters"));
    }
    Ok(())
}

fn new_session_id() -> String {
    let timestamp = now();
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp}-{}-{counter}", std::process::id())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmSettings;
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
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
                "lucy-session-{stamp}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temp home: {error}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_non_private_opened_session_file_without_chmod() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_APPEND_TEST_KEY".to_owned(),
        };
        let mut session =
            Session::create(&home, &cwd, "prompt".to_owned(), llm).expect("create session");
        fs::set_permissions(&session.path, fs::Permissions::from_mode(0o644))
            .expect("make session group-readable");

        let error = session
            .append_message(ChatMessage::user("must not append".to_owned()))
            .expect_err("unsafe permissions should be rejected");
        assert!(error.to_string().contains("private"));
        assert_eq!(
            fs::metadata(&session.path)
                .expect("session metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_symlinked_session_path() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_APPEND_LINK_KEY".to_owned(),
        };
        let mut session =
            Session::create(&home, &cwd, "prompt".to_owned(), llm).expect("create session");
        let target = home.join("append-target.jsonl");
        fs::write(&target, "target\n").expect("target file");
        fs::remove_file(&session.path).expect("remove session path");
        symlink(&target, &session.path).expect("session symlink");

        session
            .append_message(ChatMessage::user("must not append".to_owned()))
            .expect_err("symlink should be rejected");
        assert_eq!(
            fs::read_to_string(&target).expect("target contents"),
            "target\n"
        );

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_fifo_without_blocking() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost".to_owned(),
            model: "model".to_owned(),
            api_key_env: "LUCY_APPEND_FIFO_KEY".to_owned(),
        };
        let mut session =
            Session::create(&home, &cwd, "prompt".to_owned(), llm).expect("create session");
        fs::remove_file(&session.path).expect("remove session path");
        let fifo_path = CString::new(session.path.as_os_str().as_bytes()).expect("FIFO path");
        let result = unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) };
        assert_eq!(result, 0, "mkfifo: {:?}", io::Error::last_os_error());

        session
            .append_message(ChatMessage::user("must not append".to_owned()))
            .expect_err("FIFO should be rejected without a blocking open");

        fs::remove_dir_all(home).expect("remove temp home");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_session_files_and_directories() {
        let home = temporary_home();
        let directory = home.join(".lucy/sessions");
        fs::create_dir_all(&directory).expect("sessions directory");
        let target = home.join("session-target.jsonl");
        fs::write(&target, "not a session\n").expect("target session");
        let path = directory.join("linked.jsonl");
        symlink(&target, &path).expect("session symlink");
        assert!(Session::resume(&home, "linked").is_err());
        assert!(Session::list(&home).expect("list sessions").is_empty());
        fs::remove_file(path).expect("remove session symlink");
        fs::remove_file(target).expect("remove target session");
        fs::remove_dir_all(home).expect("remove temp home");

        let home = temporary_home();
        let lucy = home.join(".lucy");
        fs::create_dir(&lucy).expect("Lucy directory");
        let target = home.join("sessions-target");
        fs::create_dir(&target).expect("target sessions directory");
        symlink(&target, lucy.join("sessions")).expect("sessions directory symlink");
        assert!(Session::list(&home).is_err());
        fs::remove_file(lucy.join("sessions")).expect("remove sessions directory symlink");
        fs::remove_dir(target).expect("remove target sessions directory");
        fs::remove_dir(lucy).expect("remove Lucy directory");
        fs::remove_dir(home).expect("remove temp home");
    }

    #[test]
    fn resume_rejects_duplicate_header_as_an_invalid_record() {
        let home = temporary_home();
        let sessions = home.join(".lucy/sessions");
        fs::create_dir_all(&sessions).expect("sessions");
        let id = "duplicate-header";
        let environment = format!("LUCY_DUPLICATE_HEADER_{}", std::process::id());
        let secret = "provider-secret";
        std::env::set_var(&environment, secret);
        let header = format!(
            r#"{{"record":"session","version":1,"session_id":"{id}","created_at":1,"cwd":".","boot_system_prompt":"{secret}","boot_system_prompt":"safe","llm":{{"base_url":"http://localhost","model":"model","api_key_env":"{environment}"}}}}"#
        );
        fs::write(sessions.join(format!("{id}.jsonl")), format!("{header}\n"))
            .expect("duplicate header");

        let error = Session::resume(&home, id).expect_err("duplicate header should be rejected");
        assert_eq!(error.to_string(), "invalid session record at line 1");

        std::env::remove_var(environment);
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn creates_appends_resumes_and_lists_jsonl_session() {
        let home = temporary_home();
        let cwd = std::env::current_dir().expect("cwd");
        let llm = LlmSettings {
            base_url: "http://localhost:1234/api/v1".to_owned(),
            model: "test-model".to_owned(),
            api_key_env: "TEST_KEY".to_owned(),
        };
        let mut session =
            Session::create(&home, &cwd, "stable prompt".to_owned(), llm.clone()).expect("create");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(sessions_dir(&home))
                    .expect("sessions directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&session.path)
                    .expect("session file metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let id = session.id.clone();
        session
            .append_message(ChatMessage::user("first".to_owned()))
            .expect("append user");
        session
            .append_message(ChatMessage::assistant("last".to_owned(), Vec::new()))
            .expect("append assistant");

        let resumed = Session::resume(&home, &id).expect("resume");
        assert_eq!(resumed.boot_system_prompt, "stable prompt");
        assert_eq!(resumed.llm, llm);
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(resumed.cwd, fs::canonicalize(cwd).expect("canonical cwd"));
        let listed = Session::list(&home).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, id);
        assert!(listed[0]
            .first_message
            .as_deref()
            .is_some_and(|summary| summary.contains("first")));
        assert!(Session::resume(&home, "missing").is_err());

        let file = fs::read_to_string(resumed.path).expect("session file");
        assert!(file.lines().count() >= 3);
        assert!(!file.contains("TEST_KEY_VALUE"));
        fs::remove_dir_all(home).expect("remove temp home");
    }
}
