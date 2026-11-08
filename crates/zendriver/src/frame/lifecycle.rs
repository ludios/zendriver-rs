//! Frame lifecycle event subscriber — internal.
//!
//! Spawned per [`crate::Tab`] at construction time. Subscribes to
//! `Page.frameAttached`, `Page.frameDetached`, and `Page.frameNavigated`
//! events on the owning tab's session and maintains the tab's frames
//! registry (read via [`crate::Tab::frames`]).
//!
//! ## Wiring
//!
//! Each [`crate::Tab`] owns one background task. At construction time the
//! task:
//! 1. Subscribes to the three `Page.frame*` events, in one subscription
//!    so that they are applied in the order Chrome sent them, BEFORE
//!    awaiting `Page.enable` — avoids the race where Chrome fires events
//!    between the enable reply and our subscription registration.
//! 2. Fires `Page.enable` once on the tab's session so Chrome starts
//!    emitting `Page.frame*` events.
//! 3. Loops over the events, mutating the registry:
//!    - `Page.frameAttached` — construct a new [`crate::Frame`] sharing
//!      the tab's session (same-origin sub-frame; OOPIF frames arrive
//!      through the [`crate::frame::oopif`] observer path on their own
//!      child session, NOT this stream) and insert it under `frameId`.
//!    - `Page.frameNavigated` — update the existing entry's URL in
//!      place (and backfill the frame `name` the attach event could not
//!      carry); insert a fresh [`crate::Frame`] if no entry exists.
//!    - `Page.frameDetached` — remove the entry from the registry.
//!
//! Chrome reports each frame under one id for the frame's whole life (the
//! id is the frame tree node's DevTools token, which a move to another
//! process keeps), so applying the events in order keeps the registry
//! equal to Chrome's frame tree.
//!
//! ## Nothing in the loop may await a CDP round-trip
//!
//! While the loop waits, no other frame event is applied, so the registry
//! falls behind Chrome's frame tree (the events wait in the subscription's
//! queue, and the subscription ends if the queue fills). Keep the loop to
//! registry mutation; spawn anything that talks to Chrome.
//!
//! The task runs until its [`tokio_util::sync::CancellationToken`] fires —
//! typically when the owning Tab is dropped — or the session's events end.
//! The startup `Page.enable` it spawns observes the same token, so nothing
//! outlives it waiting out a CDP budget.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use futures::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{trace, warn};
use zendriver_transport::{RawEvent, SessionHandle};

use crate::frame::Frame;
use crate::tab::TabInner;

/// Minimal projection of `Page.frameAttached` — only the fields we need
/// to construct a [`Frame`]. URL/name are not on this payload; they arrive
/// on a subsequent `Page.frameNavigated`.
#[derive(Debug, Deserialize)]
struct FrameAttachedEvent {
    #[serde(rename = "frameId")]
    frame_id: String,
    #[serde(rename = "parentFrameId")]
    parent_frame_id: Option<String>,
}

/// Minimal projection of `Page.frameDetached` — only `frameId` matters for
/// registry removal.
#[derive(Debug, Deserialize)]
struct FrameDetachedEvent {
    #[serde(rename = "frameId")]
    frame_id: String,
}

/// Minimal projection of `Page.frameNavigated`. Chrome nests the frame
/// metadata under `frame: {...}`.
#[derive(Debug, Deserialize)]
struct FrameNavigatedEvent {
    frame: NavigatedFrameInner,
}

#[derive(Debug, Deserialize)]
struct NavigatedFrameInner {
    id: String,
    #[serde(rename = "parentId", default)]
    parent_id: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    name: Option<String>,
}

