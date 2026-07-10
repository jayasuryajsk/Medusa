//! Mid-turn cancellation: a shared token that every blocking site in the
//! turn pipeline polls. An `AtomicBool` suffices (no Condvar) because each
//! blocking operation is converted to a ≤100ms poll, so cancellation latency
//! is bounded by poll granularity, not by wakeups.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use color_eyre::eyre::{Report, Result};

/// Cooperative cancellation flag for one model turn. Cloning shares the flag,
/// so every `ToolRuntime` clone (parallel read-only threads, explore probes,
/// workflow subagents) observes the same cancel. The default token is never
/// cancelled.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent; never un-cancels.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Checkpoint: return a [`TurnCancelled`] error once cancellation was
    /// requested, so the turn unwinds through ordinary `?` propagation.
    pub fn bail_if_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(Report::new(TurnCancelled));
        }
        Ok(())
    }
}

/// Unit error raised when the user interrupts a turn. Detected anywhere up
/// the stack via [`error_is_cancellation`], so wrap_err layers added during
/// propagation don't hide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnCancelled;

impl std::fmt::Display for TurnCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("turn cancelled by user")
    }
}

impl std::error::Error for TurnCancelled {}

/// True when `error` is (or wraps, at any depth) a [`TurnCancelled`] —
/// user-initiated interruption, not a real failure.
pub fn error_is_cancellation(error: &Report) -> bool {
    error.chain().any(|cause| cause.is::<TurnCancelled>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre::WrapErr;

    #[test]
    fn default_token_never_cancels() {
        let token = CancelToken::default();
        assert!(!token.is_cancelled());
        token.bail_if_cancelled().unwrap();
    }

    #[test]
    fn cancel_flips_the_flag_for_every_clone() {
        let token = CancelToken::new();
        let clone = token.clone();

        clone.cancel();

        assert!(token.is_cancelled());
        assert!(clone.is_cancelled());
        let error = token.bail_if_cancelled().unwrap_err();
        assert!(error_is_cancellation(&error));
    }

    #[test]
    fn wrapped_cancellation_errors_are_still_detected() {
        let token = CancelToken::new();
        token.cancel();

        let error = token
            .bail_if_cancelled()
            .wrap_err("stream turn failed")
            .wrap_err("chat failed")
            .unwrap_err();

        assert!(error_is_cancellation(&error));
    }

    #[test]
    fn ordinary_errors_are_not_cancellations() {
        let error = color_eyre::eyre::eyre!("backend returned 500");
        assert!(!error_is_cancellation(&error));
    }
}
