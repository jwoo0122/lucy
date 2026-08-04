mod base;

use std::ops::Deref;
use std::path::Path;

use crate::cancellation::CancellationToken;
use crate::config::LlmSettings;
use crate::model::ChatMessage;

pub(crate) use base::ProviderStreamEvent;
pub use base::{
    parse_sse, ProviderError, ProviderModel, ProviderTurn, SseParseResult, PROVIDER_TIMEOUT,
};

/// Provider facade that keeps normal request behavior in the established
/// implementation while routing compaction through a provider-neutral plan.
pub struct Provider(base::Provider);

impl Deref for Provider {
    type Target = base::Provider;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Provider {
    pub fn new(settings: &LlmSettings) -> Result<Self, ProviderError> {
        base::Provider::new(settings).map(Self)
    }

    pub fn new_codex(home: &Path, settings: &LlmSettings) -> Result<Self, ProviderError> {
        base::Provider::new_codex(home, settings).map(Self)
    }

    pub(crate) fn with_session_id(self, session_id: &str) -> Self {
        Self(self.0.with_session_id(session_id))
    }

    /// Summarize only the history selected for removal. The retained tail is
    /// deliberately absent from this request and remains verbatim in the
    /// ordinary provider context after the resulting boundary is committed.
    pub(crate) fn summarize(
        &self,
        messages: &[ChatMessage],
        cancellation: &CancellationToken,
    ) -> Result<String, ProviderError> {
        let planned =
            crate::compaction::prepare_summary_messages(messages).map_err(ProviderError::new)?;
        self.0.summarize(&planned, cancellation)
    }
}
