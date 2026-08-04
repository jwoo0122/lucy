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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderFailureKind {
    Cancelled,
    ContextOverflow,
    Timeout,
    RateLimited,
    Authentication,
    InvalidRequest,
    Transient,
    Other,
}

impl ProviderError {
    pub(crate) fn kind(&self) -> ProviderFailureKind {
        if self.is_cancelled() {
            return ProviderFailureKind::Cancelled;
        }
        let message = self.to_string().to_ascii_lowercase();
        if contains_context_overflow(&message) {
            return ProviderFailureKind::ContextOverflow;
        }
        if message.contains("http status 401") || message.contains("http status 403") {
            return ProviderFailureKind::Authentication;
        }
        if message.contains("http status 429") {
            return ProviderFailureKind::RateLimited;
        }
        if message.contains("(timeout)") || message.contains("http status 408") {
            return ProviderFailureKind::Timeout;
        }
        if message.contains("http status 400")
            || message.contains("http status 404")
            || message.contains("http status 405")
            || message.contains("http status 422")
            || message.contains("unsupported")
            || message.contains("invalid request")
        {
            return ProviderFailureKind::InvalidRequest;
        }
        if message.contains("(connection)")
            || message.contains("(body)")
            || message.contains("(decode)")
            || message.contains("http status 500")
            || message.contains("http status 502")
            || message.contains("http status 503")
            || message.contains("http status 504")
        {
            return ProviderFailureKind::Transient;
        }
        ProviderFailureKind::Other
    }
}

fn contains_context_overflow(message: &str) -> bool {
    [
        "context window",
        "context length",
        "maximum context",
        "max context",
        "too many tokens",
        "request too large",
        "input is too long",
        "exceeds the model context",
        "http status 413",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

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
        let attempts = crate::compaction_fallback::summary_attempts(
            planned,
            self.0.context_window(),
        )
        .map_err(ProviderError::new)?;
        let mut attempts = attempts.into_iter().peekable();
        while let Some(attempt) = attempts.next() {
            match self.0.summarize(&attempt, cancellation) {
                Err(error)
                    if error.kind() == ProviderFailureKind::ContextOverflow
                        && attempts.peek().is_some() =>
                {
                    continue;
                }
                result => return result,
            }
        }
        Err(ProviderError::new(
            "compaction exhausted every bounded provider attempt",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_provider_failures_for_recovery() {
        assert_eq!(
            ProviderError::new("request exceeds the model context window").kind(),
            ProviderFailureKind::ContextOverflow
        );
        assert_eq!(
            ProviderError::new("provider returned HTTP status 429").kind(),
            ProviderFailureKind::RateLimited
        );
        assert_eq!(
            ProviderError::new("provider request failed (timeout)").kind(),
            ProviderFailureKind::Timeout
        );
        assert_eq!(
            ProviderError::new("provider returned HTTP status 401").kind(),
            ProviderFailureKind::Authentication
        );
        assert_eq!(
            ProviderError::new("provider request failed (connection)").kind(),
            ProviderFailureKind::Transient
        );
    }
}
