#[cfg(any(test, not(unix)))]
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::Session;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

static TURN_COUNTER: AtomicU64 = AtomicU64::new(0);
const TURN_JOURNAL_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    User,
    Background,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnEvent {
    Started,
    PhaseChanged,
    Finished,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Accepted,
    Compacting,
    ProviderStream,
    ExecutingTools,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    RetryableFailure,
    TerminalFailure,
    Interrupted,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Active,
    Retryable,
    Completed,
    TerminalFailure,
    Interrupted,
    Abandoned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnLifecycleRecord {
    pub timestamp: u64,
    pub turn_id: String,
    pub event: TurnEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TurnKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<TurnPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_message: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnState {
    pub turn_id: String,
    pub kind: TurnKind,
    pub phase: TurnPhase,
    pub status: TurnStatus,
    /// Inclusive ordinal of the first transcript message owned by the turn.
    pub first_message: usize,
    /// Exclusive ordinal after the last transcript message owned by the turn.
    pub last_message: Option<usize>,
    pub error: Option<String>,
}

impl TurnState {
    pub fn is_pending(&self) -> bool {
        matches!(self.status, TurnStatus::Active | TurnStatus::Retryable)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StoredTurnRecord {
    record: String,
    version: u8,
    #[serde(flatten)]
    lifecycle: TurnLifecycleRecord,
}

impl StoredTurnRecord {
    fn new(lifecycle: TurnLifecycleRecord) -> Self {
        Self {
            record: "turn".to_owned(),
            version: TURN_JOURNAL_VERSION,
            lifecycle,
        }
    }
}

impl Session {
    pub fn start_turn(&mut self, kind: TurnKind) -> Result<String, String> {
        if self.latest_turn()?.is_some_and(|turn| turn.is_pending()) {
            return Err("session already has a pending turn".to_owned());
        }
        let turn_id = new_turn_id(&self.id);
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: now(),
            turn_id: turn_id.clone(),
            event: TurnEvent::Started,
            kind: Some(kind),
            phase: Some(TurnPhase::Accepted),
            outcome: None,
            first_message: Some(self.messages.len()),
            last_message: None,
            error: None,
        })?;
        Ok(turn_id)
    }

    pub fn set_turn_phase(&mut self, turn_id: &str, phase: TurnPhase) -> Result<(), String> {
        let state = self.require_pending_turn(turn_id)?;
        if state.status != TurnStatus::Active {
            return Err("retryable turn must be resumed before changing phase".to_owned());
        }
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: now(),
            turn_id: turn_id.to_owned(),
            event: TurnEvent::PhaseChanged,
            kind: None,
            phase: Some(phase),
            outcome: None,
            first_message: None,
            last_message: None,
            error: None,
        })
    }

    pub fn resume_retryable_turn(&mut self, turn_id: &str) -> Result<(), String> {
        let state = self.require_pending_turn(turn_id)?;
        if !matches!(state.status, TurnStatus::Active | TurnStatus::Retryable) {
            return Err("turn is not retryable".to_owned());
        }
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: now(),
            turn_id: turn_id.to_owned(),
            event: TurnEvent::PhaseChanged,
            kind: None,
            phase: Some(TurnPhase::Accepted),
            outcome: None,
            first_message: None,
            last_message: None,
            error: None,
        })
    }

    pub fn finish_turn(
        &mut self,
        turn_id: &str,
        outcome: TurnOutcome,
        error: Option<String>,
    ) -> Result<(), String> {
        let state = self.require_pending_turn(turn_id)?;
        if outcome == TurnOutcome::RetryableFailure {
            return Err("use fail_turn_retryably for a retryable failure".to_owned());
        }
        if state.status == TurnStatus::Retryable && outcome != TurnOutcome::Abandoned {
            return Err("retryable turn must be resumed or abandoned before finishing".to_owned());
        }
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: now(),
            turn_id: turn_id.to_owned(),
            event: TurnEvent::Finished,
            kind: None,
            phase: None,
            outcome: Some(outcome),
            first_message: None,
            last_message: Some(self.messages.len()),
            error,
        })
    }

    pub fn fail_turn_retryably(&mut self, turn_id: &str, error: String) -> Result<(), String> {
        let state = self.require_pending_turn(turn_id)?;
        if state.status != TurnStatus::Active {
            return Err("turn is already retryable".to_owned());
        }
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: now(),
            turn_id: turn_id.to_owned(),
            event: TurnEvent::Finished,
            kind: None,
            phase: None,
            outcome: Some(TurnOutcome::RetryableFailure),
            first_message: None,
            last_message: Some(self.messages.len()),
            error: Some(error),
        })
    }

    pub fn latest_turn(&self) -> Result<Option<TurnState>, String> {
        reduce_turn_records(&load_turn_records(&turn_journal_path(&self.path))?)
    }

    pub fn turn_lifecycle(&self) -> Result<Vec<TurnLifecycleRecord>, String> {
        load_turn_records(&turn_journal_path(&self.path))
    }

    fn require_pending_turn(&self, turn_id: &str) -> Result<TurnState, String> {
        self.latest_turn()?
            .filter(|turn| turn.turn_id == turn_id && turn.is_pending())
            .ok_or_else(|| "turn is not pending".to_owned())
    }

    fn append_turn_lifecycle(&mut self, lifecycle: TurnLifecycleRecord) -> Result<(), String> {
        let path = turn_journal_path(&self.path);
        let mut records = load_turn_records(&path)?;
        records.push(lifecycle.clone());
        reduce_turn_records(&records)?;

        let stored = StoredTurnRecord::new(lifecycle);
        let mut encoded = serde_json::to_string(&stored)
            .map_err(|error| format!("unable to encode turn lifecycle record: {error}"))?;
        let secret = std::env::var(&self.llm.api_key_env).ok();
        if secret
            .as_deref()
            .is_some_and(|secret| !secret.is_empty() && encoded.contains(secret))
        {
            return Err("turn lifecycle record rejected".to_owned());
        }
        encoded.push('\n');

        let mut file = open_turn_journal_for_append(&path)?;
        file.write_all(encoded.as_bytes())
            .map_err(|_| "unable to write turn lifecycle journal".to_owned())?;
        file.flush()
            .map_err(|_| "unable to write turn lifecycle journal".to_owned())?;
        Ok(())
    }
}

