//! A click shaped like a person's, for a click that a bot check scores by
//! how the pointer got there: a curved approach that speeds up and slows
//! down, a slight tremor on the way, a little jiggle on arrival, and a press
//! held for a moment.
//!
//! It ignores the tab's [`InputProfile`](zendriver_stealth::InputProfile).
//! The default profile moves in a straight line at 10 px/ms, which suits
//! automation that only needs the click to happen, and is the motion that
//! such a check looks for.

use std::ops::Range;
use std::time::Duration;

use rand::Rng;
use serde_json::json;
use zendriver_transport::{CallError, SessionHandle};

use crate::input::InputController;
use crate::input::bezier::cubic_bezier;
use crate::input::pointer_state::MouseButtonSet;

/// One `mouseMoved`: how long to wait before sending it, and where it goes,
/// in viewport CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Step {
    pub wait: Duration,
    pub x: f64,
    pub y: f64,
}

/// One click, planned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Gesture {
    /// The approach, ending exactly on the target, and then the jiggle. The
    /// press and the release happen where the last move went.
    pub moves: Vec<Step>,
    /// The pause between the last move and the press.
    pub settle: Duration,
    /// How long the button stays down.
    pub hold: Duration,
}

/// Milliseconds between two moves of the approach: a 60 Hz pointer, give or
/// take scheduling.
const FRAME_MS: Range<u64> = 13..20;

/// The farthest the arrival jiggle strays from the target, in CSS pixels.
/// Small enough to stay inside a 24-pixel checkbox aimed at its middle.
pub(crate) const JIGGLE_PX: f64 = 1.5;

/// Plan a click at `to` for a pointer that is now at `from`, both in
/// viewport CSS pixels. All randomness comes from `rng`, so a seeded `rng`
/// gives the same plan every time.
pub(crate) fn plan(from: (f64, f64), to: (f64, f64), rng: &mut impl Rng) -> Gesture {
    let mut moves = Vec::new();
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let distance = dx.hypot(dy);
    if distance >= 1.0 {
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

        let mut elapsed_ms = 0.0;
        loop {
            let wait = rng.random_range(FRAME_MS);
            elapsed_ms += wait as f64;
            let t = (elapsed_ms / duration_ms).min(1.0);
            // Minimum-jerk progress along the path: a slow start, a fast
            // middle and a slow arrival.
            let progress = t * t * t * (10.0 + t * (6.0 * t - 15.0));
            let (x, y) = cubic_bezier(from, c1, c2, to, progress);
            // A tremor that fades out, so the approach ends exactly on `to`.
            let tremor = 0.8 * (1.0 - t);
            moves.push(Step {
                wait: Duration::from_millis(wait),
                x: x + rng.random_range(-tremor..=tremor),
                y: y + rng.random_range(-tremor..=tremor),
            });
            if t >= 1.0 {
                break;
            }
        }
    }

    // The jiggle: a hand settling on the target before it presses.
    for _ in 0..rng.random_range(2..=4) {
        moves.push(Step {
            wait: Duration::from_millis(rng.random_range(30..90)),
            x: to.0 + rng.random_range(-JIGGLE_PX..=JIGGLE_PX),
            y: to.1 + rng.random_range(-JIGGLE_PX..=JIGGLE_PX),
        });
    }

    Gesture {
        moves,
        settle: Duration::from_millis(rng.random_range(40..120)),
        hold: Duration::from_millis(rng.random_range(60..140)),
    }
}

