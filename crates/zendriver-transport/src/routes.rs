//! Ordered, lossless delivery of each session's events to the subscribers
//! of [`SessionHandle::events`](crate::session::SessionHandle::events).
//!
//! The actor hands every event to [`Routes::dispatch`], which puts it on the
//! queue of each subscriber of the event's session that asked for its
//! method. Each subscriber has its own bounded queue, so a slow subscriber
//! holds up nobody else, and a subscriber that falls
//! [`SESSION_QUEUE_CAPACITY`] events behind loses its stream (with
//! [`SessionEnd::Overflowed`]) rather than some of its events.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll, ready};

use futures::Stream;
use futures::stream::FusedStream;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::frame::RawEvent;

/// How many events one subscriber of
/// [`SessionHandle::events`](crate::session::SessionHandle::events) may have
/// waiting before its stream ends with [`SessionEnd::Overflowed`].
///
/// A queue takes memory only for the events waiting in it, and holds only
/// the methods its subscriber asked for.
pub const SESSION_QUEUE_CAPACITY: usize = 1 << 16;

/// Why a stream of [`SessionHandle::events`](crate::session::SessionHandle::events)
/// ended: no event of its session will come after this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnd {
    /// Chrome detached the session (`Target.detachedFromTarget`), for
    /// example because its target closed. Only a flat session (one with a
    /// `sessionId`) can end this way.
    #[error("Chrome detached the session")]
    Detached,
    /// The connection's socket died.
    #[error("the connection's socket died")]
    Disconnected,
    /// The connection was shut down, or every handle to it was dropped.
    #[error("the connection was shut down")]
    Shutdown,
    /// [`Connection::reconnect`](crate::connection::Connection::reconnect)
    /// moved the connection to a new socket. A flat session doesn't exist
    /// on the new socket; the connection's root session does, but the
    /// events sent while the connection had no socket are gone.
    #[error("the connection moved to a new socket")]
    Reconnected,
    /// The subscriber had [`SESSION_QUEUE_CAPACITY`] events waiting when
    /// another one arrived, so the stream ended instead of skipping that
    /// event. Every event before it was delivered.
    #[error("the subscriber fell too far behind the session's events")]
    Overflowed,
}

/// The stream returned by
/// [`SessionHandle::events`](crate::session::SessionHandle::events): the
/// session's events, as `Ok`, in the order Chrome sent them, then one `Err`
/// saying why they ended.
#[derive(Debug)]
pub struct SessionEvents {
    /// The subscriber's queue; `None` once the stream has yielded its end.
    queue: Option<mpsc::Receiver<RawEvent>>,
    /// Why the queue's sender was dropped, set before the drop. Unset if
    /// the sender was dropped with the connection itself.
    end: Arc<OnceLock<SessionEnd>>,
}

impl SessionEvents {
    /// A stream that has already ended, for the reason `end`.
    fn ended(end: SessionEnd) -> Self {
        let (_, queue) = mpsc::channel(1);
        Self {
            queue: Some(queue),
            end: Arc::new(OnceLock::from(end)),
        }
    }
}

impl Stream for SessionEvents {
    type Item = Result<RawEvent, SessionEnd>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(queue) = self.queue.as_mut() else {
            return Poll::Ready(None);
        };
        if let Some(event) = ready!(queue.poll_recv(cx)) {
            return Poll::Ready(Some(Ok(event)));
        }
        self.queue = None;
        let end = self.end.get().copied().unwrap_or(SessionEnd::Shutdown);
        Poll::Ready(Some(Err(end)))
    }
}

impl FusedStream for SessionEvents {
    fn is_terminated(&self) -> bool {
        self.queue.is_none()
    }
}

/// One subscriber's place in [`Routes`].
#[derive(Debug)]
struct Route {
    /// The methods the subscriber asked for: an entry ending in `.` stands
    /// for every method of that domain.
    methods: Box<[&'static str]>,
    queue: mpsc::Sender<RawEvent>,
    /// Shared with the subscriber's [`SessionEvents`].
    end: Arc<OnceLock<SessionEnd>>,
}

impl Route {
    fn wants(&self, method: &str) -> bool {
        self.methods.iter().any(|wanted| {
            if wanted.ends_with('.') {
                method.starts_with(wanted)
            } else {
                method == *wanted
            }
        })
    }

