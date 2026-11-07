//! Recognizing Chrome's answer for an execution context that no longer
//! exists. Zendriver's own JavaScript runs in its isolated world, whose
//! context [`crate::frame::Frame::ensure_isolated_world`] looks up anew for
//! each call; a navigation can still destroy that context before the call
//! reaches it.

/// Whether `e` is Chrome saying that the `executionContextId` sent with a
/// call names no context (typically one that a navigation destroyed after
/// its id was looked up), which is worth one retry with a fresh id.
///
/// Chrome reports this as `-32000 Cannot find context with specified id`,
/// but [`crate::error::ZendriverError`]'s `From<CallError>` maps `-32000`
/// onto [`crate::error::ZendriverError::Navigation`], so the match is on
/// that variant and not on `Cdp`.
pub(crate) fn is_stale_context(e: &crate::error::ZendriverError) -> bool {
    matches!(e, crate::error::ZendriverError::Navigation(m) if m.contains("Cannot find context"))
}
