mod base;

use std::io;
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

fn trusted_openrouter_metadata(settings: &LlmSettings) -> bool {
    reqwest::Url::parse(&settings.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host.eq_ignore_ascii_case("openrouter.ai"))
}

/// Provider facade. Normal OpenRouter and Codex requests use deterministic
/// Lucy 2 projection when those providers expose a context window. Legacy
/// semantic-compaction helpers remain temporarily for compatibility tests.
pub struct Provider {
    inner: base::Provider,
    projection_context_window: Option<usize>,
}

impl Deref for Provider {
    type Target = base::Provider;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Provider {
    pub fn new(settings: &LlmSettings) -> Result<Self, ProviderError> {
        let inner = base::Provider::new(settings)?;
        let projection_context_window = if trusted_openrouter_metadata(settings) {
            inner.context_window()
        } else {
            None
        };
        Ok(Self {
            inner,
            projection_context_window,
        })
    }

    pub fn new_codex(home: &Path, settings: &LlmSettings) -> Result<Self, ProviderError> {
        let inner = base::Provider::new_codex(home, settings)?;
        let projection_context_window = inner.context_window();
        Ok(Self {
            inner,
            projection_context_window,
        })
    }

    pub(crate) fn with_session_id(self, session_id: &str) -> Self {
        Self {
            inner: self.inner.with_session_id(session_id),
            projection_context_window: self.projection_context_window,
        }
    }

    /// The legacy Harness field historically used this metadata to trigger an
    /// LLM-authored compaction turn. Projection owns context pressure now, so
    /// normal execution deliberately does not expose a compaction window here.
    /// A projection-aware UI can surface the stored window independently later.
    pub(crate) fn context_window(&self) -> Option<usize> {
        None
    }

    pub(crate) fn project_messages(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Vec<ChatMessage>, ProviderError> {
        let Some(context_window) = self.projection_context_window else {
            return Ok(messages.to_vec());
        };
        crate::projection::project_context(messages, context_window)
            .map(|projection| projection.messages)
            .map_err(ProviderError::new)
    }

    pub fn stream_chat(
        &self,
        messages: &[ChatMessage],
        on_delta: &mut dyn FnMut(&str) -> io::Result<()>,
    ) -> Result<ProviderTurn, ProviderError> {
        let messages = self.project_messages(messages)?;
        self.inner.stream_chat(&messages, on_delta)
    }

    pub(crate) fn stream_chat_cancellable_with_options_and_events(
        &self,
        messages: &[ChatMessage],
        on_event: &mut dyn FnMut(ProviderStreamEvent) -> io::Result<()>,
        cancellation: &CancellationToken,
        include_tools: bool,
    ) -> Result<ProviderTurn, ProviderError> {
        let messages = self.project_messages(messages)?;
        self.inner.stream_chat_cancellable_with_options_and_events(
            &messages,
            on_event,
            cancellation,
            include_tools,
        )
    }

    pub(crate) fn summarize_prepared(
        &self,
        planned: Vec<ChatMessage>,
        context_window: Option<usize>,
        cancellation: &CancellationToken,
    ) -> Result<String, ProviderError> {
        let attempts = crate::compaction_fallback::summary_attempts(planned, context_window)
            .map_err(ProviderError::new)?;
        let mut attempts = attempts.into_iter().peekable();
        while let Some(attempt) = attempts.next() {
            match self.inner.summarize(&attempt, cancellation) {
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

    #[test]
    fn only_openrouter_gets_generic_provider_projection_metadata() {
        let mut settings = LlmSettings {
            base_url: "https://openrouter.ai/api/v1".to_owned(),
            model: "test-model".to_owned(),
            api_key_env: "OPENROUTER_API_KEY".to_owned(),
            effort: None,
        };
        assert!(trusted_openrouter_metadata(&settings));

        settings.base_url = "https://example.test/v1".to_owned();
        assert!(!trusted_openrouter_metadata(&settings));
    }
}