fn turn_journal_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("turns")
}

fn open_turn_journal_for_append(path: &Path) -> Result<File, String> {
    #[cfg(not(unix))]
    reject_symlink(path)?;

    let mut options = OpenOptions::new();
    options.write(true).append(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| "unable to open turn lifecycle journal".to_owned())?;
    validate_private_regular_file(&file)?;
    Ok(file)
}

fn open_turn_journal_for_read(path: &Path) -> Result<Option<File>, String> {
    #[cfg(not(unix))]
    reject_symlink(path)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    match options.open(path) {
        Ok(file) => {
            validate_private_regular_file(&file)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("unable to read turn lifecycle journal".to_owned()),
    }
}

#[cfg(not(unix))]
fn reject_symlink(path: &Path) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err("turn lifecycle journal is unsafe".to_owned());
        }
    }
    Ok(())
}

fn validate_private_regular_file(file: &File) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|_| "unable to inspect turn lifecycle journal".to_owned())?;
    if !metadata.is_file() {
        return Err("turn lifecycle journal is unsafe".to_owned());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err("turn lifecycle journal is not private".to_owned());
    }
    Ok(())
}

fn load_turn_records(path: &Path) -> Result<Vec<TurnLifecycleRecord>, String> {
    let Some(file) = open_turn_journal_for_read(path)? else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    for (line_number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|_| "unable to read turn lifecycle journal".to_owned())?;
        if line.trim().is_empty() {
            continue;
        }
        let stored: StoredTurnRecord = serde_json::from_str(&line)
            .map_err(|_| format!("invalid turn lifecycle record at line {}", line_number + 1))?;
        if stored.record != "turn" || stored.version != TURN_JOURNAL_VERSION {
            return Err(format!(
                "unsupported turn lifecycle record at line {}",
                line_number + 1
            ));
        }
        records.push(stored.lifecycle);
    }
    Ok(records)
}

