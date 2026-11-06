//! Interception-layer errors.

/// Errors surfaced by the interception API.
///
/// The [`Self::Call`] variant is boxed so the enum stays small
/// (`size_of::<usize>()` for the heavy arm). Without boxing, every
/// `Result<T, InterceptionError>` would carry the full
/// `zendriver_transport::CallError` payload — large enough to trip
/// clippy's `result_large_err` lint at every fallible call site.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InterceptionError {
    #[error("call failed: {0}")]
    Call(Box<zendriver_transport::CallError>),

    #[error("invalid url pattern: {0}")]
    InvalidPattern(String),

    /// The session's interception task is gone (it panicked, or its
    /// runtime shut down), so a change couldn't be made, or a
    /// `PausedRequest`'s answer couldn't be sent.
    #[error("the session's interception task is gone")]
    SubscriptionClosed,

    #[error("invalid response from CDP: {0}")]
    InvalidResponse(String),

    /// `PausedRequest::continue_response` on a request paused other than at
    /// a `ResponseHeaders` pause.
    #[error("wrong kind of pause: this action is only valid at a ResponseHeaders pause")]
    WrongStage,

    /// No interception entry has this id (it was never inserted, or it
    /// has been removed): from `move_rule`, `remove`, and a `Position`
    /// that names it.
    #[error("no interception entry {0}")]
    NoSuchRule(crate::interceptor::RuleId),

    /// `insert` (or `InterceptBuilder::start`) was given no rules.
    #[error("no rules to insert")]
    NoRules,

    /// A rule's action can't act on the kind of pause its match names
    /// (e.g. `ModifyResponse` on a `Request` pause).
    #[error("{action} can't act on a {pause:?} pause")]
    MismatchedPause {
        action: &'static str,
        pause: crate::types::PauseKind,
    },
}

impl From<zendriver_transport::CallError> for InterceptionError {
    fn from(e: zendriver_transport::CallError) -> Self {
        Self::Call(Box::new(e))
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_pattern() {
        let e = InterceptionError::InvalidPattern("**bad".into());
        assert_eq!(e.to_string(), "invalid url pattern: **bad");
    }
}
