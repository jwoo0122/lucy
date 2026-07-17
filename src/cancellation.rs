use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

const ACTIVE: u8 = 0;
const CANCELLED: u8 = 1;
const COMPLETED: u8 = 2;

/// A small cancellation signal shared by the interactive frontend and the
/// provider/command work running for the active turn.
#[derive(Clone, Debug)]
pub(crate) struct CancellationToken {
    state: Arc<AtomicU8>,
}

impl CancellationToken {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(ACTIVE)),
        }
    }

    /// Request cancellation. A completed turn cannot be retroactively changed
    /// into an interruption.
    pub(crate) fn cancel(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == CANCELLED
    }

    /// Atomically choose normal completion over a concurrent cancellation.
    pub(crate) fn try_complete(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, COMPLETED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_and_cancellation_have_one_linearization_winner() {
        let completed = CancellationToken::new();
        assert!(completed.try_complete());
        completed.cancel();
        assert!(!completed.is_cancelled());
        assert!(!completed.try_complete());

        let canceled = CancellationToken::new();
        canceled.cancel();
        assert!(canceled.is_cancelled());
        assert!(!canceled.try_complete());
    }
}
