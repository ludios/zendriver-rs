//! A human-like click, for a click that a bot check scores by how the
//! pointer got there: a curved approach that speeds up and slows down, a
//! slight tremor on the way, a little jiggle on arrival, and a press held
//! for a moment.
//!
//! It ignores the tab's [`InputProfile`](zendriver_stealth::InputProfile),
//! whose default moves in a straight line as fast as the moves can be sent:
//! fine for automation that only needs the click to happen, and the motion
//! that such a check looks for.
//!
//! This generator is small on purpose and serves only `Tab::cloudflare`.
//! `TODO.mouse.md` plans a fuller model (corrections after an overshoot,
//! among others) for every realistic move; when it lands, this click should
//! move onto it.

use std::ops::Range;
use std::time::Duration;

use rand::Rng;
use serde_json::json;
use tokio::time::Instant;
use zendriver_transport::{CallError, SessionHandle};

use crate::input::InputController;
use crate::input::bezier::cubic_bezier;
use crate::input::pointer_state::MouseButtonSet;

/// One `mouseMoved`: when to send it, as an offset from the gesture's
/// start, and where it goes, in viewport CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Step {
    pub at: Duration,
    pub x: f64,
    pub y: f64,
}

/// One click, planned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Gesture {
    /// The approach, ending on the target's pixel, then the jiggle. The
    /// press and the release happen where the last move went.
    pub moves: Vec<Step>,
    /// How many of `moves` are the approach.
    pub approach: usize,
    /// The pause between the last move and the press.
    pub settle: Duration,
    /// How long the button stays down.
    pub hold: Duration,
}

/// The size of the viewport, in CSS pixels. Every move stays inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Viewport {
    pub width: f64,
    pub height: f64,
}

/// Milliseconds between two moves of the approach: about one frame at
/// 60 Hz, give or take scheduling.
const FRAME_MS: Range<u64> = 13..20;

/// How far the jiggle strays from the target before rounding, at most, in
/// CSS pixels along each axis. Small enough to stay inside a 24-pixel
/// checkbox aimed at its middle.
pub(crate) const JIGGLE_PX: f64 = 1.5;

/// A pointer closer than this to the target, in CSS pixels, has nothing to
/// approach: a second attempt starts where the first one's jiggle ended.
const APPROACH_MIN_PX: f64 = 5.0;

/// Append a move to `(x, y)` at `at` to `moves`, rounded to a whole pixel,
/// as a mouse reports at a device-pixel ratio of 1, and kept inside
/// `viewport`. A move to where the pointer already is is dropped: a mouse
/// held still sends nothing.
fn push(moves: &mut Vec<Step>, viewport: Viewport, at: Duration, x: f64, y: f64) {
    let x = x.round().clamp(0.0, viewport.width - 1.0);
    let y = y.round().clamp(0.0, viewport.height - 1.0);
    if moves.last().is_some_and(|last| (last.x, last.y) == (x, y)) {
        return;
    }
    moves.push(Step { at, x, y });
}

/// Plan a click at `to` for a pointer that is now at `from`, both in CSS
/// pixels of a viewport of size `viewport`. All randomness comes from
/// `rng`, so a seeded `rng` gives the same plan every time.
pub(crate) fn plan(
    from: (f64, f64),
    to: (f64, f64),
    viewport: Viewport,
    rng: &mut impl Rng,
) -> Gesture {
    let mut moves = Vec::new();
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let distance = dx.hypot(dy);
    // A non-finite target has no path to it; the press then fails in Chrome.
    if distance >= APPROACH_MIN_PX && distance.is_finite() {
        // Fitts's law: the time to reach a target grows with the logarithm
        // of its distance over its size. A Turnstile checkbox is about 24
        // pixels wide.
        let base_ms = 240.0 + 110.0 * (1.0 + distance / 24.0).log2();
        let duration_ms = base_ms * rng.random_range(0.85..1.25);
        // Bow the path to one side, as a pivoting wrist does: both control
        // points push the same way, the first one further.
        let normal = (-dy / distance, dx / distance);
        let side = if rng.random_bool(0.5) { 1.0 } else { -1.0 };
        let bow = side * rng.random_range(0.08..0.22) * distance.min(800.0);
        let c1 = (
            from.0 + dx * 0.3 + normal.0 * bow,
            from.1 + dy * 0.3 + normal.1 * bow,
        );
        let c2 = (
            from.0 + dx * 0.75 + normal.0 * bow * 0.4,
            from.1 + dy * 0.75 + normal.1 * bow * 0.4,
        );

        let mut elapsed_ms = 0;
        loop {
            elapsed_ms += rng.random_range(FRAME_MS);
            let t = (elapsed_ms as f64 / duration_ms).min(1.0);
            // Minimum-jerk progress along the path: a slow start, a fast
            // middle and a slow arrival.
            let progress = t * t * t * (10.0 + t * (6.0 * t - 15.0));
            let (x, y) = cubic_bezier(from, c1, c2, to, progress);
            // A tremor that fades out, so the approach ends on `to`.
            let tremor = 0.8 * (1.0 - t);
            let x = x + rng.random_range(-tremor..=tremor);
            let y = y + rng.random_range(-tremor..=tremor);
            push(
                &mut moves,
                viewport,
                Duration::from_millis(elapsed_ms),
                x,
                y,
            );
            if t >= 1.0 {
                break;
            }
        }
    }

    // The jiggle: a hand settling on the target before it presses.
    let approach = moves.len();
    let mut at = moves.last().map_or(Duration::ZERO, |step| step.at);
    for _ in 0..rng.random_range(2..=4) {
        at += Duration::from_millis(rng.random_range(30..90));
        let x = to.0 + rng.random_range(-JIGGLE_PX..=JIGGLE_PX);
        let y = to.1 + rng.random_range(-JIGGLE_PX..=JIGGLE_PX);
        push(&mut moves, viewport, at, x, y);
    }

    Gesture {
        moves,
        approach,
        settle: Duration::from_millis(rng.random_range(40..120)),
        hold: Duration::from_millis(rng.random_range(60..140)),
    }
}

