mod base;
mod turn;

use std::ops::{Deref, DerefMut};
use std::path::Path;

use crate::config::LlmSettings;
use crate::context::SkillEntry;
use crate::model::{ChatMessage, OBSERVATION_ROLE};

pub use base::{
    sessions_dir, validate_session_id, CompactionRecord, InterruptionRecord, SessionHistoryRecord,
    SessionMetadata, SessionToolResult,
};
pub use turn::{
    TurnEvent, TurnKind, TurnLifecycleRecord, TurnOutcome, TurnPhase, TurnState, TurnStatus,
};

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

impl From<base::SessionError> for SessionError {
    fn from(error: base::SessionError) -> Self {
        Self(error.to_string())
    }
}

/// A thin compatibility wrapper around Lucy's existing append-only session
/// implementation. It intercepts semantic transcript boundaries while
/// preserving the established private JSONL format and public field access.
#[derive(Debug, Clone)]
pub struct Session(base::Session);

impl Deref for Session {
    type Target = base::Session;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Session {
    pub fn create(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
    ) -> Result<Self, SessionError> {
        base::Session::create(home, cwd, boot_system_prompt, llm)
            .map(Self)
            .map_err(Into::into)
    }

    pub fn create_with_secret(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
        secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        base::Session::create_with_secret(home, cwd, boot_system_prompt, llm, secret)
            .map(Self)
            .map_err(Into::into)
    }

    pub fn create_with_skills_and_secret(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
        skills: Vec<SkillEntry>,
        secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        base::Session::create_with_skills_and_secret(
            home,
            cwd,
            boot_system_prompt,
            llm,
            skills,
            secret,
        )
        .map(Self)
        .map_err(Into::into)
    }

    pub fn resume(home: &Path, id: &str) -> Result<Self, SessionError> {
        base::Session::resume(home, id)
            .map(Self)
            .map_err(Into::into)
    }

    pub fn resume_with_secret(
        home: &Path,
        id: &str,
        external_secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        base::Session::resume_with_secret(home, id, external_secret)
            .map(Self)
            .map_err(Into::into)
    }

    pub fn list(home: &Path) -> Result<Vec<SessionMetadata>, SessionError> {
        base::Session::list(home).map_err(Into::into)
    }

    pub fn list_with_secret(
        home: &Path,
        external_secret: Option<&str>,
    ) -> Result<Vec<SessionMetadata>, SessionError> {
        base::Session::list_with_secret(home, external_secret).map_err(Into::into)
    }

    /// Append a semantic message while maintaining one explicit logical turn.
    /// `!retry` is a control input: it resumes the existing turn without
    /// becoming another provider-visible user message.
    pub fn append_message(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        if message.role == "user" {
            return self.append_user_or_retry(message);
        }

        let starts_background = message.role == OBSERVATION_ROLE
            && self
                .latest_turn()
                .map_err(SessionError::new)?
                .is_none_or(|turn| !turn.is_pending());
        if starts_background {
            self.start_turn(TurnKind::Background)
                .map_err(SessionError::new)?;
        }

        self.0
            .append_message(message.clone())
            .map_err(SessionError::from)?;

        let Some(turn) = self
            .latest_turn()
            .map_err(SessionError::new)?
            .filter(|turn| turn.is_pending())
        else {
            return Ok(());
        };

        if message.role == "assistant" {
            if message.tool_calls.is_empty() {
                self.finish_turn(&turn.turn_id, TurnOutcome::Completed, None)
                    .map_err(SessionError::new)?;
            } else {
                self.set_turn_phase(&turn.turn_id, TurnPhase::ExecutingTools)
                    .map_err(SessionError::new)?;
            }
        } else if message.role == "tool" {
            self.set_turn_phase(&turn.turn_id, TurnPhase::ProviderStream)
                .map_err(SessionError::new)?;
        }
        Ok(())
    }

    pub fn append_interruption(
        &mut self,
        interruption: InterruptionRecord,
    ) -> Result<(), SessionError> {
        self.0
            .append_interruption(interruption)
            .map_err(SessionError::from)?;
        if let Some(turn) = self.latest_turn().map_err(SessionError::new)? {
            if turn.is_pending() {
                self.finish_turn(&turn.turn_id, TurnOutcome::Interrupted, None)
                    .map_err(SessionError::new)?;
            }
        }
        Ok(())
    }

    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_message: usize,
        tokens_before: usize,
    ) -> Result<(), SessionError> {
        self.0
            .append_compaction(summary, first_kept_message, tokens_before)
            .map_err(SessionError::from)?;
        if let Some(turn) = self
            .latest_turn()
            .map_err(SessionError::new)?
            .filter(|turn| turn.is_pending())
        {
            self.set_turn_phase(&turn.turn_id, TurnPhase::ProviderStream)
                .map_err(SessionError::new)?;
        }
        Ok(())
    }

    fn append_user_or_retry(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        let pending = self
            .latest_turn()
            .map_err(SessionError::new)?
            .filter(|turn| turn.is_pending());
        let is_retry = message.content.as_deref().map(str::trim) == Some("!retry");

        if let Some(turn) = pending {
            if !is_retry {
                return Err(SessionError::new(
                    "session has an unresolved turn; send !retry before starting a new message",
                ));
            }
            if turn.status == TurnStatus::Active {
                self.fail_turn_retryably(
                    &turn.turn_id,
                    "previous turn stopped before completion".to_owned(),
                )
                .map_err(SessionError::new)?;
            }
            self.resume_retryable_turn(&turn.turn_id)
                .map_err(SessionError::new)?;
            return Ok(());
        }
        if is_retry {
            return Err(SessionError::new("session has no pending turn to retry"));
        }

        let turn_id = self.start_turn(TurnKind::User).map_err(SessionError::new)?;
        if let Err(error) = self.0.append_message(message) {
            let _ = self.finish_turn(
                &turn_id,
                TurnOutcome::TerminalFailure,
                Some(error.to_string()),
            );
            return Err(error.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod wrapper_tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn session() -> (std::path::PathBuf, Session) {
        let home = std::env::temp_dir().join(format!(
            "lucy-session-wrapper-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&home).expect("home");
        let session = Session::create_with_secret(
            &home,
            &std::env::current_dir().expect("cwd"),
            "prompt".to_owned(),
            LlmSettings {
                base_url: "http://localhost".to_owned(),
                model: "model".to_owned(),
                api_key_env: "LUCY_WRAPPER_TEST_KEY".to_owned(),
                effort: None,
            },
            None,
        )
        .expect("session");
        (home, session)
    }

    #[test]
    fn retry_control_does_not_append_a_second_user_message() {
        let (home, mut session) = session();
        session
            .append_message(ChatMessage::user("original".to_owned()))
            .expect("original");
        assert!(session
            .append_message(ChatMessage::user("replacement".to_owned()))
            .is_err());
        session
            .append_message(ChatMessage::user("!retry".to_owned()))
            .expect("retry");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content.as_deref(), Some("original"));
        assert_eq!(
            session.latest_turn().expect("state").expect("turn").status,
            TurnStatus::Active
        );
        fs::remove_dir_all(home).expect("cleanup");
    }

    #[test]
    fn final_assistant_message_completes_the_tracked_turn() {
        let (home, mut session) = session();
        session
            .append_message(ChatMessage::user("work".to_owned()))
            .expect("user");
        session
            .append_message(ChatMessage::assistant("done".to_owned(), Vec::new()))
            .expect("assistant");
        assert_eq!(
            session.latest_turn().expect("state").expect("turn").status,
            TurnStatus::Completed
        );
        fs::remove_dir_all(home).expect("cleanup");
    }
}
