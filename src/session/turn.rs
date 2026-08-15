use serde::{Deserialize, Serialize};

use super::Session;
use crate::journal::JournalEvent;

pub(crate) const TURN_LIFECYCLE_KIND: &str = "turn_lifecycle";

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
    pub fn start_turn(&mut self, kind: TurnKind) -> Result<String, String> {
        self.ensure_attention()?;
        if self.latest_turn()?.is_some_and(|turn| turn.is_pending()) {
            return Err("Lucy already has a pending turn".to_owned());
        }
        let turn_id = new_turn_id()?;
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: 0,
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
            timestamp: 0,
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
        self.ensure_attention()?;
        let state = self.require_pending_turn(turn_id)?;
        if !matches!(state.status, TurnStatus::Active | TurnStatus::Retryable) {
            return Err("turn is not retryable".to_owned());
        }
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: 0,
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
            timestamp: 0,
            turn_id: turn_id.to_owned(),
            event: TurnEvent::Finished,
            kind: None,
            phase: None,
            outcome: Some(outcome),
            first_message: None,
            last_message: Some(self.messages.len()),
            error,
        })?;
        self.release_attention();
        Ok(())
    }

    pub fn fail_turn_retryably(&mut self, turn_id: &str, error: String) -> Result<(), String> {
        let state = self.require_pending_turn(turn_id)?;
        if state.status != TurnStatus::Active {
            return Err("turn is already retryable".to_owned());
        }
        self.append_turn_lifecycle(TurnLifecycleRecord {
            timestamp: 0,
            turn_id: turn_id.to_owned(),
            event: TurnEvent::Finished,
            kind: None,
            phase: None,
            outcome: Some(TurnOutcome::RetryableFailure),
            first_message: None,
            last_message: Some(self.messages.len()),
            error: Some(error),
        })?;
        self.release_attention();
        Ok(())
    }

    pub fn latest_turn(&self) -> Result<Option<TurnState>, String> {
        reduce_turn_records(&self.turn_lifecycle)
    }

    pub fn turn_lifecycle(&self) -> Result<Vec<TurnLifecycleRecord>, String> {
        Ok(self.turn_lifecycle.clone())
    }

    fn require_pending_turn(&self, turn_id: &str) -> Result<TurnState, String> {
        self.latest_turn()?
            .filter(|turn| turn.turn_id == turn_id && turn.is_pending())
            .ok_or_else(|| "turn is not pending".to_owned())
    }

    fn append_turn_lifecycle(&mut self, mut lifecycle: TurnLifecycleRecord) -> Result<(), String> {
        let mut event = JournalEvent::new(
            TURN_LIFECYCLE_KIND,
            serde_json::to_value(&lifecycle)
                .map_err(|_| "unable to encode turn lifecycle record".to_owned())?,
        )?;
        lifecycle.timestamp = event.timestamp_ms / 1000;
        event.payload = serde_json::to_value(&lifecycle)
            .map_err(|_| "unable to encode turn lifecycle record".to_owned())?;
        event.turn_id = Some(lifecycle.turn_id.clone());
        self.decorate_event(&mut event);
        self.journal().append(&event)?;
        let mut records = self.turn_lifecycle.clone();
        records.push(lifecycle.clone());
        reduce_turn_records(&records)?;
        self.turn_lifecycle.push(lifecycle);
        self.updated_at = event.timestamp_ms / 1000;
        Ok(())
    }
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
                state.status = match outcome {
                    TurnOutcome::Completed => TurnStatus::Completed,
                    TurnOutcome::RetryableFailure => TurnStatus::Retryable,
                    TurnOutcome::TerminalFailure => TurnStatus::TerminalFailure,
                    TurnOutcome::Interrupted => TurnStatus::Interrupted,
                    TurnOutcome::Abandoned => TurnStatus::Abandoned,
                };
                state.last_message = lifecycle.last_message;
                state.error = lifecycle.error.clone();
            }
        }
    }
    Ok(latest)
}

fn new_turn_id() -> Result<String, String> {
    let mut random = [0u8; 12];
    getrandom::fill(&mut random).map_err(|_| "unable to generate turn id".to_owned())?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("turn-{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_reconstructs_completed_turn() {
        let records = vec![
            TurnLifecycleRecord {
                timestamp: 1,
                turn_id: "turn-1".to_owned(),
                event: TurnEvent::Started,
                kind: Some(TurnKind::User),
                phase: Some(TurnPhase::Accepted),
                outcome: None,
                first_message: Some(0),
                last_message: None,
                error: None,
            },
            TurnLifecycleRecord {
                timestamp: 2,
                turn_id: "turn-1".to_owned(),
                event: TurnEvent::Finished,
                kind: None,
                phase: None,
                outcome: Some(TurnOutcome::Completed),
                first_message: None,
                last_message: Some(2),
                error: None,
            },
        ];
        let state = reduce_turn_records(&records).expect("state").expect("turn");
        assert_eq!(state.status, TurnStatus::Completed);
        assert_eq!(state.last_message, Some(2));
    }
}