/// Drive the lifecycle subscriber until `cancel` fires or the session's
/// events end.
///
/// `frames` is the [`RwLock`]-protected registry shared with the owning
/// [`TabInner`]. `tab_weak` is the [`Weak<TabInner>`] handed to each
/// constructed [`Frame`] so its `tab_for_synthesize` upgrade can return
/// the owning `Tab`.
///
/// The `Page.enable` call is fire-and-forget: failure is logged + ignored
/// so the subscriber keeps running on a previously-enabled Page domain.
pub(crate) async fn run(
    session: SessionHandle,
    frames: Arc<RwLock<HashMap<String, Frame>>>,
    tab_weak: Weak<TabInner>,
    cancel: CancellationToken,
) {
    // One subscription for the three methods, so that the events are
    // applied in the order Chrome sent them.
    let mut events = session.events(&[
        "Page.frameAttached",
        "Page.frameDetached",
        "Page.frameNavigated",
    ]);

    // Fire-and-forget `Page.enable`: the mock harness never replies to this
    // call, and in production the subscription above is already
    // registered, so awaiting the response would only serialize the first
    // arriving event behind the enable round-trip.
    //
    // Cancellation-aware: an unanswered call sits on the transport's
    // default budget ([`zendriver_transport::DEFAULT_CALL_TIMEOUT`], 180s),
    // and nothing spawned here may outlive the Tab that owns it by that
    // much while holding a session clone.
    let enable_session = session.clone();
    let enable_cancel = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            () = enable_cancel.cancelled() => {}
            res = enable_session.call("Page.enable", serde_json::json!({})) => {
                if let Err(e) = res {
                    warn!(error = %e, "frame::lifecycle: Page.enable failed; frame events may be inactive");
                }
            }
        }
    });

    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => {
                trace!("frame::lifecycle: cancellation received, exiting");
                return;
            }
            event = events.next() => event,
        };
        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(end)) => {
                trace!(%end, "frame::lifecycle: the session's events ended, exiting");
                return;
            }
            None => return,
        };
        match event.method.as_str() {
            "Page.frameAttached" => {
                let Some(ev) = decode::<FrameAttachedEvent>(&event) else {
                    continue;
                };
                let frame = Frame::new(
                    ev.frame_id.clone(),
                    ev.parent_frame_id,
                    String::new(),
                    None,
                    session.clone(),
                    tab_weak.clone(),
                );
                frames.write().await.insert(ev.frame_id, frame);
            }
            "Page.frameNavigated" => {
                let Some(ev) = decode::<FrameNavigatedEvent>(&event) else {
                    continue;
                };
                let frame_id = ev.frame.id;
                let new_url = ev.frame.url;
                // `Page.frameAttached` carries no name, so an entry created
                // from it always starts nameless. Treat a non-empty name on
                // `frameNavigated` as the authoritative one; never clear a
                // known name from an event that simply omits the field.
                let new_name = ev.frame.name.filter(|n| !n.is_empty());

                // Fast path, under a READ guard: refreshing a URL mutates
                // the `Frame`'s own `RwLock`, not the registry map, so the
                // ordinary navigation never needs exclusive access to the
                // registry. Only the rename below replaces a map entry.
                let needs_rename = {
                    let map = frames.read().await;
                    match map.get(&frame_id) {
                        Some(existing) => {
                            let name_changed = new_name.is_some()
                                && new_name.as_deref() != existing.inner.name.as_deref();
                            // Write through the shared `Arc` on BOTH paths.
                            // Every `Frame` handle a caller already took
                            // points at THIS inner, and the rename below
                            // swaps the registry row for a fresh one those
                            // handles never see. Writing first means a
                            // pre-rename handle at least observes the
                            // navigation that carries the rename.
                            //
                            // It does not survive the swap. The NEXT
                            // `frameNavigated` resolves the new row and
                            // writes THAT inner, so a handle taken before
                            // the rename freezes at this URL forever. The
                            // repair is one navigation deep, not permanent;
                            // closing it needs interior-mutable
                            // `FrameInner::name`, which is a public break a
                            // maintainer has to sign off (see the ignored
                            // two-navigation test in this module).
                            *existing.inner.url.write().await = new_url.clone();
                            if !name_changed {
                                continue;
                            }
                            true
                        }
                        // No entry: treat the navigation as the implicit
                        // attach (Chrome may emit `frameNavigated` for the
                        // main frame before any subscriber sees an explicit
                        // `frameAttached`).
                        None => false,
                    }
                };

                let mut map = frames.write().await;
                if needs_rename {
                    // Re-look up. The registry is mutated concurrently by
                    // the OOPIF observer, so the row seen under the read
                    // guard may be gone by now. A name backfill is a refinement, never a
                    // reason to resurrect a row something else deliberately
                    // removed.
                    let Some(existing) = map.get(&frame_id) else {
                        continue;
                    };
                    // `FrameInner::name` is immutable (it backs
                    // `Frame::name() -> Option<&str>`, which cannot hand out
                    // a lock guard), so backfilling means replacing the
                    // registry entry. Everything else is carried over —
                    // notably the session, which for an OOPIF is a child
                    // session and NOT this subscriber's.
                    let renamed = Frame::new(
                        frame_id.clone(),
                        existing.inner.parent_frame_id.clone(),
                        new_url,
                        new_name,
                        existing.inner.session.clone(),
                        tab_weak.clone(),
                    );
                    map.insert(frame_id, renamed);
                } else {
                    // Re-look up, for the same reason the rename path does.
                    // The read guard said "no entry", but the OOPIF observer
                    // and the attach arm both insert into this map, and an
                    // OOPIF row carries a CHILD session. Inserting blind
                    // would overwrite one with a `Frame` on the parent
                    // session, sending every later call on that frame to the
                    // wrong target — so a row that appeared in the gap is
                    // refreshed in place instead.
                    //
                    // A name on this event is left to the next navigation:
                    // backfilling it means swapping the row, which is the
                    // thing being avoided here, and the rename path above
                    // does it correctly once the row is visible to a read
                    // guard.
                    let raced = map.get(&frame_id).map(|f| Arc::clone(&f.inner));
                    match raced {
                        Some(inner) => {
                            drop(map);
                            *inner.url.write().await = new_url;
                        }
                        None => {
                            let frame = Frame::new(
                                frame_id.clone(),
                                ev.frame.parent_id,
                                new_url,
                                new_name,
                                session.clone(),
                                tab_weak.clone(),
                            );
                            map.insert(frame_id, frame);
                        }
                    }
                }
            }
            "Page.frameDetached" => {
                let Some(ev) = decode::<FrameDetachedEvent>(&event) else {
                    continue;
                };
                frames.write().await.remove(&ev.frame_id);
            }
            _ => {}
        }
    }
}

