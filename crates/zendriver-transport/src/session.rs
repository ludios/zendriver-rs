//! `SessionHandle`: a [`Connection`] bound to a particular CDP session.
//!
//! - A *flat* handle ([`SessionHandle::new`]) rides a shared, flattened browser
//!   socket and tags every frame with its `sessionId`.
//! - A *root* handle ([`SessionHandle::new_root`]) owns a connection dialed
//!   straight at one target (e.g. `/devtools/page/<targetId>`), so it is that
//!   target's session and carries no `sessionId` — the one-socket-per-tab model.

use std::sync::Arc;

use futures::{Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::connection::Connection;
use crate::error::CallError;
use crate::frame::{AccountedRawEvent, RawEvent};

/// Cheap-to-clone handle binding a [`Connection`] to a specific CDP session.
///
/// Every CDP target — page, OOPIF, worker — has its own `sessionId` after the
/// browser fires `Target.attachedToTarget`. A `SessionHandle` couples a shared
/// transport with one such id so callers can issue commands without repeating
/// the id on every call.
#[derive(Clone, Debug)]
pub struct SessionHandle {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    conn: Connection,
    /// `Some(id)` for a flat session; `None` for a root (per-target) session.
    session_id: Option<String>,
}

impl SessionHandle {
    /// Construct a flat handle around `conn` scoped to `session_id` (a session
    /// multiplexed over a shared, flattened browser socket).
    pub fn new(conn: Connection, session_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                conn,
                session_id: Some(session_id.into()),
            }),
        }
    }

    /// Construct a root handle around a connection dialed directly at one target
    /// (e.g. `/devtools/page/<targetId>`): commands carry no `sessionId` and
    /// event filtering matches frames that likewise carry none.
    pub fn new_root(conn: Connection) -> Self {
        Self {
            inner: Arc::new(Inner {
                conn,
                session_id: None,
            }),
        }
    }

    /// The CDP `sessionId` this handle is scoped to, or `None` for a root
    /// (per-target) session whose frames carry no `sessionId` on the wire.
    pub fn session_id(&self) -> Option<&str> {
        self.inner.session_id.as_deref()
    }

    /// Whether this is a root (per-target connection) handle.
    pub fn is_root(&self) -> bool {
        self.inner.session_id.is_none()
    }

    /// Borrow the underlying [`Connection`].
    pub fn connection(&self) -> &Connection {
        &self.inner.conn
    }

    /// Send a CDP command routed to this session. A flat session tags the frame
    /// with its `sessionId`; a root session sends none.
    pub async fn call(&self, method: impl Into<String>, params: Value) -> Result<Value, CallError> {
        self.inner
            .conn
            .call_raw(method, params, self.inner.session_id.clone())
            .await
    }

    /// Subscribe to events of type `T` on this session. Matches a flat session's
    /// `sessionId`, or (for a root session) frames that carry none.
    pub fn subscribe<T>(
        &self,
        method: &'static str,
    ) -> impl Stream<Item = T> + Send + Unpin + use<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let sid = self.inner.session_id.clone();
        let raw = self.inner.conn.subscribe_raw();
        Box::pin(raw.filter_map(move |ev: RawEvent| {
            let matches = ev.session_id == sid && ev.method == method;
            async move {
                matches
                    .then(|| serde_json::from_value(ev.params).ok())
                    .flatten()
            }
        }))
    }

    /// Subscribe to this session's events on the loss-accounted bus of
    /// [`Connection::subscribe_raw_accounted`]: the events that carry this
    /// session's `sessionId` (for a root session, the events that carry
    /// none), whatever their method, and every marker of the connection
    /// (`Lagged`, `Reconnected`, `Disconnected`, `Shutdown`). Since the
    /// other sessions' events are left out, a gap between two `sequence`
    /// values doesn't mean a loss here; only a `Lagged` does, and its
    /// `missed` counts the lost events of every session.
    ///
    /// A flat session's stream also ends when Chrome detaches the session
    /// (for example, because the session's target closed), since Chrome
    /// sends no event of the session after that: at the
    /// `Target.detachedFromTarget` naming the session, the stream yields
    /// [`AccountedRawEvent::Shutdown`], with that event's generation, and
    /// then ends. A stream that misses the detach (because it subscribed
    /// after the detach, or lost the detach in a `Lagged` gap) never ends:
    /// like the connection's stream, it reports the connection's end, and
    /// then stays open.
    pub fn subscribe_accounted(
        &self,
    ) -> impl Stream<Item = AccountedRawEvent> + Send + Unpin + use<> {
        let events = self.inner.conn.subscribe_raw_accounted();
        let state = Some((events, self.inner.session_id.clone()));
        // The state is `None` after the detach, which ends the stream.
        let events = futures::stream::unfold(state, |state| async move {
            let (mut events, session_id) = state?;
            loop {
                let ev = events.next().await?;
                if let AccountedRawEvent::Event {
                    generation, event, ..
                } = &ev
                {
                    let detached = event.method == "Target.detachedFromTarget"
                        && session_id.is_some()
                        && event.params["sessionId"].as_str() == session_id.as_deref();
                    if detached {
                        let end = AccountedRawEvent::Shutdown {
                            generation: *generation,
                        };
                        return Some((end, None));
                    }
                    if event.session_id != session_id {
                        continue;
                    }
                }
                return Some((ev, Some((events, session_id))));
            }
        });
        // An `Unfold` panics if polled after its end.
        Box::pin(events.fuse())
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::connection::spawn_actor;
    use crate::connection::test_only::duplex_pair;
    use crate::testing::MockConnection;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn session_call_includes_session_id() {
        let (ws, _test_tx, mut test_rx) = duplex_pair();
        let conn = spawn_actor(ws);
        let sess = SessionHandle::new(conn.clone(), "S1");

        let call = tokio::spawn({
            let s = sess.clone();
            async move {
                s.call("Page.navigate", json!({ "url": "https://x.test" }))
                    .await
            }
        });

        let sent = test_rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(match &sent {
            Message::Text(t) => t,
            _ => panic!("expected text frame"),
        })
        .unwrap();
        assert_eq!(v["sessionId"], "S1");

        // Cancel via dropping
        drop(call);
        conn.shutdown();
    }

    #[tokio::test]
    async fn root_session_omits_session_id() {
        let (ws, _test_tx, mut test_rx) = duplex_pair();
        let conn = spawn_actor(ws);
        let sess = SessionHandle::new_root(conn.clone());
        assert!(sess.is_root());
        assert_eq!(sess.session_id(), None);

        let call = tokio::spawn({
            let s = sess.clone();
            async move {
                s.call("Page.navigate", json!({ "url": "https://x.test" }))
                    .await
            }
        });

        let sent = test_rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(match &sent {
            Message::Text(t) => t,
            _ => panic!("expected text frame"),
        })
        .unwrap();
        // A direct per-target connection carries no sessionId on the wire.
        assert!(v.get("sessionId").is_none());

        drop(call);
        conn.shutdown();
    }

    /// The next item of `events`, which must come within a second.
    async fn next_within_a_second(
        events: &mut (impl Stream<Item = AccountedRawEvent> + Unpin),
    ) -> Option<AccountedRawEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
            .await
            .unwrap()
    }

    /// A flat session's accounted stream skips the events of other sessions
    /// (including another session's detach), and ends with `Shutdown` at its
    /// own detach.
    #[tokio::test]
    async fn flat_accounted_stream_ends_at_its_own_detach() {
        let (mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn, "S1");
        let mut events = sess.subscribe_accounted();

        mock.emit_event_for_session("Page.loadEventFired", json!({}), "S2")
            .await;
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S2" }))
            .await;
        mock.emit_event_for_session("Page.frameStoppedLoading", json!({}), "S1")
            .await;
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;

        match next_within_a_second(&mut events).await {
            Some(AccountedRawEvent::Event { event, .. }) => {
                assert_eq!(event.method, "Page.frameStoppedLoading");
            }
            other => panic!("expected S1's event, got {other:?}"),
        }
        match next_within_a_second(&mut events).await {
            Some(AccountedRawEvent::Shutdown { generation: 1 }) => {}
            other => panic!("expected Shutdown, got {other:?}"),
        }
        assert!(next_within_a_second(&mut events).await.is_none());
        assert!(next_within_a_second(&mut events).await.is_none());
    }

    /// A root session has no session id for a detach to name: a detach on
    /// its socket is just one of its events, and the stream goes on, passing
    /// on the connection's markers.
    #[tokio::test]
    async fn root_accounted_stream_does_not_end_at_a_detach() {
        let (mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new_root(conn.clone());
        let mut events = sess.subscribe_accounted();

        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        mock.emit_event("Target.detachedFromTarget", json!({}))
            .await;
        for _ in 0..2 {
            match next_within_a_second(&mut events).await {
                Some(AccountedRawEvent::Event { event, .. }) => {
                    assert_eq!(event.method, "Target.detachedFromTarget");
                }
                other => panic!("expected the detach event, got {other:?}"),
            }
        }
        conn.shutdown();
        match next_within_a_second(&mut events).await {
            Some(AccountedRawEvent::Shutdown { generation: 1 }) => {}
            other => panic!("expected Shutdown, got {other:?}"),
        }
    }
}
