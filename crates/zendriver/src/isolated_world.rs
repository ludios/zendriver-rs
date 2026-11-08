//! Execution contexts: recognizing Chrome's answer for one that no longer
//! exists, and reading which one a remote object lives in. Zendriver's own
//! JavaScript runs in its isolated world, whose context
//! [`crate::frame::Frame::ensure_isolated_world`] looks up anew for each
//! call; a navigation can still destroy that context before Chrome handles
//! the command that uses its id.

use crate::error::{Result, ZendriverError};

/// Whether `e` is Chrome saying that the `executionContextId` sent with a
/// call names no context (typically one that a navigation destroyed after
/// its id was looked up), which is worth one retry with a fresh id.
///
/// Chrome reports this as `-32000 Cannot find context with specified id`,
/// but [`crate::error::ZendriverError`]'s `From<CallError>` maps `-32000`
/// onto [`crate::error::ZendriverError::Navigation`], so the match is on
/// that variant and not on `Cdp`.
pub(crate) fn is_stale_context(e: &ZendriverError) -> bool {
    matches!(e, ZendriverError::Navigation(m) if m.contains("Cannot find context"))
}

/// Run `attempt`, and run it once more if it fails because a navigation
/// destroyed the execution context that it used ([`is_stale_context`]).
/// Each run of `attempt` must look its context up anew.
///
/// Only that one failure is retried: a navigation while the script itself
/// runs fails differently, and isn't retried, since the script may
/// already have had effects.
pub(crate) async fn retry_once_on_stale_context<T, F, Fut>(mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    match attempt().await {
        Err(e) if is_stale_context(&e) => attempt().await,
        done => done,
    }
}

/// The id of the execution context that the V8 remote object `object_id`
/// lives in.
///
/// V8 writes an object id as `<isolate id>.<context id>.<object number>`
/// (its `RemoteObjectId`), three integers; the isolate id is 64 bits, and
/// Chrome 157 writes it signed (it sent `-3707980480376565251.3.1`). The protocol doesn't
/// document that format, so this fails loudly rather than guessing when an
/// id doesn't have it.
///
/// # Errors
///
/// [`ZendriverError::Navigation`] when `object_id` isn't three integers
/// separated by `.`.
pub(crate) fn context_id_of_object(object_id: &str) -> Result<i64> {
    let parts: Vec<&str> = object_id.split('.').collect();
    let mut context = None;
    if let [isolate, middle, object] = parts.as_slice() {
        // `i128`: V8 has written the isolate id signed; written unsigned,
        // it would exceed `i64`.
        if isolate.parse::<i128>().is_ok() && object.parse::<i64>().is_ok() {
            context = middle.parse().ok();
        }
    }
    context.ok_or_else(|| {
        ZendriverError::Navigation(format!(
            "can't read an execution context id out of the remote object id {object_id:?}"
        ))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn context_id_of_object_reads_the_middle_of_three_parts() {
        assert_eq!(context_id_of_object("5137660561264051122.3.1").unwrap(), 3);
        assert_eq!(context_id_of_object("-3707980480376565251.4.2").unwrap(), 4);
        for bad in [
            "3",
            "1.3",
            "1.x.2",
            "1.3.2.4",
            "x.3.1",
            "1.3.",
            "1..2",
            r#"{"injectedScriptId":3,"id":1}"#,
        ] {
            assert!(
                matches!(
                    context_id_of_object(bad),
                    Err(ZendriverError::Navigation(_))
                ),
                "{bad}"
            );
        }
    }
}