    /// End the subscriber's stream, once the events already queued are
    /// read, for the reason `end`.
    fn finish(self, end: SessionEnd) {
        // The reason must be set before the queue's sender drops, which
        // happens when `self` drops at the end of this function.
        let _ = self.end.set(end);
    }
}

/// Lock `routes`, even if a panic poisoned it: no change to [`Routes`] can
/// be left half done.
pub(crate) fn lock(routes: &Mutex<Routes>) -> MutexGuard<'_, Routes> {
    routes.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The subscribers of each session of one connection, and what the
/// connection knows about the sessions' ends.
#[derive(Debug)]
pub(crate) struct Routes {
    /// The connection's current generation. A generation's events and end
    /// are taken only from that generation's actor: an actor that a
    /// [`Connection::reconnect`](crate::connection::Connection::reconnect)
    /// replaced can still be running for a moment.
    generation: u64,
    /// How the current generation ended, or `None` while its actor runs.
    ended: Option<SessionEnd>,
    /// The subscribers, by the `sessionId` of their session (`None` for the
    /// connection's root session).
    by_session: HashMap<Option<String>, Vec<Route>>,
    /// The sessions that Chrome has detached. Grows by one entry per
    /// detached session over the connection's life.
    detached: HashSet<String>,
    /// The size of each new subscriber's queue.
    capacity: usize,
}

impl Routes {
    /// The routes of a new connection, in generation 1.
    pub(crate) fn new() -> Self {
        Self {
            generation: 1,
            ended: None,
            by_session: HashMap::new(),
            detached: HashSet::new(),
            capacity: SESSION_QUEUE_CAPACITY,
        }
    }

    /// Set the size of later subscribers' queues, so that a test can make
    /// one overflow.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
    }

    /// Whether Chrome has detached the session `session_id`.
    pub(crate) fn is_detached(&self, session_id: &str) -> bool {
        self.detached.contains(session_id)
    }

