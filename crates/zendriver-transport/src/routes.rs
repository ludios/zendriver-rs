//! Delivery of each session's events, in order and without gaps, to the
//! subscribers of [`SessionHandle::events`](crate::session::SessionHandle::events).
//!
//! The actor hands every event to `Routes::dispatch`, which puts it on the
//! queue of each subscriber of the event's session that asked for its
//! method. Each subscriber has its own bounded queue, so a slow subscriber
//! holds up nobody else. A subscriber whose queue is full when an event
//! arrives gets no more events: its stream ends with
//! [`SessionEnd::Overflowed`] once it has read the queued ones, so it never
//! sees a gap in what it does get.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll, ready};

use futures::Stream;
use futures::stream::FusedStream;
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::warn;

use crate::frame::RawEvent;

/// How many events one subscriber of
/// [`SessionHandle::events`](crate::session::SessionHandle::events) may have
/// waiting: an event that arrives while its queue holds this many ends its
/// stream with [`SessionEnd::Overflowed`].
///
/// A queue holds only the methods its subscriber asked for, and the
/// subscribers that want an event share one copy of it. A subscriber that
/// keeps up never comes near this; one that has this many events waiting
/// has stopped reading, and the bound caps the memory it holds.
pub const SESSION_QUEUE_CAPACITY: usize = 4096;

/// Why a stream of [`SessionHandle::events`](crate::session::SessionHandle::events)
/// ended: the stream yields nothing after it.
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
    /// on the new socket. The connection's root session does, and a new
    /// subscription gets its events from the moment it is made; the root
    /// session's events between this end and that moment are not delivered
    /// to anyone.
    #[error("the connection moved to a new socket")]
    Reconnected,
    /// The subscriber's queue was full (it held
    /// [`SESSION_QUEUE_CAPACITY`] events, unless a test set another size)
    /// when another event arrived, so the stream ended instead of skipping
    /// that event. Every event before it was delivered.
    #[error("the subscriber fell too far behind the session's events")]
    Overflowed,
}

/// The stream returned by
/// [`SessionHandle::events`](crate::session::SessionHandle::events): the
/// session's events, as `Ok`, in the order Chrome sent them, then one `Err`
/// saying why this stream stopped.
#[derive(Debug)]
pub struct SessionEvents {
    /// The subscriber's queue; `None` once the stream has yielded its end.
    queue: Option<mpsc::Receiver<Arc<RawEvent>>>,
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
    type Item = Result<Arc<RawEvent>, SessionEnd>;

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
    /// The sending end of the subscriber's queue.
    queue: mpsc::Sender<Arc<RawEvent>>,
    /// Shared with the subscriber's [`SessionEvents`].
    end: Arc<OnceLock<SessionEnd>>,
}