/// A random point on the edge of `viewport`, where a pointer that was
/// outside the page enters it.
fn edge_point(viewport: Viewport, rng: &mut impl Rng) -> (f64, f64) {
    let x = rng.random_range(0.0..viewport.width).floor();
    let y = rng.random_range(0.0..viewport.height).floor();
    let right = viewport.width - 1.0;
    let bottom = viewport.height - 1.0;
    match rng.random_range(0..4) {
        0 => (x, 0.0),
        1 => (right, y),
        2 => (x, bottom),
        _ => (0.0, y),
    }
}

/// The size of the main frame's viewport, in CSS pixels, at least 1 by 1.
async fn viewport(session: &SessionHandle) -> Result<Viewport, CallError> {
    let metrics = session.call("Page.getLayoutMetrics", json!({})).await?;
    let size = &metrics["cssLayoutViewport"];
    Ok(Viewport {
        width: size["clientWidth"].as_f64().unwrap_or(1280.0).max(1.0),
        height: size["clientHeight"].as_f64().unwrap_or(720.0).max(1.0),
    })
}

/// Send one `Input.dispatchMouseEvent` of `kind` at `at` through `session`,
/// with `modifiers` and the button set `buttons`; `button` names the button
/// a press or a release is for.
async fn dispatch(
    session: &SessionHandle,
    kind: &str,
    at: (f64, f64),
    button: Option<&str>,
    modifiers: i32,
    buttons: MouseButtonSet,
) -> Result<(), CallError> {
    let mut params = json!({
        "type": kind, "x": at.0, "y": at.1,
        "modifiers": modifiers, "buttons": buttons.bits(),
    });
    if let Some(button) = button {
        params["button"] = json!(button);
        params["clickCount"] = json!(1);
    }
    session.call("Input.dispatchMouseEvent", params).await?;
    Ok(())
}