fn reduce_turn_records(records: &[TurnLifecycleRecord]) -> Result<Option<TurnState>, String> {
    let mut latest: Option<TurnState> = None;
    for lifecycle in records {
        match lifecycle.event {
            TurnEvent::Started => {
                if latest.as_ref().is_some_and(TurnState::is_pending) {
                    return Err("invalid turn lifecycle sequence".to_owned());
                }
                let (Some(kind), Some(phase), Some(first_message)) =
                    (lifecycle.kind, lifecycle.phase, lifecycle.first_message)
                else {
                    return Err("invalid turn start record".to_owned());
                };
                if lifecycle.outcome.is_some()
                    || lifecycle.last_message.is_some()
                    || lifecycle.error.is_some()
                {
                    return Err("invalid turn start record".to_owned());
                }
                latest = Some(TurnState {
                    turn_id: lifecycle.turn_id.clone(),
                    kind,
                    phase,
                    status: TurnStatus::Active,
                    first_message,
                    last_message: None,
                    error: None,
                });
            }
            TurnEvent::PhaseChanged => {
                let Some(state) = latest
                    .as_mut()
                    .filter(|state| state.turn_id == lifecycle.turn_id && state.is_pending())
                else {
                    return Err("invalid turn phase record".to_owned());
                };
                let Some(phase) = lifecycle.phase else {
                    return Err("invalid turn phase record".to_owned());
                };
                if lifecycle.kind.is_some()
                    || lifecycle.outcome.is_some()
                    || lifecycle.first_message.is_some()
                    || lifecycle.last_message.is_some()
                    || lifecycle.error.is_some()
                {
                    return Err("invalid turn phase record".to_owned());
                }
                state.phase = phase;
                state.status = TurnStatus::Active;
                state.error = None;
            }
            TurnEvent::Finished => {
                let Some(state) = latest
                    .as_mut()
                    .filter(|state| state.turn_id == lifecycle.turn_id && state.is_pending())
                else {
                    return Err("invalid turn finish record".to_owned());
                };
                let Some(outcome) = lifecycle.outcome else {
                    return Err("invalid turn finish record".to_owned());
                };
                if lifecycle.kind.is_some()
                    || lifecycle.phase.is_some()
                    || lifecycle.first_message.is_some()
                {
                    return Err("invalid turn finish record".to_owned());
                }
                if state.status == TurnStatus::Retryable && outcome != TurnOutcome::Abandoned {
                    return Err("invalid retryable turn transition".to_owned());
                }
                state.status = status_for_outcome(outcome);
                state.last_message = lifecycle.last_message;
                state.error = lifecycle.error.clone();
            }
        }
    }
    Ok(latest)
}

fn status_for_outcome(outcome: TurnOutcome) -> TurnStatus {
    match outcome {
        TurnOutcome::Completed => TurnStatus::Completed,
        TurnOutcome::RetryableFailure => TurnStatus::Retryable,
        TurnOutcome::TerminalFailure => TurnStatus::TerminalFailure,
        TurnOutcome::Interrupted => TurnStatus::Interrupted,
        TurnOutcome::Abandoned => TurnStatus::Abandoned,
    }
}

fn new_turn_id(session_id: &str) -> String {
    let counter = TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{session_id}-turn-{}-{counter}", now())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::config::LlmSettings;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temporary_home() -> PathBuf {
        loop {
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lucy-turn-{}-{}-{counter}",
                now(),
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temp home: {error}"),
            }
        }
    }

    fn create_session(home: &Path) -> Session {
        Session::create_with_secret(
            home,
            &std::env::current_dir().expect("cwd"),
            "prompt".to_owned(),
            LlmSettings {
                base_url: "http://localhost".to_owned(),
                model: "model".to_owned(),
                api_key_env: "LUCY_TURN_TEST_KEY".to_owned(),
                effort: None,
            },
            None,
        )
        .expect("session")
    }

    #[test]
    fn lifecycle_round_trips_and_reconstructs_retryable_state() {
        let home = temporary_home();
        let mut session = create_session(&home);
        let turn_id = session.start_turn(TurnKind::User).expect("start");
        session
            .set_turn_phase(&turn_id, TurnPhase::Compacting)
            .expect("phase");
        session
            .fail_turn_retryably(&turn_id, "provider unavailable".to_owned())
            .expect("failure");

        let expected = session.latest_turn().expect("read").expect("turn");
        assert_eq!(expected.status, TurnStatus::Retryable);
        assert_eq!(expected.error.as_deref(), Some("provider unavailable"));

        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.latest_turn().expect("read"), Some(expected));
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn pending_turn_blocks_a_second_turn_until_resolved() {
        let home = temporary_home();
        let mut session = create_session(&home);
        let turn_id = session.start_turn(TurnKind::User).expect("start");
        assert!(session.start_turn(TurnKind::User).is_err());
        session
            .finish_turn(&turn_id, TurnOutcome::Completed, None)
            .expect("finish");
        assert!(session.start_turn(TurnKind::User).is_ok());
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn legacy_session_without_lifecycle_journal_remains_valid() {
        let home = temporary_home();
        let session = create_session(&home);
        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.latest_turn().expect("read"), None);
        fs::remove_dir_all(home).expect("cleanup");
    }
}
