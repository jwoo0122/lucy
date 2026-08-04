mod base;
mod turn;

use std::ops::{Deref, DerefMut};
use std::path::Path;

use crate::config::LlmSettings;
use crate::context::SkillEntry;

pub use base::{
    sessions_dir, validate_session_id, CompactionRecord, InterruptionRecord, SessionError,
    SessionHistoryRecord, SessionMetadata, SessionToolResult,
};
pub use turn::{
    TurnEvent, TurnKind, TurnLifecycleRecord, TurnOutcome, TurnPhase, TurnState, TurnStatus,
};

/// A thin compatibility wrapper around Lucy's existing append-only session
/// implementation. The wrapper provides durable turn lifecycle state while
/// preserving the established transcript format and public field access.
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
        base::Session::create(home, cwd, boot_system_prompt, llm).map(Self)
    }

    pub fn create_with_secret(
        home: &Path,
        cwd: &Path,
        boot_system_prompt: String,
        llm: LlmSettings,
        secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        base::Session::create_with_secret(home, cwd, boot_system_prompt, llm, secret).map(Self)
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
    }

    pub fn resume(home: &Path, id: &str) -> Result<Self, SessionError> {
        base::Session::resume(home, id).map(Self)
    }

    pub fn resume_with_secret(
        home: &Path,
        id: &str,
        external_secret: Option<&str>,
    ) -> Result<Self, SessionError> {
        base::Session::resume_with_secret(home, id, external_secret).map(Self)
    }

    pub fn list(home: &Path) -> Result<Vec<SessionMetadata>, SessionError> {
        base::Session::list(home)
    }

    pub fn list_with_secret(
        home: &Path,
        external_secret: Option<&str>,
    ) -> Result<Vec<SessionMetadata>, SessionError> {
        base::Session::list_with_secret(home, external_secret)
    }
}