/// Left-click `(x, y)`, in viewport CSS pixels, with a gesture [`plan`]ned
/// from where `input` records this tab's pointer, sending it through
/// `session`. A pointer that was never moved on this tab is still at the
/// origin, where no hand rests; it enters at a random point on the
/// viewport's edge instead.
///
/// Each move is sent at its planned offset from the gesture's start, so the
/// wait for Chrome's reply to one move (Chrome answers at the next frame)
/// does not delay the ones after it.
///
/// Holds `input`'s lock for the whole gesture, so no other gesture that
/// takes the lock (the tab's other mouse moves and clicks) runs in the
/// middle of it, and records the pointer's position after each move. It
/// records the button as released after the release, and does not record
/// it as held in between: a gesture cut short between the press and the
/// release — by an error, or by its future being dropped — leaves Chrome
/// with the button down, and the record without it.
///
/// # Errors
/// The error of the first CDP call that failed.
pub(crate) async fn click(
    input: &InputController,
    session: &SessionHandle,
    x: f64,
    y: f64,
) -> Result<(), CallError> {
    let viewport = viewport(session).await?;
    let mut state = input.state.lock().await;
    let modifiers = state.modifiers_held.cdp_bits();
    let held = state.buttons_held;
    let start = Instant::now();

    let mut from = (state.pointer_x, state.pointer_y);
    if from == (0.0, 0.0) {
        from = edge_point(viewport, &mut state.rng);
        dispatch(session, "mouseMoved", from, None, modifiers, held).await?;
        state.pointer_x = from.0;
        state.pointer_y = from.1;
    }
    let gesture = plan(from, (x, y), viewport, &mut state.rng);

    for step in &gesture.moves {
        tokio::time::sleep_until(start + step.at).await;
        let at = (step.x, step.y);
        dispatch(session, "mouseMoved", at, None, modifiers, held).await?;
        state.pointer_x = step.x;
        state.pointer_y = step.y;
    }

    let at = (state.pointer_x, state.pointer_y);
    let last = gesture.moves.last().map_or(Duration::ZERO, |step| step.at);
    tokio::time::sleep_until(start + last + gesture.settle).await;
    let pressed = held | MouseButtonSet::LEFT;
    dispatch(
        session,
        "mousePressed",
        at,
        Some("left"),
        modifiers,
        pressed,
    )
    .await?;
    tokio::time::sleep(gesture.hold).await;
    let released = held.difference(MouseButtonSet::LEFT);
    dispatch(
        session,
        "mouseReleased",
        at,
        Some("left"),
        modifiers,
        released,
    )
    .await?;
    state.buttons_held = released;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::SmallRng;
    use serde_json::Value;
    use zendriver_stealth::InputProfile;
    use zendriver_transport::testing::MockConnection;

    use super::*;
    use crate::test_support::{drain_mouse_dispatches, expect};

    const VIEWPORT: Viewport = Viewport {
        width: 1000.0,
        height: 800.0,
    };
    const FROM: (f64, f64) = (700.0, 650.0);
    const TO: (f64, f64) = (46.0, 336.5);
    /// The pixel `TO` rounds to, where the approach ends.
    const TO_PIXEL: (f64, f64) = (46.0, 337.0);

    /// Distance of `point` from the straight line through `FROM` and `TO`.
    fn off_line(point: (f64, f64)) -> f64 {
        let dx = TO.0 - FROM.0;
        let dy = TO.1 - FROM.1;
        ((point.0 - FROM.0) * dy - (point.1 - FROM.1) * dx).abs() / dx.hypot(dy)
    }

    /// The approach, which ends on the target's pixel.
    fn approach(gesture: &Gesture) -> &[Step] {
        let approach = &gesture.moves[..gesture.approach];
        let end = approach.last().unwrap();
        assert_eq!(
            (end.x, end.y),
            TO_PIXEL,
            "the approach ends on the target's pixel"
        );
        approach
    }

    fn plan_with(seed: u64) -> Gesture {
        plan(FROM, TO, VIEWPORT, &mut SmallRng::seed_from_u64(seed))
    }

    /// The shape of the approach, over many seeds: it ends on the target's
    /// pixel, bows away from the straight line without wandering off, moves
    /// no faster than a 60 Hz pointer, and takes about as long as a person
    /// would.
    #[test]
    fn the_approach_is_a_curved_eased_path_onto_the_target() {
        let distance = (TO.0 - FROM.0).hypot(TO.1 - FROM.1);
        for seed in 0..50 {
            let gesture = plan_with(seed);
            let approach = approach(&gesture);

            let bow = approach
                .iter()
                .map(|step| off_line((step.x, step.y)))
                .fold(0.0, f64::max);
            assert!(
                bow > 0.03 * distance && bow < 0.25 * distance,
                "seed {seed}: the path strays {bow:.1}px from the line over {distance:.0}px"
            );

            let first_gap = approach[0].at;
            assert!(first_gap >= Duration::from_millis(FRAME_MS.start));
            for pair in approach.windows(2) {
                assert!(
                    pair[1].at - pair[0].at >= Duration::from_millis(FRAME_MS.start),
                    "seed {seed}: two moves less than a frame apart"
                );
            }
            let took = approach.last().unwrap().at;
            assert!(
                took > Duration::from_millis(500) && took < Duration::from_millis(1200),
                "seed {seed}: a {distance:.0}px approach took {took:?}"
            );

            // Eased: fastest in the middle half, and slow at both ends.
            let speeds: Vec<f64> = approach
                .windows(2)
                .map(|pair| {
                    let stride = (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y);
                    stride / (pair[1].at - pair[0].at).as_secs_f64()
                })
                .collect();
            let (fastest, top) = speeds
                .iter()
                .copied()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap();
            let quarter = speeds.len() / 4;
            assert!(
                (quarter..speeds.len() - quarter).contains(&fastest),
                "seed {seed}: the fastest move is #{fastest} of {}",
                speeds.len()
            );
            for end in [speeds[0], speeds[speeds.len() - 1]] {
                assert!(
                    end < 0.4 * top,
                    "seed {seed}: {end:.0} px/s at an end, {top:.0} px/s top"
                );
            }
        }
    }

    /// Every move is a whole pixel, inside the viewport, and somewhere other
    /// than the move before it — even for a path that bows out past the
    /// viewport's left edge.
    #[test]
    fn every_move_is_a_new_whole_pixel_inside_the_viewport() {
        for seed in 0..50 {
            let gesture = plan(
                (3.0, 700.0),
                (5.0, 40.0),
                VIEWPORT,
                &mut SmallRng::seed_from_u64(seed),
            );
            for step in &gesture.moves {
                assert_eq!((step.x.fract(), step.y.fract()), (0.0, 0.0));
                assert!((0.0..VIEWPORT.width).contains(&step.x));
                assert!((0.0..VIEWPORT.height).contains(&step.y));
            }
            for pair in gesture.moves.windows(2) {
                assert_ne!((pair[0].x, pair[0].y), (pair[1].x, pair[1].y));
            }
        }
    }

    /// The jiggle: at most 4 moves after the approach, each within the
    /// jiggle's reach of the target once rounded.
    #[test]
    fn the_jiggle_stays_on_the_target() {
        for seed in 0..50 {
            let gesture = plan_with(seed);
            let jiggle = &gesture.moves[gesture.approach..];
            assert!(jiggle.len() <= 4, "seed {seed}: {jiggle:?}");
            for step in jiggle {
                assert!((step.x - TO.0).abs() <= JIGGLE_PX + 0.5);
                assert!((step.y - TO.1).abs() <= JIGGLE_PX + 0.5);
            }
        }
    }

    /// A pointer a few pixels from the target — where a first attempt's
    /// jiggle leaves it for the next — has nothing to approach, and only
    /// jiggles.
    #[test]
    fn a_pointer_near_the_target_only_jiggles() {
        for seed in 0..20 {
            let near = (TO.0 + 2.0, TO.1 - 1.0);
            let gesture = plan(near, TO, VIEWPORT, &mut SmallRng::seed_from_u64(seed));
            assert!(gesture.moves.len() <= 4, "seed {seed}: {:?}", gesture.moves);
        }
    }

    /// A target at no finite distance has no path to it; the plan must still
    /// end rather than step towards it forever.
    #[test]
    fn a_non_finite_target_does_not_hang_the_plan() {
        let gesture = plan(
            FROM,
            (f64::INFINITY, 1.0),
            VIEWPORT,
            &mut SmallRng::seed_from_u64(1),
        );
        assert!(gesture.moves.len() <= 4);
    }

    #[test]
    fn a_seeded_plan_is_the_same_every_time() {
        assert_eq!(plan_with(3), plan_with(3));
    }

    /// The dispatch: a pointer never moved on this tab enters at the
    /// viewport's edge, then follows the plan; the press and the release
    /// come where the last move went, and the tab's pointer is left there
    /// with no button held.
    #[tokio::test(start_paused = true)]
    async fn click_enters_at_the_edge_then_sends_the_plan_and_a_held_press() {
        let input = InputController::new_with_seed(InputProfile::native(), 11);
        let (mut mock, conn) = MockConnection::pair();
        let session = SessionHandle::new(conn.clone(), "S1");

        let clicking = {
            let input = input.clone();
            tokio::spawn(async move { click(&input, &session, TO.0, TO.1).await })
        };
        let id = expect(&mut mock, "Page.getLayoutMetrics").await;
        let size = json!({ "clientWidth": VIEWPORT.width, "clientHeight": VIEWPORT.height });
        mock.reply(id, json!({ "cssLayoutViewport": size })).await;
        let sent = drain_mouse_dispatches(&mut mock).await;
        clicking.await.unwrap().unwrap();

        let at = |event: &Value| (event["x"].as_f64().unwrap(), event["y"].as_f64().unwrap());
        let kinds: Vec<&str> = sent.iter().map(|e| e["type"].as_str().unwrap()).collect();
        let moved = kinds.len() - 2;
        assert!(kinds[..moved].iter().all(|kind| *kind == "mouseMoved"));
        assert_eq!(kinds[moved..], ["mousePressed", "mouseReleased"]);

        let entry = at(&sent[0]);
        let on_edge = entry.0 == 0.0
            || entry.1 == 0.0
            || entry.0 == VIEWPORT.width - 1.0
            || entry.1 == VIEWPORT.height - 1.0;
        assert!(on_edge, "the pointer entered at {entry:?}");

        let last_move = at(&sent[moved - 1]);
        let press = &sent[moved];
        let release = &sent[moved + 1];
        assert_eq!((at(press), press["buttons"].as_i64()), (last_move, Some(1)));
        assert_eq!(
            (at(release), release["buttons"].as_i64()),
            (last_move, Some(0))
        );
        assert!(
            (last_move.0 - TO.0).abs() <= 2.0 && (last_move.1 - TO.1).abs() <= 2.0,
            "pressed at {last_move:?}, aimed at {TO:?}"
        );

        let state = input.state.lock().await;
        assert_eq!((state.pointer_x, state.pointer_y), last_move);
        assert!(state.buttons_held.is_empty());
        conn.shutdown();
    }
}