/// Left-click `(x, y)`, in viewport CSS pixels, with a gesture [`plan`]ned
/// from where `input` records the pointer to be, sending it through
/// `session`.
///
/// Holds `input`'s lock for the whole gesture, as the other pointer gestures
/// do, so that no other input on the tab interleaves with it, and records
/// the pointer's position after each move. It does not record the button as
/// held: with the lock held, nothing can read that record mid-gesture, and a
/// gesture cut short between the press and the release — by an error, or by
/// its future being dropped — then leaves the record as it was, rather than
/// a button held for the rest of the tab's life.
///
/// # Errors
/// The error of the first `Input.dispatchMouseEvent` that failed.
pub(crate) async fn click(
    input: &InputController,
    session: &SessionHandle,
    x: f64,
    y: f64,
) -> Result<(), CallError> {
    let mut state = input.state.lock().await;
    let from = (state.pointer_x, state.pointer_y);
    let gesture = plan(from, (x, y), &mut state.rng);
    let modifiers = state.modifiers_held.cdp_bits();
    let held = state.buttons_held;

    for step in &gesture.moves {
        tokio::time::sleep(step.wait).await;
        session
            .call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved", "x": step.x, "y": step.y,
                    "modifiers": modifiers, "buttons": held.bits(),
                }),
            )
            .await?;
        state.pointer_x = step.x;
        state.pointer_y = step.y;
    }

    let at = (state.pointer_x, state.pointer_y);
    tokio::time::sleep(gesture.settle).await;
    session
        .call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed", "x": at.0, "y": at.1,
                "button": "left", "clickCount": 1,
                "modifiers": modifiers, "buttons": (held | MouseButtonSet::LEFT).bits(),
            }),
        )
        .await?;
    tokio::time::sleep(gesture.hold).await;
    session
        .call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased", "x": at.0, "y": at.1,
                "button": "left", "clickCount": 1,
                "modifiers": modifiers, "buttons": held.bits(),
            }),
        )
        .await?;
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

    const FROM: (f64, f64) = (700.0, 650.0);
    const TO: (f64, f64) = (46.0, 336.0);

    /// Distance of `point` from the straight line through `FROM` and `TO`.
    fn off_line(point: (f64, f64)) -> f64 {
        let (dx, dy) = (TO.0 - FROM.0, TO.1 - FROM.1);
        ((point.0 - FROM.0) * dy - (point.1 - FROM.1) * dx).abs() / dx.hypot(dy)
    }

    /// The approach: every move but the jiggle, which is the last 2 to 4.
    /// The approach is the moves up to and including the one exactly on
    /// `TO`.
    fn approach(gesture: &Gesture) -> &[Step] {
        let end = gesture
            .moves
            .iter()
            .rposition(|step| (step.x, step.y) == TO)
            .expect("the approach ends exactly on the target");
        &gesture.moves[..=end]
    }

    /// The shape of the approach, over many seeds: it ends exactly on the
    /// target, bows away from the straight line without wandering off,
    /// moves at a 60 Hz pointer's cadence, and takes about as long as a
    /// person would.
    #[test]
    fn the_approach_is_a_curved_eased_path_onto_the_target() {
        let distance = (TO.0 - FROM.0).hypot(TO.1 - FROM.1);
        for seed in 0..50 {
            let gesture = plan(FROM, TO, &mut SmallRng::seed_from_u64(seed));
            let approach = approach(&gesture);

            let bow = approach
                .iter()
                .map(|step| off_line((step.x, step.y)))
                .fold(0.0, f64::max);
            assert!(
                bow > 0.03 * distance && bow < 0.25 * distance,
                "seed {seed}: the path strays {bow:.1}px from the line over {distance:.0}px"
            );

            assert!(
                approach
                    .iter()
                    .all(|step| FRAME_MS.contains(&(step.wait.as_millis() as u64))),
                "seed {seed}: a move off the 60 Hz cadence"
            );
            let took: Duration = approach.iter().map(|step| step.wait).sum();
            assert!(
                took > Duration::from_millis(500) && took < Duration::from_millis(1200),
                "seed {seed}: a {distance:.0}px approach took {took:?}"
            );

            // Eased: fastest in the middle half, and slow at both ends.
            let speeds: Vec<f64> = approach
                .windows(2)
                .map(|pair| {
                    let stride = (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y);
                    stride / pair[1].wait.as_secs_f64()
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

    /// The jiggle: 2 to 4 small moves around the target after the approach,
    /// none farther than `JIGGLE_PX` from it.
    #[test]
    fn the_jiggle_stays_on_the_target() {
        for seed in 0..50 {
            let gesture = plan(FROM, TO, &mut SmallRng::seed_from_u64(seed));
            let jiggle = &gesture.moves[approach(&gesture).len()..];
            assert!((2..=4).contains(&jiggle.len()), "seed {seed}: {jiggle:?}");
            for step in jiggle {
                assert!((step.x - TO.0).abs() <= JIGGLE_PX && (step.y - TO.1).abs() <= JIGGLE_PX);
            }
        }
    }

    /// A pointer already on the target has nothing to approach, and still
    /// jiggles before it presses.
    #[test]
    fn a_pointer_on_the_target_only_jiggles() {
        let gesture = plan(TO, TO, &mut SmallRng::seed_from_u64(7));
        assert!((2..=4).contains(&gesture.moves.len()));
    }

    #[test]
    fn a_seeded_plan_is_the_same_every_time() {
        let a = plan(FROM, TO, &mut SmallRng::seed_from_u64(3));
        let b = plan(FROM, TO, &mut SmallRng::seed_from_u64(3));
        assert_eq!(a, b);
    }

    /// The dispatch: the planned moves in order, then a press and a release
    /// where the last move went, and the tab's pointer left there.
    #[tokio::test(start_paused = true)]
    async fn click_sends_the_planned_moves_then_a_held_press() {
        let input = InputController::new_with_seed(InputProfile::native(), 11);
        let expected = {
            let mut state = input.state.lock().await;
            state.pointer_x = FROM.0;
            state.pointer_y = FROM.1;
            plan(FROM, TO, &mut state.rng.clone())
        };
        let (mut mock, conn) = MockConnection::pair();
        let session = SessionHandle::new(conn.clone(), "S1");

        let clicking = {
            let input = input.clone();
            tokio::spawn(async move { click(&input, &session, TO.0, TO.1).await })
        };
        // Where an event went. Read back through `serde_json`, which can
        // parse a float one unit in the last place off what it wrote, so
        // compared to the plan within a nanopixel.
        let at = |event: &Value| (event["x"].as_f64().unwrap(), event["y"].as_f64().unwrap());
        let near =
            |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9;

        let mut sent = Vec::new();
        while let Some((method, id)) = mock.recv_cmd_timeout(Duration::from_secs(5)).await {
            assert_eq!(method, "Input.dispatchMouseEvent");
            sent.push(mock.last_sent()["params"].clone());
            mock.reply(id, json!({})).await;
        }
        clicking.await.unwrap().unwrap();

        let last = expected.moves.last().unwrap();
        assert_eq!(sent.len(), expected.moves.len() + 2);
        for (params, step) in sent.iter().zip(&expected.moves) {
            assert_eq!(params["type"], "mouseMoved");
            assert!(
                near(at(params), (step.x, step.y)),
                "{params} is not {step:?}"
            );
            assert_eq!(params["buttons"], 0);
        }
        let press = &sent[sent.len() - 2];
        let release = &sent[sent.len() - 1];
        assert_eq!(
            (press["type"].as_str(), press["buttons"].as_i64()),
            (Some("mousePressed"), Some(1))
        );
        assert_eq!(
            (release["type"].as_str(), release["buttons"].as_i64()),
            (Some("mouseReleased"), Some(0))
        );
        for event in [press, release] {
            assert!(
                near(at(event), (last.x, last.y)),
                "{event} is not at {last:?}"
            );
            assert_eq!(event["button"], "left");
        }

        let state = input.state.lock().await;
        assert_eq!((state.pointer_x, state.pointer_y), (last.x, last.y));
        assert!(state.buttons_held.is_empty());
        conn.shutdown();
    }
}
