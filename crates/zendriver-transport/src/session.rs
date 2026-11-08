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
use crate::routes::{self, SessionEvents};

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
    /// The [`Connection::connection_generation`] whose socket attached the
    /// session. A flat session doesn't survive a [`Connection::reconnect`],
    /// so a handle of an earlier generation names a session that is gone.
    generation: u64,
}

impl SessionHandle {
    /// Construct a flat handle around `conn` scoped to `session_id` (a session
    /// multiplexed over a shared, flattened browser socket).
    ///
    /// The session is taken to belong to the connection's current socket.
    /// An observer should build its handle with
    /// [`PausedSession::session_handle`](crate::PausedSession::session_handle)
    /// instead, which knows the socket that attached the session.
    pub fn new(conn: Connection, session_id: impl Into<String>) -> Self {
        let generation = conn.connection_generation();
        Self::in_generation(conn, session_id.into(), generation)
    }

    /// A flat handle to the session `session_id`, attached by the socket of
    /// the connection's generation `generation`.
    pub(crate) fn in_generation(conn: Connection, session_id: String, generation: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                conn,
                session_id: Some(session_id),
                generation,
            }),
        }
    }

    /// Construct a root handle around a connection dialed directly at one target
    /// (e.g. `/devtools/page/<targetId>`): commands carry no `sessionId` and
    /// event filtering matches frames that likewise carry none.
    pub fn new_root(conn: Connection) -> Self {
        let generation = conn.connection_generation();
        Self {
            inner: Arc::new(Inner {
                conn,
                session_id: None,
                generation,
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

    /// Subscribe to this session's events whose method is one of `methods`,
    /// in the order Chrome sent them, with none lost. An entry of `methods`
    /// that ends in `.` (such as `"Network."`) stands for every method of
    /// that domain; any other entry stands for one method.
    ///
    /// The stream yields `Ok` for each such event that arrives after this
    /// call, and then one `Err` saying why no more will come
    /// ([`SessionEnd`](crate::routes::SessionEnd)), and then ends:
    ///
    /// - `Detached`: Chrome detached the session (a flat session only).
    /// - `Disconnected` / `Shutdown`: the connection's socket died, or the
    ///   connection was shut down.
    /// - `Reconnected`: [`Connection::reconnect`] moved the connection to a
    ///   new socket. A flat session doesn't exist there; the root session
    ///   does, but a new subscription is needed for its events there, and
    ///   the events sent in between are lost.
    /// - `Overflowed`: [`SESSION_QUEUE_CAPACITY`](crate::routes::SESSION_QUEUE_CAPACITY)
    ///   events were waiting to be read when another arrived. The stream
    ///   ends rather than skip it.
    ///
    /// A subscription made after its session's end yields that end at once.
    ///
    /// Unlike [`Self::subscribe`] and [`Self::subscribe_accounted`], which
    /// filter one broadcast of every event of the connection, each stream
    /// here has its own queue, filled by the connection's actor with only
    /// the events asked for.
    ///
    /// # Panics
    ///
    /// Panics if `methods` is empty, or if one of its entries has no `.` or
    /// has a `*`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use futures::StreamExt;
    ///
    /// # async fn example(session: zendriver_transport::SessionHandle) {
    /// let mut frames = session.events(&["Page.frameAttached", "Page.frameDetached"]);
    /// while let Some(Ok(event)) = frames.next().await {
    ///     println!("{}: {}", event.method, event.params["frameId"]);
    /// }
    /// # }
    /// ```
    pub fn events(&self, methods: &[&'static str]) -> SessionEvents {
        assert!(
            !methods.is_empty(),
            "a subscription needs at least one method"
        );
        for method in methods {
            assert!(
                method.contains('.') && !method.contains('*'),
                "{method:?} is neither a method (\"Page.frameAttached\") nor a domain (\"Page.\")"
            );
        }
        routes::lock(&self.inner.conn.inner.routes).subscribe(
            self.session_id(),
            self.inner.generation,
            methods,
        )
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
    /// then ends. A stream that subscribes after the detach yields that
    /// `Shutdown` (with the current generation) at once, and ends. A stream
    /// that lost the detach in a `Lagged` gap never ends: like the
    /// connection's stream, it reports the connection's end, and then stays
    /// open.
    pub fn subscribe_accounted(
        &self,
    ) -> impl Stream<Item = AccountedRawEvent> + Send + Unpin + use<> {
        // Subscribe first, then check for a past detach: the actor records a
        // detach before it broadcasts it, so a detach that this check misses
        // arrives on the subscription, and one that the subscription misses
        // (because it subscribed later) is found here.
        let raw = self.inner.conn.subscribe_raw_accounted();
        let session_id = self.inner.session_id.clone();
        let detached_already = session_id
            .as_ref()
            .is_some_and(|sid| routes::lock(&self.inner.conn.inner.routes).is_detached(sid));
        let past_detach = detached_already.then(|| AccountedRawEvent::Shutdown {
            generation: self.inner.conn.connection_generation(),
        });
        // The state is `None` after the detach, which ends the stream.
        let state = if detached_already {
            None
        } else {
            Some((raw, session_id))
        };
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
        Box::pin(futures::stream::iter(past_detach).chain(events).fuse())
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::connection::spawn_actor;
    use crate::connection::test_only::duplex_pair;
    use crate::observer::{ObserverError, PausedSession, TargetObserver};
    use crate::routes::SessionEnd;
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

    /// A flat session's accounted stream that starts after the session's
    /// detach ends at once, with `Shutdown`.
    #[tokio::test]
    async fn flat_accounted_stream_started_after_the_detach_ends_at_once() {
        let (mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn, "S1");
        // Seen through this stream, so that the actor has processed the
        // detach before the next subscription.
        let mut before = sess.subscribe_accounted();
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        assert!(matches!(
            next_within_a_second(&mut before).await,
            Some(AccountedRawEvent::Shutdown { generation: 1 })
        ));

        let mut after = sess.subscribe_accounted();
        match next_within_a_second(&mut after).await {
            Some(AccountedRawEvent::Shutdown { generation: 1 }) => {}
            other => panic!("expected Shutdown, got {other:?}"),
        }
        assert!(next_within_a_second(&mut after).await.is_none());
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

    /// The next item of `events`, which must come within a second.
    async fn next_of(events: &mut SessionEvents) -> Option<Result<RawEvent, SessionEnd>> {
        tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
            .await
            .unwrap()
    }

    /// The methods of the events `events` yields until its end, which must
    /// come within a second, and the end.
    async fn drain(events: &mut SessionEvents) -> (Vec<String>, SessionEnd) {
        let mut methods = Vec::new();
        loop {
            match next_of(events).await.unwrap() {
                Ok(event) => methods.push(event.method),
                Err(end) => {
                    assert!(next_of(events).await.is_none(), "the end is the last item");
                    return (methods, end);
                }
            }
        }
    }

    /// Through the actor: a session's stream gets its methods of its
    /// session, in the order sent, and ends at its detach.
    #[tokio::test]
    async fn events_come_in_order_and_end_at_the_detach() {
        let (mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn, "S1");
        let mut events = sess.events(&["Page.frameAttached", "Page.frameDetached"]);

        for (method, session) in [
            ("Page.frameAttached", "S1"),
            ("Page.frameNavigated", "S1"),
            ("Page.frameAttached", "S2"),
            ("Page.frameDetached", "S1"),
            ("Page.frameAttached", "S1"),
        ] {
            mock.emit_event_for_session(method, json!({}), session)
                .await;
        }
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;

        let (methods, end) = drain(&mut events).await;
        assert_eq!(
            methods,
            [
                "Page.frameAttached",
                "Page.frameDetached",
                "Page.frameAttached"
            ]
        );
        assert_eq!(end, SessionEnd::Detached);
        let (methods, end) = drain(&mut sess.events(&["Page."])).await;
        assert!(methods.is_empty());
        assert_eq!(
            end,
            SessionEnd::Detached,
            "a later subscription ends at once"
        );
    }

    /// Through the actor: a reconnect ends a flat session's stream, and a
    /// handle made before it can't subscribe again; a root session can.
    #[tokio::test]
    async fn events_end_at_a_reconnect() {
        let (mut mock, conn) = MockConnection::pair();
        let flat = SessionHandle::new(conn.clone(), "S1");
        let root = SessionHandle::new_root(conn.clone());
        let mut flat_events = flat.events(&["Page."]);
        let mut root_events = root.events(&["Page."]);
        mock.reconnect(&conn);
        assert_eq!(drain(&mut flat_events).await.1, SessionEnd::Reconnected);
        assert_eq!(drain(&mut root_events).await.1, SessionEnd::Reconnected);
        assert_eq!(
            drain(&mut flat.events(&["Page."])).await.1,
            SessionEnd::Reconnected
        );

        let mut root_events = root.events(&["Page."]);
        mock.emit_event("Page.loadEventFired", json!({})).await;
        assert_eq!(
            next_of(&mut root_events).await.unwrap().unwrap().method,
            "Page.loadEventFired"
        );
    }

    /// Through the actor: the socket's death and a shutdown end every
    /// stream, and later subscriptions at once.
    #[tokio::test]
    async fn events_end_with_the_connection() {
        let (mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let mut events = sess.events(&["Page."]);
        mock.emit_event_for_session("Page.loadEventFired", json!({}), "S1")
            .await;
        mock.disconnect();
        let (methods, end) = drain(&mut events).await;
        assert_eq!(methods, ["Page.loadEventFired"]);
        assert_eq!(end, SessionEnd::Disconnected);
        assert_eq!(
            drain(&mut sess.events(&["Page."])).await.1,
            SessionEnd::Disconnected
        );

        let (_mock, conn) = MockConnection::pair();
        let mut events = SessionHandle::new_root(conn.clone()).events(&["Page."]);
        conn.shutdown();
        assert_eq!(drain(&mut events).await.1, SessionEnd::Shutdown);
    }

    /// Through the actor: a subscriber that doesn't read loses its stream,
    /// not some of its events.
    #[tokio::test]
    async fn events_overflow_ends_the_stream() {
        let (mock, conn) = MockConnection::pair_with_session_queue_capacity(2);
        let sess = SessionHandle::new(conn.clone(), "S1");
        let mut events = sess.events(&["Page."]);
        // The actor fills the queues before it broadcasts an event, so once
        // the broadcast has every event, each has met the full queue.
        let mut broadcast = conn.subscribe_raw();
        for method in ["Page.a", "Page.b", "Page.c", "Page.d"] {
            mock.emit_event_for_session(method, json!({}), "S1").await;
        }
        for _ in 0..4 {
            broadcast.next().await;
        }
        let (methods, end) = drain(&mut events).await;
        assert_eq!(methods, ["Page.a", "Page.b"]);
        assert_eq!(end, SessionEnd::Overflowed);
    }

    /// Through the actor: Chrome sends no detach for a session attached
    /// through another one when that one detaches, so its stream ends with
    /// its parent's.
    #[tokio::test]
    async fn events_of_a_child_session_end_with_its_parent() {
        let (mock, conn) = MockConnection::pair();
        let mut child = SessionHandle::new(conn.clone(), "C").events(&["Page."]);
        mock.emit_event_for_session("Target.attachedToTarget", json!({ "sessionId": "C" }), "P")
            .await;
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "P" }))
            .await;
        assert_eq!(drain(&mut child).await.1, SessionEnd::Detached);
    }

    /// Through the actor: when every handle to the connection is dropped,
    /// the actor stops, and the streams end.
    #[tokio::test]
    async fn events_end_when_the_connection_is_dropped() {
        let (_mock, conn) = MockConnection::pair();
        let mut events = SessionHandle::new(conn, "S1").events(&["Page."]);
        assert_eq!(drain(&mut events).await.1, SessionEnd::Shutdown);
    }

    /// An observer that, once released, hands out the handle of the
    /// session it saw attach.
    struct HandleKeeper {
        release: Arc<tokio::sync::Notify>,
        handle: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<SessionHandle>>>,
    }

    #[async_trait::async_trait]
    impl TargetObserver for HandleKeeper {
        async fn on_target_attached(
            &self,
            session: PausedSession<'_>,
        ) -> Result<(), ObserverError> {
            self.release.notified().await;
            let sender = self.handle.lock().unwrap().take().unwrap();
            let _ = sender.send(session.session_handle());
            Ok(())
        }

        fn name(&self) -> &'static str {
            "handle-keeper"
        }
    }

    /// A handle that an observer builds after a reconnect, for a session
    /// that the replaced socket attached, names a session that is gone.
    #[tokio::test]
    async fn a_paused_sessions_handle_knows_its_socket() {
        let release = Arc::new(tokio::sync::Notify::new());
        let (handle_tx, handle_rx) = tokio::sync::oneshot::channel();
        let keeper = HandleKeeper {
            release: Arc::clone(&release),
            handle: std::sync::Mutex::new(Some(handle_tx)),
        };
        let (mut mock, conn) = MockConnection::pair_with_observers(vec![Arc::new(keeper)]);
        let mut broadcast = conn.subscribe_raw();
        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S9",
                "targetInfo": { "targetId": "T9", "type": "page", "url": "about:blank" },
                "waitingForDebugger": true,
            }),
        )
        .await;
        broadcast.next().await;
        mock.reconnect(&conn);
        release.notify_one();
        let handle = handle_rx.await.unwrap();
        assert_eq!(
            drain(&mut handle.events(&["Page."])).await.1,
            SessionEnd::Reconnected
        );
    }
}
