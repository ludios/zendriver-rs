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

    /// Subscribe to this session's events whose method is one of `methods`.
    /// An entry of `methods` that ends in `.` (such as `"Network."`) stands
    /// for every method of that domain; any other entry stands for one
    /// method.
    ///
    /// The stream yields `Ok` for each such event that arrives after this
    /// call, in the order Chrome sent them, and then one `Err`
    /// ([`SessionEnd`](crate::routes::SessionEnd)) saying why this stream
    /// stopped, and then ends. It skips none of those events before the
    /// `Err`. The reasons:
    ///
    /// - `Detached`: Chrome detached the session, or the session it was
    ///   attached through (a flat session only).
    /// - `Disconnected` / `Shutdown`: the connection's socket died, or the
    ///   connection was shut down.
    /// - `Reconnected`: [`Connection::reconnect`] moved the connection to a
    ///   new socket. A flat session doesn't exist there. The root session
    ///   does: a new subscription gets its events from the moment it is
    ///   made, and the root session's events between this end and that
    ///   moment are not delivered to anyone.
    /// - `Overflowed`: the stream's queue was full (it held
    ///   [`SESSION_QUEUE_CAPACITY`](crate::routes::SESSION_QUEUE_CAPACITY)
    ///   events, unless a test set another size) when another event
    ///   arrived. The stream ends rather than skip that event.
    ///
    /// A subscription made after its session ended yields an end at once:
    /// `Reconnected` if a reconnect came after the handle's session was
    /// attached, `Detached` if Chrome detached the session, and otherwise the
    /// reason the connection's current socket stopped.
    ///
    /// Unlike [`Connection::subscribe_raw`] and
    /// [`Connection::subscribe_raw_accounted`], which filter one broadcast of
    /// every event of the connection, each stream here has its own queue,
    /// filled by the connection's actor with only the events asked for.
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
    /// use zendriver_transport::SessionEnd;
    ///
    /// # async fn example(session: zendriver_transport::SessionHandle) {
    /// let mut frames = session.events(&["Page.frameAttached", "Page.frameDetached"]);
    /// while let Some(next) = frames.next().await {
    ///     match next {
    ///         Ok(event) => println!("{}: {}", event.method, event.params["frameId"]),
    ///         Err(SessionEnd::Detached) => println!("the session is gone"),
    ///         Err(end) => eprintln!("stopped before the session ended: {end}"),
    ///     }
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

    /// Subscribe to this session's events of method `method`, decoded into
    /// `T`: the events of [`Self::events`] for that one method, in order,
    /// skipping one that doesn't decode as a `T`. The stream ends, without
    /// saying why, where that one would yield its [`SessionEnd`](crate::routes::SessionEnd).
    ///
    /// # Panics
    ///
    /// Panics if `method` has no `.` or has a `*` (see [`Self::events`]).
    pub fn subscribe<T>(
        &self,
        method: &'static str,
    ) -> impl Stream<Item = T> + Send + Unpin + use<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        Box::pin(self.events(&[method]).filter_map(|next| {
            let decoded = next
                .ok()
                .and_then(|event| T::deserialize(&event.params).ok());
            std::future::ready(decoded)
        }))
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::connection::spawn_actor;
    use crate::connection::test_only::duplex_pair;
    use crate::frame::RawEvent;
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
    async fn next_of(events: &mut SessionEvents) -> Option<Result<Arc<RawEvent>, SessionEnd>> {
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
                Ok(event) => methods.push(event.method.clone()),
                Err(end) => {
                    assert!(next_of(events).await.is_none(), "the end is the last item");
                    return (methods, end);
                }
            }
        }
    }

    /// Through the actor: a stream gets the methods it asked for, of its own
    /// session only, in the order they were sent, and ends when the session
    /// detaches.
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
    /// stream, and a later subscription ends at once with the same reason.
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

    /// `subscribe` decodes its method's events of its session, in order,
    /// skips one that doesn't decode, and ends with the session.
    #[tokio::test]
    async fn subscribe_decodes_its_events_and_ends_with_the_session() {
        #[derive(serde::Deserialize)]
        struct Frame {
            #[serde(rename = "frameId")]
            frame_id: String,
        }
        let (mock, conn) = MockConnection::pair();
        let mut frames =
            SessionHandle::new(conn.clone(), "S1").subscribe::<Frame>("Page.frameAttached");
        for (params, session) in [
            (json!({ "frameId": "A" }), "S1"),
            (json!({ "frameId": "X" }), "S2"),
            (json!({ "frameId": 7 }), "S1"),
            (json!({ "frameId": "B" }), "S1"),
        ] {
            mock.emit_event_for_session("Page.frameAttached", params, session)
                .await;
        }
        mock.emit_event("Target.detachedFromTarget", json!({ "sessionId": "S1" }))
            .await;
        let mut ids = Vec::new();
        while let Some(frame) =
            tokio::time::timeout(std::time::Duration::from_secs(1), frames.next())
                .await
                .unwrap()
        {
            ids.push(frame.frame_id);
        }
        assert_eq!(ids, ["A", "B"]);
    }
}