impl Route {
    /// Whether the subscriber asked for the events of method `method`.
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
    /// The sessions of the current generation that Chrome has detached.
    detached: HashSet<String>,
    /// The sessions of the current generation attached through another
    /// session (`Target.attachedToTarget` arriving on that session), by that
    /// parent session. Chrome sends no detach for them when their parent
    /// detaches: they end with it.
    children: HashMap<String, Vec<String>>,
    /// The parent of each session in [`Self::children`].
    parents: HashMap<String, String>,
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
            children: HashMap::new(),
            parents: HashMap::new(),
            capacity: SESSION_QUEUE_CAPACITY,
        }
    }

    /// Set the size of later subscribers' queues, so that a test can make
    /// one overflow.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn set_capacity(&mut self, capacity: usize) {
        assert!(capacity > 0, "a queue needs room for at least one event");
        self.capacity = capacity;
    }

    /// Add a subscriber to the events of the session `session_id` whose
    /// method is one of `methods`.
    ///
    /// `handle_generation` is the generation in which the session was
    /// attached: a flat session of an earlier generation is gone.
    ///
    /// A subscription to a session that has ended ends at once, with the
    /// first reason that applies: `Reconnected` if the session belongs to an
    /// earlier generation, `Detached` if Chrome detached it, and otherwise
    /// the end of the current generation.
    pub(crate) fn subscribe(
        &mut self,
        session_id: Option<&str>,
        handle_generation: u64,
        methods: &[&'static str],
    ) -> SessionEvents {
        let mut end = None;
        if session_id.is_some() && handle_generation != self.generation {
            end = Some(SessionEnd::Reconnected);
        } else if session_id.is_some_and(|id| self.detached.contains(id)) {
            end = Some(SessionEnd::Detached);
        } else if let Some(ended) = self.ended {
            end = Some(ended);
        }
        if let Some(end) = end {
            return SessionEvents::ended(end);
        }

        // Forget the subscribers that went away, which `dispatch` notices
        // only when an event of their session arrives.
        self.by_session.retain(|_, routes| {
            routes.retain(|route| !route.queue.is_closed());
            !routes.is_empty()
        });
        let (queue, receiver) = mpsc::channel(self.capacity);
        let end = Arc::new(OnceLock::new());
        let routes = self
            .by_session
            .entry(session_id.map(str::to_owned))
            .or_default();
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
    /// the queue of each subscriber that wants it. The subscribers share one
    /// copy of it, made only if one of them wants it.
    pub(crate) fn dispatch(&mut self, generation: u64, event: &RawEvent) {
        if generation != self.generation {
            return;
        }
        let Some(routes) = self.by_session.get_mut(&event.session_id) else {
            return;
        };
        let mut shared: Option<Arc<RawEvent>> = None;
        routes.retain(|route| {
            if route.queue.is_closed() {
                return false;
            }
            if !route.wants(&event.method) {
                return true;
            }
            let shared = shared.get_or_insert_with(|| Arc::new(event.clone()));
            match route.queue.try_send(Arc::clone(shared)) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    warn!(
                        session_id = ?event.session_id,
                        methods = ?route.methods,
                        capacity = route.queue.max_capacity(),
                        "a subscriber of a session's events left its queue full; ending its stream",
                    );
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

    /// The actor of generation `generation` received the attach of session
    /// `child` on session `parent`.
    pub(crate) fn attach(&mut self, generation: u64, parent: &str, child: &str) {
        if generation != self.generation {
            return;
        }
        self.children
            .entry(parent.to_owned())
            .or_default()
            .push(child.to_owned());
        self.parents.insert(child.to_owned(), parent.to_owned());
    }

    /// The actor of generation `generation` received the detach of session
    /// `session_id`: end its subscribers' streams, and those of later
    /// subscribers, and do the same for the sessions attached through it,
    /// whose ends Chrome doesn't report.
    pub(crate) fn detach(&mut self, generation: u64, session_id: &str) {
        if generation != self.generation {
            return;
        }
        if let Some(parent) = self.parents.remove(session_id) {
            if let Some(siblings) = self.children.get_mut(&parent) {
                siblings.retain(|sibling| sibling != session_id);
            }
        }
        let mut ending = vec![session_id.to_owned()];
        while let Some(id) = ending.pop() {
            ending.extend(self.children.remove(&id).into_iter().flatten());
            self.parents.remove(&id);
            for route in self
                .by_session
                .remove(&Some(id.clone()))
                .into_iter()
                .flatten()
            {
                route.finish(SessionEnd::Detached);
            }
            self.detached.insert(id);
        }
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
    /// the previous one, and forget its sessions (a later subscription to
    /// one of them ends because of its generation).
    pub(crate) fn start_generation(&mut self, generation: u64) {
        assert!(
            generation > self.generation,
            "generation {generation} doesn't follow generation {}",
            self.generation
        );
        self.generation = generation;
        self.ended = None;
        self.detached.clear();
        self.children.clear();
        self.parents.clear();
        self.finish_all(SessionEnd::Reconnected);
    }

    /// End every subscriber's stream, for the reason `end`, and forget the
    /// subscribers.
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
                Ok(event) => methods.push(event.method.clone()),
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
        routes.detach(1, "S1");
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
        let events = routes.subscribe(None, 1, &["Test."]);
        routes.dispatch(1, &event("Test.created", Some("S1")));
        routes.dispatch(1, &event("Test.created", None));
        routes.end_generation(1, SessionEnd::Shutdown);
        let (methods, end) = drain(events).await;
        assert_eq!(methods, ["Test.created"]);
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
        routes.detach(1, "S1");
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

        let root = routes.subscribe(None, 1, &["Test."]);
        routes.dispatch(1, &event("Test.old", None));
        routes.end_generation(1, SessionEnd::Disconnected);
        routes.dispatch(2, &event("Test.new", None));
        routes.end_generation(2, SessionEnd::Shutdown);
        let (methods, end) = drain(root).await;
        assert_eq!(methods, ["Test.new"]);
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
        drop(routes.subscribe(Some("S1"), 1, &["Test.rare"]));
        let _kept = routes.subscribe(Some("S2"), 1, &["Test.rare"]);
        assert!(!routes.by_session.contains_key(&Some("S1".to_owned())));
    }

    #[tokio::test]
    async fn any_event_of_the_session_forgets_the_subscribers_that_went_away() {
        let mut routes = Routes::new();
        let _kept = routes.subscribe(Some("S1"), 1, &["Test.other"]);
        drop(routes.subscribe(Some("S1"), 1, &["Test.rare"]));
        routes.dispatch(1, &event("Test.other", Some("S1")));
        assert_eq!(routes.by_session[&Some("S1".to_owned())].len(), 1);
    }

    #[tokio::test]
    async fn detaching_a_session_ends_the_sessions_attached_through_it() {
        let mut routes = Routes::new();
        routes.attach(1, "P", "C");
        routes.attach(1, "C", "G");
        routes.attach(1, "Q", "D");
        let child = routes.subscribe(Some("C"), 1, &["Page."]);
        let grandchild = routes.subscribe(Some("G"), 1, &["Page."]);
        let mut other = routes.subscribe(Some("D"), 1, &["Page."]);
        routes.detach(1, "P");
        assert_eq!(drain(child).await.1, SessionEnd::Detached);
        assert_eq!(drain(grandchild).await.1, SessionEnd::Detached);
        let (_, end) = drain(routes.subscribe(Some("G"), 1, &["Page."])).await;
        assert_eq!(end, SessionEnd::Detached);
        routes.dispatch(1, &event("Page.a", Some("D")));
        assert_eq!(other.next().await.unwrap().unwrap().method, "Page.a");
    }

    #[tokio::test]
    async fn a_child_that_detaches_leaves_its_parent_alone() {
        let mut routes = Routes::new();
        routes.attach(1, "P", "C");
        let mut parent = routes.subscribe(Some("P"), 1, &["Page."]);
        routes.detach(1, "C");
        assert!(routes.children["P"].is_empty());
        assert!(routes.parents.is_empty());
        routes.dispatch(1, &event("Page.a", Some("P")));
        assert_eq!(parent.next().await.unwrap().unwrap().method, "Page.a");
    }

    #[tokio::test]
    async fn a_replaced_actor_can_neither_attach_nor_detach() {
        let mut routes = Routes::new();
        routes.start_generation(2);
        let mut events = routes.subscribe(Some("S1"), 2, &["Page."]);
        routes.attach(1, "S1", "C");
        routes.detach(1, "S1");
        assert!(routes.children.is_empty());
        routes.dispatch(2, &event("Page.a", Some("S1")));
        assert_eq!(events.next().await.unwrap().unwrap().method, "Page.a");
    }

    #[tokio::test]
    async fn a_late_subscription_ends_with_its_sessions_own_end() {
        let mut routes = Routes::new();
        routes.detach(1, "S1");
        routes.end_generation(1, SessionEnd::Shutdown);
        let (_, end) = drain(routes.subscribe(Some("S1"), 1, &["Page."])).await;
        assert_eq!(end, SessionEnd::Detached);

        let mut routes = Routes::new();
        routes.start_generation(2);
        routes.end_generation(2, SessionEnd::Disconnected);
        let (_, end) = drain(routes.subscribe(Some("S1"), 1, &["Page."])).await;
        assert_eq!(end, SessionEnd::Reconnected);
    }
}
