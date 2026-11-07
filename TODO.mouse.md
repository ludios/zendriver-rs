# Mouse movement

This plan makes zendriver's realistic mouse moves move like a hand: `Element::click` and `hover`, `Tab::mouse_move` and `mouse_click`, and later the drags. It was written on 2026-11-07. Before writing it, I reviewed HumanMoveMouse, ghost-cursor, cursorflow, veilus-human-input and windmouse-rs, measured all of them with one harness, and probed Chrome 157 over raw CDP. The harness and the probe were in `/tmp/hmm-scripts/` and are not in the repo.

**Order:** do the items in order. Item 1 (pacing) does not depend on the path model and can land first. Item 7 needs items 1 and 2.

**Rule:** the same as in [TODO.md](TODO.md). Every fix lands with a test that fails before the fix, and behaviour that depends on Chrome is tested against real Chrome.

---

## Where we are

- [bezier.rs](crates/zendriver/src/input/bezier.rs) builds a cubic Bezier curve. Its two control points sit at 25% and 75% of the straight line, and each is moved by at most ±`jitter_amplitude_px` (2 px in `spoofed`). So the path strays at most about 2 px from the straight line, whatever the distance.
- [`move_realistic`](crates/zendriver/src/input/mouse.rs#L81) waits `segment length / mouse_speed_px_per_ms` before each segment. The speed is constant (1.5 px/ms in `spoofed`), the cursor never speeds up or slows down, and the duration grows in proportion to the distance.
- `move_realistic` also waits for Chrome's reply to each `Input.dispatchMouseEvent` before it starts the next wait. In Chrome, each reply arrives one frame later, about 16.7 ms (see "Chrome facts" below). So a 600 px move that should take 0.4 s takes about 0.4 s + 60 × 16.7 ms ≈ 1.4 s. I inferred this from the probe and the code; item 0 measures it. The unit tests use a mocked connection and a paused clock, so they can't see it.
- [`InputProfile::overshoot_rate`](crates/zendriver-stealth/src/input_profile.rs#L32) (0.20 in `spoofed`) is never read anywhere. The phase-3 design spec promised overshoot, but it was never built.
- `click` and `hover` aim at the exact center of the element's bounding box.
- [`patches/mouse.js`](crates/zendriver-stealth/src/patches/mouse.js) is injected on every stealth launch. It hands the page's `mousemove` listeners on `document` four events built with `new MouseEvent(...)`. Such events have `isTrusted === false`, and these four always move down and to the right.

## What I measured

Every generator ran in the same harness: 300 moves each, to a 40 px target. The table shows medians, with the 10th–90th percentile range in parentheses where it's given.

- **peak/mean:** peak speed divided by average speed. A minimum-jerk movement gives 1.875; constant speed gives 1.0.
- **peak at:** when the speed peaks, as a fraction of the duration. A person's aimed movements peak a little before the middle.
- **sideways:** the largest distance from the straight line, divided by the length of the move.
- **overshoot:** the share of moves that pass the target by more than 2 px.

| Generator | Duration at 150 / 600 / 1200 px | peak/mean (600 px) | peak at (600 px) | sideways (600 px) | overshoot at 150 / 600 / 1200 px | Time between events |
|---|---|---|---|---|---|---|
| zendriver now, on paper | 100 / 400 / 800 ms | 1.00 | — (constant speed) | 0.001 | 0 / 0 / 0 | 3.5–14 ms, plus 16.7 ms per dispatch in real Chrome |
| ghost-cursor's `path()` (my Rust copy; 16.7 ms per point, as in Chrome) | 752 / 1486 / 1503 ms | 2.53 (1.66–4.85) | 0.25 (0.01–0.67) | 0.147 | 0 / 0.54 / 0.54 | 16.7 ms |
| cursorflow 0.3.0 | 478 / 778 / 963 ms | 2.14 (1.96–2.67) | 0.39 (0.29–0.46) | 0.040 | 0.27 / 0.40 / 0.47 | 8.3 ms ± 5% |
| veilus-human-input 0.1.0 | 432 / 944 / 1144 ms | 1.84 (1.76–1.90) | 0.38 (0.33–0.44) | 0.122 | 0 / 0.83 / 0.98 | 8 ms |
| windmouse-rs 0.1.1 (verbatim copy with a seeded RNG) | 181 / 667 / 1322 ms | 1.08 | 0.46 (0.07–0.92) | 0.064 | 0.66 / 0.60 / 0.69 | 0–10 ms |

### cursorflow 0.3.0: the best model, but orphaned

- **Status:** MIT. The crate exists only on crates.io: its GitHub repo and the author's account return 404, and the publisher (`waterflow256`) has no public repos. Four versions came out between 2026-05-29 and 2026-09-24, with 85 downloads in total and no dependents.
- **Dependencies:** rand 0.10 and rand_distr 0.6. zendriver's core uses rand 0.9; in this workspace only `rmcp`, in zendriver-mcp, pulls in rand 0.10. There's no `unsafe` and no build script.
- **Timing:** the primary movement's duration follows Fitts' law (0.16 s + 0.15 s per bit of `log2(distance / width + 1)`), with a log-normal spread.
- **Landing:** the landing error grows with speed (Meyer's optimized-submovement model). It is shifted so that `overshoot_tendency` is exactly the chance of landing past the aim point; a test checks this.
- **Corrections:** while the cursor rests outside the target, up to three corrections follow. Each one either blends into the end of the one before (half the time) or starts after a log-normal pause (median 85 ms, halved when the correction reverses direction). The last allowed one always lands inside the target.
- **Speed profile:** speed follows a Beta(3, b) curve, which is minimum-jerk when b = 3. It peaks at 36–46% of the movement, with optional small fluctuations.
- **Path shape:** a cubic Bezier curve walked by arc length. It bows by 3–28% of the distance (more for longer moves), mostly toward one per-person direction, and 10% of curves are S-shaped.
- **Noise:** slow hand wobble and 9 Hz tremor are added while the hand moves, and fade out over the last 60 ms.
- **Output:** samples at 120 Hz with ±5% timing jitter, rounded to whole pixels and clamped to the screen.
- **What we'd have to handle:**
  - During a correction's pause it repeats the same position. A mouse held still sends nothing, so those samples must go; `Trajectory::deduplicate()` drops them.
  - It lands anywhere inside the target, so the click has to happen where it lands.
  - It needs the screen size; we'd pass the viewport's.
- **Size:** about 1,040 lines of code without comments and tests. The parts we'd use come to about 600–700.

### veilus-human-input 0.1.0: simpler, sound, young

- **Status:** MIT OR Apache-2.0. Published 2026-10-11: one version, 16 downloads, and a public repo with 0 stars. Uses rand 0.8.
- **Path and timing:** one cubic Bezier curve bent to one side by 5–25% of the distance (at most 120 px), with smoothstep progress. The duration follows Fitts' law (100 ms + 150 ms per bit), and it emits one point per 8 ms tick, like a 125 Hz mouse.
- **Overshoot:** every move over 500 px overshoots by 3–8% (at most 60 px) and corrects straight back with no pause. No move under 500 px ever overshoots, a hard threshold that a classifier could learn.
- **Detours:** 20% of moves first wander through one or two waypoints.
- **Clicks:** at a random point in the middle half of the box. It hovers 10–80 ms before pressing and holds the button 80–140 ms, with a 20% chance of a 1 px drift while the button is down.
- **Repeats:** it drops steps that don't move.
- **Also:** it has wheel-scrolling (notched, and macOS-accelerated) and typing-rhythm models. They are worth a separate look for `type_text` and scrolling.

### ghost-cursor (TypeScript): popular, but a weak model

- **Status:** MIT, 1,625 stars, still active.
- **Path shape:** a Bezier curve with both anchors on one side of the line. The spread is the distance clamped to 2–200 px, so a 150 px move can bow by more than half its length.
- **Speed:** points are evenly spaced in the curve's parameter, not along its length, so where the speed peaks is left to chance.
- **Duration:** the step count is `ceil(3 × (log2(fitts + 1) + 25 × random))`. The random term dominates, so the duration barely depends on distance or target size.
- **Pacing:** it doesn't wait between points. It awaits each `Input.dispatchMouseEvent`, so in Chrome each point takes one frame.
- **Overshoot:** on moves over 500 px it first goes to a random point within 120 px of the destination, then corrects with a 10 px spread.
- **Clicks:** at a random point in the box.
- **Timestamp bug:** with `useTimestamps`, it passes `Date.now()`, which is in milliseconds, as CDP's `timestamp`. Chrome reads that value as seconds since the epoch (`GetEventTimeTicks` in `content/browser/devtools/protocol/input_handler.cc`). The lesson for us: don't set `timestamp`, and pace in real time instead.
- **Detection:** The Web Scraping Club reports that it passes DataDome's mouse checks (I haven't verified that). That says more about how low the bar of some detectors is than about the model.

### windmouse-rs 0.1.1: reject

- **It doesn't compile with stable rand 0.9.** It imports `thread_rng` and `random` from the prelude, which only had them in the 0.9.0 alpha.
- **It can't be seeded.** It always uses the unseeded `thread_rng`.
- **It departs from Benjamin Land's WindMouse:**
  - x and y get the same random wind increment, whose mean is positive.
  - The initial wind is positive on both axes.
  - The step limit doesn't shrink near the target.
- **It can hang.** In 5–7% of moves, my verbatim copy with a seeded RNG never came within 1 px of the target in 100,000 steps. The crate's loop has no step limit, so those calls would never return.

### HumanMoveMouse: reject

- **The model is narrow:** sideways shapes learned with PCA and Gaussian mixtures from 331 drags, apparently by one person, sampled at 60 Hz.
- **Its timing bug cancels the speed curve:** the speed stays constant, and the events bunch up microseconds apart at both ends.
- **It can't overshoot:** the shape model only stores sideways offsets.

## Chrome facts (Chrome 157, headless, raw CDP from Node)

- **Waiting for each reply:** each `Input.dispatchMouseEvent` call returns one frame later (about 16.7 ms), whatever pacing we ask for. 60 moves took 971–981 ms, both at an 8 ms cadence and back to back.
- **Not waiting for each reply:** the page sees `mousemove` events at our cadence. An 8 ms cadence gave 8.1 ms median gaps, and 2 ms gave 2.3 ms. Mostly one event arrived per dispatch: 58 and 55 events for 60 dispatches.
- **Screen coordinates:** the events are trusted. Natively, `screenX = clientX + window.screenX` and `screenY = clientY + window.screenY + (outerHeight − innerHeight)`; in the probe the offsets were 10 and 97 = 10 + 87. zendriver's `screen.js` sets `window.screenX` and `window.screenY` to 0 and spoofs the outer and inner sizes, but the coordinates Chrome puts in events won't follow those spoofs. That mismatch is unverified; item 6 checks it.
- **Coalesced events:** a `pointermove` listener that calls `getCoalescedEvents()` threw on a `data:` URL and worked on 127.0.0.1.
- **zendriver's transport:** `Session::call` is an `async fn`, so it sends nothing until it is polled. To keep several dispatches in flight, poll them together, e.g. in a `FuturesOrdered`.

## Decisions (settled 2026-11-07)

1. **Port cursorflow's model** to `crates/zendriver/src/input/motion.rs`, on rand 0.9.
   - Drop what we don't use: the multi-target `generate` and `merge`, the `Trajectory` column type, and screen clamping.
   - Keep its statistical tests, and credit cursorflow (MIT) in the module's doc comment.
   - Rejected: depending on `cursorflow = "=0.3.0"`. That would add rand 0.10 to the core crate, with nobody upstream to fix bugs. Also rejected: a smaller generator of our own, which would leave out the corrections and tremor.
2. **Replace `InputProfile`'s mouse fields.** `mouse_speed_px_per_ms`, `jitter_amplitude_px` and `overshoot_rate` give way to the model's own settings: speed factor, precision, nervousness, overshoot tendency, maximum phases and arc direction. This is a breaking change to a public type.
3. **Click where the cursor comes to rest.** That's a random point inside the box, like a person's, so the actionability gate hit-tests that point instead of the center.
4. **Replace `patches/mouse.js` with real CDP moves.** A short idle wander through the new model produces trusted events, and then `mouse.js` is deleted.

## Work items

- [ ] **0. Pin today's timing in real Chrome.**
  - Add a fixture page to `zendriver-testkit` that records `mousemove` and `pointermove`: `timeStamp`, client and screen coordinates, `isTrusted`, and the coalesced count.
  - Add a test that runs `Tab::mouse_move` over 600 px with `spoofed`, and checks that the duration the page observes matches `mouse_speed_px_per_ms`.
  - It should fail today by about 1 s, so name it `known_bug_*` and `#[ignore]` it until item 1 lands.
  - Tags: `chrome`.
- [ ] **1. Pace moves in real time, without waiting for Chrome's replies.** In `move_realistic`:
  - Dispatch each point at an absolute deadline (`tokio::time::sleep_until(start + offset)`), so that the sleeps' rounding errors don't add up.
  - Keep the dispatched calls in flight in a `FuturesOrdered` that is polled alongside the sleep. Cap it at about 8 calls in flight, so a stalled renderer can't build a queue.
  - Fail the move on the first error, and update `pointer_x`/`pointer_y` only after every call has succeeded.
  - This makes item 0's test pass.
  - Also check headed Chrome and a background tab, where frames are throttled and replies may arrive late.
  - Tags: `chrome`, `unit`.
- [ ] **2. Replace `input/bezier.rs` with the port of cursorflow's model** (decision 1).
  - **Output:** a list of `(offset from the move's start, x, y)`.
  - **Requirements:**
    - Drop samples that don't move.
    - Draw from `InputController`'s RNG, so seeded tests are deterministic.
    - The target is the element's box for `click`/`hover`, and a point for `mouse_move`/`mouse_click`.
    - `native()` keeps a near-instant path.
  - **Unit tests (statistical and seeded):**
    - The duration grows with distance and shrinks with target size.
    - The speed peaks at 30–50% of each stroke.
    - Overshoots happen as often as the setting says.
    - The cursor comes to rest inside the target.
    - No position repeats.
    - The same seed gives the same path.
  - Replace the `bezier_path_seed_42` snapshot.
  - Tags: `unit`.
- [ ] **3. Click where the cursor comes to rest** (decision 3).
  - Generate the path first, hit-test the point where it comes to rest in the actionability gate, and press and release there.
  - Drop `ClickOptions`' "None clicks at the bbox center" wording.
  - An explicit offset (`ClickOptions` with `Some((dx, dy))`) still lands exactly on that offset.
  - Tags: `chrome`.
- [ ] **4. Replace `InputProfile`'s mouse fields with the model's settings** (decision 2).
  - `native()` keeps a near-instant path; `spoofed()`/`coherent()` take cursorflow's defaults.
  - Regenerate the public-API baseline, and update the MCP coverage ledger.
  - Check whether zendriver-mcp's `InputProfile` options (in `state.rs`) need new fields.
- [ ] **5. Move drags to the same model.** `Tab::mouse_drag` and `Element::mouse_drag` currently interpolate in a straight line over `steps`. Drags are slower and straighter than free moves, so reuse the model with a lower speed factor and higher precision rather than writing new code. Tags: `chrome`.
- [ ] **6. Check mouse events' screen coordinates under the persona.**
  - With stealth on, probe whether `e.screenX − e.clientX == window.screenX` and `e.screenY − e.clientY == window.screenY + (outerHeight − innerHeight)` hold.
  - If they don't, add a `KNOWN_DISAGREEMENTS` row, then make the spoofed window geometry agree with what Chrome puts in events.
  - Don't patch `MouseEvent.prototype.screenX`/`screenY` instead. That would replace native getters with JS functions, which is a worse tell.
  - Tags: `probe`.
- [ ] **7. Replace `patches/mouse.js` with a real idle wander** (decision 4), after items 1 and 2.
  - With humanized input on, after a navigation's document loads, move the cursor through one to three random points in the viewport, using the new model.
  - Take `InputController`'s lock for each move, so the wander can't interleave with a move the caller asks for. Stop the wander as soon as a caller's move is waiting.
  - Then delete `mouse.js`, and its line in `patches.rs`.
  - Open question for the implementer: what to hook. Candidates are `Page.loadEventFired` and the end of `goto`; it must also cover navigations the page starts itself.
  - Check against a reese84/Incapsula fixture or site, which is what `mouse.js` was written for.
  - Tags: `chrome`.
- [ ] **8. Update the docs and MCP descriptions.**
  - The mdBook's `input.md` (it says "Bezier" on six lines), `quickstart.md:175` and `multi-tab.md:98`.
  - The descriptions of `browser_click`, `browser_hover` and `browser_mouse` in [server.rs](crates/zendriver-mcp/src/server.rs), plus the schema snapshots if any input or output type changes.
  - The rustdoc of `Element::click`/`hover`, `Tab::mouse_move`/`mouse_click`, and `InputProfile`.
- [ ] **9. (Optional, needs a person) Calibrate against a real hand.**
  - Add a fixture page that asks a person to click a series of targets in headed Chrome, and records `pointermove` with `getCoalescedEvents()`. Serve it from 127.0.0.1, not a `data:` URL.
  - Compare its distributions with the model's: duration against Fitts' index of difficulty, peak/mean speed, where the peak falls, how often moves overshoot, and the event cadence and coalescing.
  - This also shows how a real 125 Hz or 1000 Hz mouse's events reach the page, which decides item 1's cadence.

## Not pursuing

- **Porting HumanMoveMouse, windmouse-rs or ghost-cursor,** for the reasons above.
- **Setting CDP's `timestamp`.** Fake timestamps make `event.timeStamp` disagree with when the page actually handles the event, and pacing in real time (item 1) makes them unnecessary.

**Simplifications this enables:** `input/bezier.rs` goes away, and `InputProfile` loses three fields that misdescribe the behaviour; one of them does nothing at all.