    /// Add a subscriber to the events of the session `session_id` whose
    /// method is one of `methods`.
    ///
    /// `handle_generation` is the generation in which the subscriber's
    /// handle to the session was made: a flat session made in an earlier
    /// generation is gone.
    pub(crate) fn subscribe(
        &mut self,
        session_id: Option<&str>,
        handle_generation: u64,
        methods: &[&'static str],
    ) -> SessionEvents {
        let mut end = self.ended;
        if end.is_none() && session_id.is_some_and(|id| self.detached.contains(id)) {
            end = Some(SessionEnd::Detached);
        }
        if end.is_none() && session_id.is_some() && handle_generation != self.generation {
            end = Some(SessionEnd::Reconnected);
        }
        if let Some(end) = end {
            return SessionEvents::ended(end);
        }

        let (queue, receiver) = mpsc::channel(self.capacity);
        let end = Arc::new(OnceLock::new());
        let routes = self
            .by_session
            .entry(session_id.map(str::to_owned))
            .or_default();
        // Forget the subscribers that went away, which `dispatch` notices
        // only when an event they asked for arrives.
        routes.retain(|route| !route.queue.is_closed());
        routes.push(Route {
            methods: methods.into(),
            queue,
            end: Arc::clone(&end),
        });
        SessionEvents {
            queue: Some(receiver),
            end,
        }
    }

    /// Put `event`, which the actor of generation `generation` received, on
    /// the queue of each subscriber that wants it.
    pub(crate) fn dispatch(&mut self, generation: u64, event: &RawEvent) {
        if generation != self.generation {
            return;
        }
        let Some(routes) = self.by_session.get_mut(&event.session_id) else {
            return;
        };
        routes.retain(|route| {
            if !route.wants(&event.method) {
                return true;
            }
            match route.queue.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    let _ = route.end.set(SessionEnd::Overflowed);
                    false
                }
                Err(TrySendError::Closed(_)) => false,
            }
        });
        if routes.is_empty() {
            self.by_session.remove(&event.session_id);
        }
    }

    /// Chrome detached the session `session_id`: end its subscribers'
    /// streams, and those of later subscribers.
    pub(crate) fn detach(&mut self, session_id: &str) {
        let key = Some(session_id.to_owned());
        for route in self.by_session.remove(&key).into_iter().flatten() {
            route.finish(SessionEnd::Detached);
        }
        self.detached.insert(session_id.to_owned());
    }

    /// The actor of generation `generation` stopped, for the reason `end`
    /// (`Disconnected` or `Shutdown`): end every stream, and those of later
    /// subscribers until a reconnect. Does nothing if a reconnect has
    /// already replaced that generation.
    pub(crate) fn end_generation(&mut self, generation: u64, end: SessionEnd) {
        if generation != self.generation {
            return;
        }
        self.ended = Some(end);
        self.finish_all(end);
    }

    /// A reconnect started generation `generation`: end every stream of
    /// the previous one.
    pub(crate) fn start_generation(&mut self, generation: u64) {
        assert!(
            generation > self.generation,
            "generation {generation} doesn't follow generation {}",
            self.generation
        );
        self.generation = generation;
        self.ended = None;
        self.finish_all(SessionEnd::Reconnected);
    }

    fn finish_all(&mut self, end: SessionEnd) {
        for route in self.by_session.drain().flat_map(|(_, routes)| routes) {
            route.finish(end);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    fn event(method: &str, session_id: Option<&str>) -> RawEvent {
        RawEvent {
            method: method.to_owned(),
            params: json!({}),
            session_id: session_id.map(str::to_owned),
        }
    }

    /// The methods of the events `events` yields until its end, and the end.
    async fn drain(mut events: SessionEvents) -> (Vec<String>, SessionEnd) {
        let mut methods = Vec::new();
        loop {
            match events.next().await.unwrap() {
                Ok(event) => methods.push(event.method),
                Err(end) => {
                    assert!(events.next().await.is_none(), "the end is the last item");
                    return (methods, end);
                }
            }
        }
    }

    #[tokio::test]
    async fn a_subscriber_gets_the_methods_it_asked_for_of_its_session_in_order() {
        let mut routes = Routes::new();
        let events = routes.subscribe(Some("S1"), 1, &["Page.frameAttached", "Network."]);
        routes.dispatch(1, &event("Network.requestWillBeSent", Some("S1")));
        routes.dispatch(1, &event("Page.frameAttached", Some("S2")));
        routes.dispatch(1, &event("Page.frameDetached", Some("S1")));
        routes.dispatch(1, &event("Page.frameAttached", Some("S1")));
        routes.dispatch(1, &event("Network.loadingFinished", Some("S1")));
        routes.dispatch(1, &event("Network.loadingFinished", None));
        routes.detach("S1");
        let (methods, end) = drain(events).await;
        assert_eq!(
            methods,
            [
                "Network.requestWillBeSent",
                "Page.frameAttached",
                "Network.loadingFinished"
            ]
        );
        assert_eq!(end, SessionEnd::Detached);
    }

    #[tokio::test]
    async fn the_root_session_has_its_own_subscribers() {
        let mut routes = Routes::new();
        let events = routes.subscribe(None, 1, &["Target."]);
        routes.dispatch(1, &event("Target.targetCreated", Some("S1")));
        routes.dispatch(1, &event("Target.targetCreated", None));
        routes.end_generation(1, SessionEnd::Shutdown);
        let (methods, end) = drain(events).await;
        assert_eq!(methods, ["Target.targetCreated"]);
        assert_eq!(end, SessionEnd::Shutdown);
    }

    #[tokio::test]
    async fn a_full_queue_ends_the_stream_after_the_queued_events() {
        let mut routes = Routes::new();
        routes.set_capacity(2);
        let slow = routes.subscribe(Some("S1"), 1, &["Page."]);
        let other = routes.subscribe(Some("S1"), 1, &["Page."]);
        let mut other = other.boxed();
        for method in ["Page.a", "Page.b", "Page.c"] {
            routes.dispatch(1, &event(method, Some("S1")));
            // The other subscriber keeps up, so it loses nothing.
            assert_eq!(other.next().await.unwrap().unwrap().method, method);
        }
        let (methods, end) = drain(slow).await;
        assert_eq!(methods, ["Page.a", "Page.b"]);
        assert_eq!(end, SessionEnd::Overflowed);
    }

    #[tokio::test]
    async fn a_subscription_after_its_sessions_end_ends_at_once() {
        let mut routes = Routes::new();
        routes.detach("S1");
        let (methods, end) = drain(routes.subscribe(Some("S1"), 1, &["Page."])).await;
        assert!(methods.is_empty());
        assert_eq!(end, SessionEnd::Detached);

        routes.start_generation(2);
        let (_, end) = drain(routes.subscribe(Some("S2"), 1, &["Page."])).await;
        assert_eq!(
            end,
            SessionEnd::Reconnected,
            "a flat session doesn't survive a reconnect"
        );

        routes.end_generation(2, SessionEnd::Disconnected);
        let (_, end) = drain(routes.subscribe(None, 2, &["Page."])).await;
        assert_eq!(end, SessionEnd::Disconnected);
    }

    #[tokio::test]
    async fn a_reconnect_ends_every_stream_and_ignores_the_old_actor() {
        let mut routes = Routes::new();
        let flat = routes.subscribe(Some("S1"), 1, &["Page."]);
        let root = routes.subscribe(None, 1, &["Page."]);
        routes.start_generation(2);
        assert_eq!(drain(flat).await.1, SessionEnd::Reconnected);
        assert_eq!(drain(root).await.1, SessionEnd::Reconnected);

        let root = routes.subscribe(None, 1, &["Page."]);
        routes.dispatch(1, &event("Page.old", None));
        routes.end_generation(1, SessionEnd::Disconnected);
        routes.dispatch(2, &event("Page.new", None));
        routes.end_generation(2, SessionEnd::Shutdown);
        let (methods, end) = drain(root).await;
        assert_eq!(methods, ["Page.new"]);
        assert_eq!(end, SessionEnd::Shutdown);
    }

    #[tokio::test]
    async fn a_stream_whose_connection_is_dropped_ends_as_shut_down() {
        let mut routes = Routes::new();
        let events = routes.subscribe(Some("S1"), 1, &["Page."]);
        drop(routes);
        assert_eq!(drain(events).await, (Vec::new(), SessionEnd::Shutdown));
    }

    #[tokio::test]
    async fn a_new_subscription_forgets_the_subscribers_that_went_away() {
        let mut routes = Routes::new();
        drop(routes.subscribe(Some("S1"), 1, &["Page.rare"]));
        let _kept = routes.subscribe(Some("S1"), 1, &["Page.rare"]);
        assert_eq!(routes.by_session[&Some("S1".to_owned())].len(), 1);
    }
}