/// `event`'s params as a `T`, or `None` (with a warning) if they don't fit.
fn decode<T: DeserializeOwned>(event: &RawEvent) -> Option<T> {
    match T::deserialize(&event.params) {
        Ok(decoded) => Some(decoded),
        Err(e) => {
            warn!(method = %event.method, error = %e, "frame::lifecycle: unreadable frame event; ignoring it");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use zendriver_transport::Connection;
    use zendriver_transport::testing::MockConnection;

    type Registry = Arc<RwLock<HashMap<String, Frame>>>;

    /// Session id every test drives events on — the subscriber only sees
    /// events routed to its own session.
    const SESSION: &str = "S1";

    /// Spawn the subscriber on a mock connection and drain its startup
    /// `Page.enable`. Once that command has landed the `Page.frame*`
    /// subscription is registered, so any later `emit_event_for_session`
    /// reaches the loop.
    async fn start() -> (MockConnection, Connection, Registry, CancellationToken) {
        let (mut mock, conn) = MockConnection::pair();
        let session = SessionHandle::new(conn.clone(), SESSION);
        let frames: Registry = Arc::new(RwLock::new(HashMap::new()));
        let cancel = CancellationToken::new();
        tokio::spawn(run(session, frames.clone(), Weak::new(), cancel.clone()));
        let id = tokio::time::timeout(Duration::from_secs(2), mock.expect_cmd("Page.enable"))
            .await
            .expect("frame lifecycle did not send Page.enable within 2s");
        mock.reply(id, json!({})).await;
        (mock, conn, frames, cancel)
    }

    async fn attach(mock: &MockConnection, frame_id: &str, parent: &str) {
        mock.emit_event_for_session(
            "Page.frameAttached",
            json!({ "frameId": frame_id, "parentFrameId": parent }),
            SESSION,
        )
        .await;
    }

    /// Poll the registry until `pred` holds, then return a snapshot. Panics
    /// after a second rather than asserting on a half-applied event.
    async fn wait_for(
        frames: &Registry,
        pred: impl Fn(&HashMap<String, Frame>) -> bool,
    ) -> HashMap<String, Frame> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            {
                let map = frames.read().await;
                if pred(&map) {
                    return map.clone();
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "registry never reached the expected state within a second",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Emit a `Page.frameNavigated` for `frame_id` on the test session.
    async fn navigate(mock: &MockConnection, frame_id: &str, url: &str, name: Option<&str>) {
        mock.emit_event_for_session(
            "Page.frameNavigated",
            json!({
                "frame": { "id": frame_id, "parentId": "P", "url": url, "name": name }
            }),
            SESSION,
        )
        .await;
    }

    /// Frame events are applied in the order Chrome sent them. Applied in
    /// another order, the detach of F1 could come before its attach and
    /// leave a frame in the registry that Chrome no longer has.
    #[tokio::test]
    async fn frame_events_are_applied_in_order() {
        let (mock, conn, frames, cancel) = start().await;
        for _ in 0..20 {
            attach(&mock, "F1", "P").await;
            mock.emit_event_for_session("Page.frameDetached", json!({ "frameId": "F1" }), SESSION)
                .await;
        }
        attach(&mock, "F2", "P").await;
        let map = wait_for(&frames, |m| m.contains_key("F2")).await;
        assert_eq!(map.len(), 1, "only F2 is left: {:?}", map.keys());

        cancel.cancel();
        conn.shutdown();
    }

    /// Two iframes under one parent, neither navigated yet, are two frames:
    /// Chrome never reports one frame under two ids, so a second attach
    /// leaves the first alone and asks Chrome nothing.
    #[tokio::test]
    async fn a_second_attach_under_one_parent_keeps_the_first() {
        let (mut mock, conn, frames, cancel) = start().await;
        attach(&mock, "F1", "P").await;
        attach(&mock, "F2", "P").await;
        let map = wait_for(&frames, |m| m.contains_key("F2")).await;
        assert!(map.contains_key("F1"));
        assert_eq!(mock.try_recv_cmd(), None);

        cancel.cancel();
        conn.shutdown();
    }

    #[tokio::test]
    async fn navigated_backfills_the_frame_name_onto_an_attached_entry() {
        let (mock, conn, frames, cancel) = start().await;

        attach(&mock, "FCHILD", "P").await;
        let map = wait_for(&frames, |m| m.contains_key("FCHILD")).await;
        assert_eq!(map["FCHILD"].name(), None, "attach cannot know the name");

        navigate(&mock, "FCHILD", "https://host.test/side", Some("sidebar")).await;

        let map = wait_for(&frames, |m| {
            m.get("FCHILD").is_some_and(|f| f.name().is_some())
        })
        .await;
        let frame = &map["FCHILD"];
        assert_eq!(frame.name(), Some("sidebar"));
        assert_eq!(frame.url().await, "https://host.test/side");
        assert_eq!(
            frame.parent_id(),
            Some("P"),
            "parent must survive the backfill"
        );

        cancel.cancel();
        conn.shutdown();
    }

    /// The backfill only ever adds a name. A later navigation that omits
    /// the field (Chrome does that for the unnamed case) must not erase the
    /// name a previous event established.
    #[tokio::test]
    async fn navigated_without_a_name_keeps_the_known_one() {
        let (mock, conn, frames, cancel) = start().await;

        attach(&mock, "FCHILD", "P").await;
        wait_for(&frames, |m| m.contains_key("FCHILD")).await;
        for (url, name) in [
            ("https://host.test/one", Some("sidebar")),
            ("https://host.test/two", None),
        ] {
            navigate(&mock, "FCHILD", url, name).await;
        }

        // The url is the marker for "the SECOND navigation landed".
        let map = wait_for(&frames, |m| {
            m.get("FCHILD").is_some_and(|f| {
                f.inner
                    .url
                    .try_read()
                    .is_ok_and(|u| *u == "https://host.test/two")
            })
        })
        .await;
        assert_eq!(map["FCHILD"].name(), Some("sidebar"));

        cancel.cancel();
        conn.shutdown();
    }

    /// Chrome emits `frameNavigated` for the main frame before any
    /// subscriber sees a `frameAttached` for it, so a navigation with no
    /// registry row is an implicit attach and has to insert one — off the
    /// event's own `parentId`, since there was no attach to take it from.
    ///
    /// The insert is guarded against a row that appeared while the arm was
    /// upgrading from the read guard to the write guard, and that guard is
    /// what this pins: the ordinary uncontended case must still insert.
    #[tokio::test]
    async fn navigated_for_an_unknown_frame_inserts_it() {
        let (mock, conn, frames, cancel) = start().await;

        navigate(&mock, "MAIN", "https://host.test/", Some("top")).await;

        let map = wait_for(&frames, |m| m.contains_key("MAIN")).await;
        let frame = &map["MAIN"];
        assert_eq!(frame.url().await, "https://host.test/");
        assert_eq!(frame.name(), Some("top"));
        assert_eq!(
            frame.parent_id(),
            Some("P"),
            "the implicit attach takes its parent from the navigation event",
        );

        cancel.cancel();
        conn.shutdown();
    }

    /// A `Frame` is a cheap `Arc` handle and `Tab::frames()` hands out
    /// clones, so holding one across navigations is the intended usage.
    ///
    /// The name backfill replaces the registry row with a fresh `Frame`, so
    /// a handle taken in the window between `frameAttached` and the first
    /// named `frameNavigated` points at an inner nothing would ever touch
    /// again. Writing the URL through that inner *before* the swap repairs
    /// exactly one navigation: the one carrying the rename, asserted here.
    ///
    /// It does **not** make the handle track the frame. Later navigations
    /// resolve the new row and leave this one frozen — see
    /// [`a_held_handle_stops_tracking_after_the_rename_swap`], which is the
    /// same scenario one navigation further along and is `#[ignore]`d
    /// because it cannot pass without the API break below.
    ///
    /// Only `url()` is repaired here. `Frame::name()` on the held handle
    /// still reads `None` forever, because `FrameInner::name` is immutable
    /// and making it interior-mutable turns `Frame::name()` into an `async
    /// fn` — a deliberate public break that is a maintainer's call, not
    /// this fix's.
    #[tokio::test]
    async fn a_handle_taken_before_the_name_backfill_still_tracks_the_url() {
        let (mock, conn, frames, cancel) = start().await;

        attach(&mock, "FCHILD", "P").await;
        let map = wait_for(&frames, |m| m.contains_key("FCHILD")).await;
        let held = map["FCHILD"].clone();
        assert_eq!(held.url().await, "", "nothing has navigated yet");

        navigate(
            &mock,
            "FCHILD",
            "https://host.test/checkout",
            Some("sidebar"),
        )
        .await;
        let map = wait_for(&frames, |m| {
            m.get("FCHILD").is_some_and(|f| f.name().is_some())
        })
        .await;
        assert_eq!(map["FCHILD"].url().await, "https://host.test/checkout");

        assert_eq!(
            held.url().await,
            "https://host.test/checkout",
            "a handle taken before the rename must not be orphaned on url()",
        );

        cancel.cancel();
        conn.shutdown();
    }

    /// The write-through's boundary, and the reason it is a stopgap rather
    /// than a fix: it repairs the navigation that carries the rename and
    /// nothing after it.
    ///
    /// Two navigations, one handle. The first renames the frame and swaps
    /// the registry row; the second resolves that new row and writes its
    /// inner. The held handle points at the old one, so it reads `/cart`
    /// while the registry reads `/checkout` — and
    /// `loop { if f.url().await.contains("/checkout") { break } }`, the
    /// obvious way to wait on an iframe, never terminates on that handle.
    ///
    /// **Ignored, not deleted.** Passing it requires interior-mutable
    /// `FrameInner::name` so the backfill mutates the shared inner instead
    /// of replacing the row, which turns `Frame::name()` into an `async fn`
    /// and breaks a public signature. That call is reserved for a
    /// maintainer (founder's review round 1, §5 item 2 — "`Frame::name()`
    /// and `Frame::id()` interior mutability"). Un-`#[ignore]` this the day
    /// the break is taken; it is the acceptance test for it.
    #[tokio::test]
    #[ignore = "needs interior-mutable FrameInner::name; reserved public API break"]
    async fn a_held_handle_stops_tracking_after_the_rename_swap() {
        let (mock, conn, frames, cancel) = start().await;

        attach(&mock, "FCHILD", "P").await;
        let held = wait_for(&frames, |m| m.contains_key("FCHILD")).await["FCHILD"].clone();

        // Navigation 1 carries the name, so it swaps the registry row.
        navigate(&mock, "FCHILD", "https://host.test/cart", Some("sidebar")).await;
        wait_for(&frames, |m| {
            m.get("FCHILD").is_some_and(|f| f.name().is_some())
        })
        .await;

        // Navigation 2 lands on the row the swap installed.
        navigate(&mock, "FCHILD", "https://host.test/checkout", None).await;
        let map = wait_for(&frames, |m| {
            m.get("FCHILD").is_some_and(|f| {
                f.inner
                    .url
                    .try_read()
                    .is_ok_and(|u| u.ends_with("/checkout"))
            })
        })
        .await;
        assert_eq!(map["FCHILD"].url().await, "https://host.test/checkout");

        assert_eq!(
            held.url().await,
            "https://host.test/checkout",
            "a handle must keep tracking the frame across every navigation, \
             not just the one that renamed it",
        );

        cancel.cancel();
        conn.shutdown();
    }
}
