use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{
    now, open_session_for_append, record_contains_secret, session_record_rejected, write_record,
    Session, SessionError, SessionHistoryRecord, SessionRecord,
};

static TURN_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    pub first_message: usize,
    pub last_message: Option<usize>,
    pub error: Option<String>,
}

impl TurnState {
    pub fn is_pending(&self) -> bool {
        matches!(self.status, TurnStatus::Active | TurnStatus::Retryable)
    }
}

impl Session {
    pub fn start_turn(&mut self, kind: TurnKind) -> Result<String, SessionError> {
        if self.latest_turn().is_some_and(|turn| turn.is_pending()) {
            return Err(SessionError::new("session already has a pending turn"));
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

    pub fn set_turn_phase(
        &mut self,
        turn_id: &str,
        phase: TurnPhase,
    ) -> Result<(), SessionError> {
        let state = self.require_pending_turn(turn_id)?;
        if state.status != TurnStatus::Active {
            return Err(SessionError::new(
                "retryable turn must be resumed before changing phase",
            ));
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

    pub fn resume_retryable_turn(&mut self, turn_id: &str) -> Result<(), SessionError> {
        let state = self.require_pending_turn(turn_id)?;
        if state.status != TurnStatus::Retryable {
            return Err(SessionError::new("turn is not retryable"));
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
    ) -> Result<(), SessionError> {
        let state = self.require_pending_turn(turn_id)?;
        if state.status == TurnStatus::Retryable && outcome != TurnOutcome::Abandoned {
            return Err(SessionError::new(
                "retryable turn must be resumed or abandoned before finishing",
            ));
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

    pub fn fail_turn_retryably(
        &mut self,
        turn_id: &str,
        error: String,
    ) -> Result<(), SessionError> {
        let state = self.require_pending_turn(turn_id)?;
        if state.status != TurnStatus::Active {
            return Err(SessionError::new("turn is already retryable"));
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

    pub fn latest_turn(&self) -> Option<TurnState> {
        let mut latest = None;
        for lifecycle in self.history.iter().filter_map(|record| match record {
            SessionHistoryRecord::Turn { lifecycle } => Some(lifecycle),
            _ => None,
        }) {
            match lifecycle.event {
                TurnEvent::Started => {
                    let (Some(kind), Some(phase), Some(first_message)) =
                        (lifecycle.kind, lifecycle.phase, lifecycle.first_message)
                    else {
                        continue;
                    };
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
                    let Some(state) = latest.as_mut().filter(|state| {
                        state.turn_id == lifecycle.turn_id && state.is_pending()
                    }) else {
                        continue;
                    };
                    let Some(phase) = lifecycle.phase else {
                        continue;
                    };
                    state.phase = phase;
                    state.status = TurnStatus::Active;
                    state.error = None;
                }
                TurnEvent::Finished => {
                    let Some(state) = latest
                        .as_mut()
                        .filter(|state| state.turn_id == lifecycle.turn_id)
                    else {
                        continue;
                    };
                    let Some(outcome) = lifecycle.outcome else {
                        continue;
                    };
                    state.status = status_for_outcome(outcome);
                    state.last_message = lifecycle.last_message;
                    state.error = lifecycle.error.clone();
                }
            }
        }
        latest
    }

    fn require_pending_turn(&self, turn_id: &str) -> Result<TurnState, SessionError> {
        self.latest_turn()
            .filter(|turn| turn.turn_id == turn_id && turn.is_pending())
            .ok_or_else(|| SessionError::new("turn is not pending"))
    }

    fn append_turn_lifecycle(
        &mut self,
        lifecycle: TurnLifecycleRecord,
    ) -> Result<(), SessionError> {
        let record = SessionRecord::Turn {
            lifecycle: lifecycle.clone(),
        };
        if let Some(secret) = self.secret.as_deref() {
            if record_contains_secret(&record, secret) {
                return Err(session_record_rejected(secret));
            }
        }
        let mut file = open_session_for_append(&self.path)?;
        write_record(&mut file, &record)?;
        self.updated_at = lifecycle.timestamp;
        self.history
            .push(SessionHistoryRecord::Turn { lifecycle });
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::LlmSettings;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temporary_home() -> PathBuf {
        loop {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lucy-turn-{stamp}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temp home: {error}"),
            }
        }
    }

    fn create_session(home: &std::path::Path) -> Session {
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
    fn turn_lifecycle_round_trips_and_reconstructs_latest_state() {
        let home = temporary_home();
        let mut session = create_session(&home);
        let turn_id = session.start_turn(TurnKind::User).expect("start turn");
        session
            .set_turn_phase(&turn_id, TurnPhase::Compacting)
            .expect("phase");
        session
            .fail_turn_retryably(&turn_id, "provider unavailable".to_owned())
            .expect("retryable failure");

        let retryable = session.latest_turn().expect("latest turn");
        assert_eq!(retryable.turn_id, turn_id);
        assert_eq!(retryable.phase, TurnPhase::Compacting);
        assert_eq!(retryable.status, TurnStatus::Retryable);
        assert_eq!(retryable.error.as_deref(), Some("provider unavailable"));

        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.latest_turn(), Some(retryable));
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn pending_turn_must_be_resumed_or_abandoned_before_a_new_turn() {
        let home = temporary_home();
        let mut session = create_session(&home);
        let turn_id = session.start_turn(TurnKind::User).expect("start turn");
        session
            .fail_turn_retryably(&turn_id, "temporary".to_owned())
            .expect("retryable failure");
        assert!(session.start_turn(TurnKind::User).is_err());

        session
            .resume_retryable_turn(&turn_id)
            .expect("resume retryable turn");
        session
            .finish_turn(&turn_id, TurnOutcome::Completed, None)
            .expect("finish turn");
        assert_eq!(
            session.latest_turn().expect("latest turn").status,
            TurnStatus::Completed
        );
        assert!(session.start_turn(TurnKind::User).is_ok());
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn legacy_sessions_without_turn_records_remain_valid() {
        let home = temporary_home();
        let session = create_session(&home);
        let resumed = Session::resume(&home, &session.id).expect("resume");
        assert_eq!(resumed.latest_turn(), None);
        fs::remove_dir_all(home).expect("cleanup");
    }
}
