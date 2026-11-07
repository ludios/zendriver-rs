# Mouse movement

This plan makes zendriver's realistic mouse moves move like a hand: `Element::click` and `hover`, `Tab::mouse_move` and `mouse_click`, and later the drags. It was written on 2026-11-07. Before writing it, I reviewed HumanMoveMouse, ghost-cursor, cursorflow, veilus-human-input and windmouse-rs, measured all of them with one harness, and probed Chrome 157 over raw CDP. The harness and the probe were in `/tmp/hmm-scripts/` and are not in the repo; "What I measured" gives the harness's inputs.

**Order:** items 0–8 in order. Item 0 comes first: it pins today's bug and measures native input. Item 1 doesn't depend on the path model. Item 5 waits for an answer to its open question. Item 7 needs items 1 and 2.

**Rule:** the same as in [TODO.md](TODO.md). Every fix lands with a test that fails before the fix, and behaviour that depends on Chrome is tested against real Chrome.

---

## Where we are

- [bezier.rs](crates/zendriver/src/input/bezier.rs) builds a cubic Bezier curve. Its two control points sit at 25% and 75% of the straight line, and each is moved by at most ±`jitter_amplitude_px` (2 px in `spoofed`). So the path strays at most about 2 px from the straight line, whatever the distance.
- [`move_realistic`](crates/zendriver/src/input/mouse.rs#L81) waits `segment length / mouse_speed_px_per_ms` before each segment. The speed is constant (1.5 px/ms in `spoofed`), the cursor never speeds up or slows down, and the duration grows in proportion to the distance.
- `move_realistic` also waits for Chrome's reply to each `Input.dispatchMouseEvent` before it starts the next wait. Chrome replies at the next frame boundary, and frames come every 16.7 ms (see "Chrome facts" below). So each step takes the longer of its sleep and the time to the next frame. A 600 px move is 60 steps with 6.67 ms sleeps, so it takes about 60 frames, ≈ 1.0 s, where `mouse_speed_px_per_ms` says 0.4 s. Replaying that schedule over raw CDP took 982 ms. Moves whose sleeps are longer than a frame (about 1,500 px and up) aren't slowed. The unit tests use a mocked connection and a paused clock, so they can't see any of this.
- [`InputProfile::overshoot_rate`](crates/zendriver-stealth/src/input_profile.rs#L32) (0.20 in `spoofed`) is never read anywhere. The phase-3 design spec promised overshoot, but it was never built.
- `click` and `hover` aim at the exact center of the element's bounding box.
- [`patches/mouse.js`](crates/zendriver-stealth/src/patches/mouse.js) is injected only with `spoofed` stealth (`ProfileKind::Spoofed`), not `native` or `off`.
  - On about 8 page loads in 11 (`rnd(0, 10) > 2`), each time the page adds a `mousemove` listener on `document`, it calls that listener with up to four events built with `new MouseEvent(...)`.
  - It also sends `mouseout`/`mouseover` pairs, gated the same way.
  - Such events have `isTrusted === false`, and its moves always go down and to the right.
  - It stops early once the page removes any listener from `document`.

## What I measured

Every generator ran in the same harness: 300 moves each, to a 40 × 40 px target. Every move started at (100, 900) and headed up and to the right at 20° (0.35 rad) above the horizontal. Moves used seeds 1000–1299, and each generator ran with its default settings; cursorflow drew a `MovementStyle::random` for each seed. Speeds were smoothed over 3 samples. The table shows medians, with the 10th–90th percentile range in parentheses where it's given.

- **peak/mean:** peak speed divided by average speed. A minimum-jerk movement gives 1.875; constant speed gives 1.0.
- **peak at:** when the speed peaks, as a fraction of the duration. A person's aimed movements peak a little before the middle.
- **sideways:** the largest distance from the straight line, divided by the length of the move.
- **overshoot:** the share of moves that pass the target by more than 2 px.

| Generator | Duration at 150 / 600 / 1200 px | peak/mean (600 px) | peak at (600 px) | sideways (600 px) | overshoot at 150 / 600 / 1200 px | Time between events |
|---|---|---|---|---|---|---|
| zendriver now, on paper | 100 / 400 / 800 ms | 1.00 | — (constant speed) | 0.001 | 0 / 0 / 0 | 3.5–14 ms, plus 16.7 ms per dispatch in real Chrome |
| ghost-cursor's `path()`, plus `move()`'s overshoot on moves over 500 px (my Rust copy; 16.7 ms per point, as in Chrome) | 752 / 1486 / 1503 ms | 2.53 (1.66–4.85) | 0.25 (0.01–0.67) | 0.147 | 0 / 0.54 / 0.54 | 16.7 ms |
| cursorflow 0.3.0 | 478 / 778 / 963 ms | 2.14 (1.96–2.67) | 0.39 (0.29–0.46) | 0.040 | 0.27 / 0.40 / 0.47 | 8.3 ms (each timestamp shifted by up to ±5% of that) |
| veilus-human-input 0.1.0 | 432 / 944 / 1144 ms | 1.84 (1.76–1.90) | 0.38 (0.33–0.44) | 0.122 | 0 / 0.83 / 0.98 | 8 ms |
| windmouse-rs 0.1.1 (verbatim copy with a seeded RNG) | 181 / 667 / 1322 ms | 1.08 | 0.46 (0.07–0.92) | 0.064 | 0.66 / 0.60 / 0.69 | 8 ms median |

The durations mix the model with how it's played back. ghost-cursor's are 16.7 ms per point only because it waits for every reply; under item 1's dispatcher its moves would be shorter. zendriver's row leaves out the reply waits that real Chrome adds.

### cursorflow 0.3.0: the best model, but orphaned

- **Status:** MIT. The crate exists only on crates.io: its GitHub repo and the author's account return 404, and the publisher (`waterflow256`) has no public repos. Four versions came out between 2026-05-29 and 2026-09-24, with 85 downloads in total and no dependents.
- **Dependencies:** rand 0.10 and rand_distr 0.6. zendriver's core uses rand 0.9; in this workspace only `rmcp`, in zendriver-mcp, pulls in rand 0.10. There's no `unsafe` and no build script.
- **Timing:** the primary movement's duration follows Fitts' law (0.16 s + 0.15 s per bit of `log2(distance / width + 1)`), with a log-normal spread.
- **Landing:** the landing error grows with speed (Meyer's optimized-submovement model). It is shifted so that `overshoot_tendency` is the chance that the primary movement lands past the aim point, with the setting clamped to 0.1–99.9%; a test checks this. That differs from how often the whole path passes the target, which corrections and blending also affect.
- **Corrections:** while the cursor rests outside the target, up to three corrections follow. Each one either blends into the end of the one before (half the time) or starts after a log-normal pause (median 85 ms, halved when the correction reverses direction). The last allowed one always lands inside the target.
- **Speed profile:** speed follows a Beta(3, b) curve, which is minimum-jerk when b = 3. It peaks at 36–46% of the movement, with optional small fluctuations.
- **Path shape:** a cubic Bezier curve walked by arc length. Its base bow is drawn from ranges between 3% and 28% of the distance (larger for longer moves). The bow is then scaled by precision, and by how the move's direction relates to the per-person arc direction, so actual bows can be smaller. 10% of curves are S-shaped. We measured the result as 4% of the distance at 600 px (median).
- **Noise:** slow hand wobble and 9 Hz tremor are added while the hand moves, and fade out over the last 60 ms.
- **Output:** samples at 120 Hz, each timestamp shifted by up to ±5% of the interval. Samples are rounded to whole pixels and clamped to the screen. A point target is treated as the one pixel its center rounds to, so its Fitts' law width is 1 px.
- **What we'd have to handle:**
  - During a correction's pause it repeats the same position. A mouse held still sends nothing, so those samples must go; `Trajectory::deduplicate()` drops them.
  - It lands anywhere inside the target, so the click has to happen where it lands.
  - It needs the screen size; we'd pass the viewport's.
- **Size:** about 1,040 lines of code without comments and tests. The parts we'd use come to about 600–700.

### veilus-human-input 0.1.0: simpler, sound, young

- **Status:** MIT OR Apache-2.0. Published 2026-10-11: one version, 16 downloads, and a public repo with 0 stars. Uses rand 0.8.
- **Path and timing:** a move is one or more legs, and each leg is one cubic Bezier curve. Its control points are pushed to one side by 5–25% of the distance (at most 120 px), and it uses smoothstep progress. The duration follows Fitts' law (100 ms + 150 ms per bit), and it emits one point per 8 ms tick, like a 125 Hz mouse.
- **Overshoot:** every leg over 500 px overshoots by 3–8% (at most 60 px) and corrects back almost straight (at most 4 px of bend), with no pause. Legs under 500 px never overshoot, a hard threshold that a classifier could learn. Detours split a move into legs, so a few moves under 500 px overshoot too.
- **Detours:** 20% of moves first wander through one or two waypoints.
- **Clicks:** at a random point in the middle half of the box. It hovers 10–80 ms before pressing and holds the button 80–140 ms. While the button is down there's a 20% chance of one or two steps of ±1 px on each axis.
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
- **It can hang.** In 5–7% of moves, my verbatim copy with a seeded RNG didn't come within 1 px of the target in 100,000 steps. The crate's loop has no step limit, so such calls can run arbitrarily long, possibly forever.

### HumanMoveMouse: reject

- **The model is narrow:** sideways shapes learned with PCA and Gaussian mixtures from 331 drags, apparently by one person, sampled at 60 Hz.
- **Its timing bug cancels the speed curve:** the speed stays constant, and the events bunch up microseconds apart at both ends.
- **No overshoot-and-correct phase:** the shape model only stores sideways offsets. Its random jitter at the end point can still land a pixel or two past the target.

## Chrome facts (Chrome 157, headless, raw CDP from Node)

- **Waiting for each reply:** each `Input.dispatchMouseEvent` call returns at the next frame boundary (frames come every 16.7 ms). A sleep shorter than a frame before a dispatch therefore adds nothing. Each run of 60 moves took 971–984 ms: back to back, at an 8 ms cadence, and on today's 600 px schedule (a 6.67 ms sleep, then a wait for the reply).
- **Not waiting for each reply:** the page sees `mousemove` events at our cadence. An 8 ms cadence gave 8.1 ms median gaps, and 2 ms gave 2.3 ms. Mostly one event arrived per dispatch: 50–58 events for 60 dispatches. Real hardware input may not look like this: Chrome aligns continuous input to frames and coalesces it ([Chrome's explanation](https://developer.chrome.com/blog/aligning-input-events/)). That's why item 0 also measures native input.
- **Screen coordinates:** the events are trusted. At page scale 1, natively, `screenX = clientX + window.screenX` and `screenY = clientY + window.screenY + (outerHeight − innerHeight)`; in the probe the offsets were 10 and 97 = 10 + 87. These relations don't hold under a device-metrics scale other than 1. zendriver's `screen.js` sets `window.screenX` and `window.screenY` to 0 and spoofs the outer and inner sizes, but the coordinates Chrome puts in events won't follow those spoofs. That mismatch is unverified; item 6 checks it.
- **Coalesced events:** `getCoalescedEvents()` exists only in secure contexts. A `pointermove` listener that called it threw on a `data:` URL and worked on 127.0.0.1. `zendriver-testkit`'s `*.test` sites are plain HTTP, so they aren't secure contexts either.
- **zendriver's transport:** `SessionHandle::call` is an `async fn`, so it sends nothing until it is polled. To keep several dispatches in flight, poll them together.

## Decisions (settled 2026-11-07)

1. **Port cursorflow's model** to `crates/zendriver/src/input/motion.rs`, on rand 0.9.
   - **Drop what we don't use:** the multi-target `generate` and `merge`, and the `Trajectory` column type. Keep clamping, but clamp to the viewport.
   - **Keep its statistical tests.**
   - **Licence:** keep cursorflow's whole MIT notice ("Copyright (c) 2026 xth24" plus the permission text) with the ported code, in the header of each ported file. Its upstream repo is gone, so take the text from the crates.io tarball of 0.3.0.
   - **Normal draws:** cursorflow draws normals through rand_distr 0.6, which needs rand 0.10. On rand 0.9, write a Box-Muller normal (about five lines, as veilus does) instead of adding rand_distr 0.5 to the core crate. Log-normal draws are `exp(normal)`. The Beta draw is used only by `merge`, which we drop.
   - **Rejected:** depending on `cursorflow = "=0.3.0"`, which would add rand 0.10 to the core crate with nobody upstream to fix bugs. Also rejected: a smaller generator of our own, which would leave out the corrections and tremor.
2. **Replace `InputProfile`'s mouse fields.** `mouse_speed_px_per_ms`, `jitter_amplitude_px` and `overshoot_rate` give way to the model's own settings: speed factor, precision, nervousness, overshoot tendency, maximum phases, jerk smoothness and arc direction. This is a breaking change to a public type.
3. **`click` and `hover` end where the cursor comes to rest.** That's a random point inside the element's box, like a person's, so the actionability gate hit-tests that point instead of the center.
4. **Replace `patches/mouse.js` with real CDP moves,** under the same condition (`spoofed` stealth). Moves through the new model produce trusted events, and then `mouse.js` is deleted. Real moves reach real page handlers, unlike `mouse.js`'s direct calls to listeners, so item 7 sets the limits.

## Work items

- [ ] **0. Pin today's timing, and measure native input, in real Chrome.**
  - **Fixture page:** add one to `zendriver-testkit` that records `mousemove` and `pointermove`: `timeStamp`, client and screen coordinates, `isTrusted`, and the coalesced count. Serve it from 127.0.0.1; `getCoalescedEvents()` doesn't exist on the plain-HTTP `*.test` sites.
  - **Pinning test:** run `Tab::mouse_move` over 600 px with `spoofed`, and check that the duration the page observes matches `mouse_speed_px_per_ms` (0.4 s).
    - Today it takes about 1.0 s, so name it `known_bug_*` and `#[ignore]` it until item 1 lands.
    - Once item 2 lands, change its check to: the observed duration matches the generated path's last offset within one frame, and the gaps between events match the planned cadence.
  - **Native input:** run headed Chrome under Xvfb, move the X pointer with `xdotool` (installed here), and record what the page sees: the gaps between events, and how many events each `pointermove` coalesces. That's how hardware-like input reaches a page through Chrome's frame alignment, and it decides item 1's cadence.
  - Tags: `chrome`.
- [ ] **1. Pace moves in real time, without waiting for Chrome's replies.** In `move_realistic`:
  - **Deadlines:** dispatch each point at an absolute deadline (`tokio::time::sleep_until(start + offset)`), so that the sleeps' rounding errors don't add up.
  - **In-flight calls:** keep the dispatched calls in a `FuturesUnordered` of `(index, reply)` that is polled alongside the sleep, so an error surfaces as soon as it arrives. A `FuturesOrdered` would hold it behind an earlier reply that hasn't come yet.
    - Cap it at about 8 calls in flight, so a stalled renderer can't build a queue.
    - Fail the move on the first error.
  - **Lateness:** when sends fall behind (the cap is reached, the renderer stalls, or the task wakes late), skip intermediate points whose deadline passed more than one interval ago, but always send the last point. Don't send the overdue points in a burst.
  - **Cursor position:** keep `pointer_x`/`pointer_y` true to Chrome's cursor after a failure or a cancellation.
    - Chrome handles the commands of a session in order, and a call that was already sent still runs after its future is dropped.
    - So record the last point Chrome confirmed, and finish the calls already sent before the next gesture builds its path.
  - **Cadence:** from item 0's native-input measurement.
  - **Checks:** this makes item 0's test pass. Also check headed Chrome and a background tab, where frames are throttled and replies may arrive late. Test the lateness rule with a mocked reply that is delayed.
  - Tags: `chrome`, `unit`.
- [ ] **2. Replace `input/bezier.rs` with the port of cursorflow's model** (decision 1).
  - **Output:** a list of `(offset from the move's start, x, y)` with `f64` coordinates.
  - **Pixel grid:** round intermediate points to the device-pixel grid (1/`devicePixelRatio` CSS px), as a real mouse reports them. When the caller gave an exact point, the last sample is that point.
  - **Targets:**
    - For `click`/`hover`, the target is the element's box, intersected with the viewport.
    - `mouse_move`/`mouse_click` aim at a point. For timing and landing error, give a point target an effective width of about 24 px; cursorflow's 1 px makes a 600 px move take about 1.9 s instead of 0.8 s. The final correction then ends exactly on the point.
  - **Viewport:** clamp every sample to the viewport.
  - **Repeats:** drop samples at the same position as the sample before them. A later return to an earlier position is fine, as when correcting an overshoot.
  - **First move:** a tab's first move starts at a random point on the viewport's edge, where a real cursor enters the window. Today it starts at (0, 0), which `InputController::new` sets.
  - **RNG:** draw from `InputController`'s RNG, so seeded tests are deterministic.
  - **`native()`:** keeps a near-instant path.
  - **Unit tests (statistical and seeded, asserting medians or shares over many seeds):**
    - The median duration grows with distance and shrinks with target size.
    - The median speed peak of primary strokes falls at 33–48% of the stroke. A single stroke may fall outside that range.
    - The primary landing lands past the aim point as often as `overshoot_tendency` says (cursorflow's own test). Test how often the whole path passes the target separately, as a range.
    - The cursor comes to rest inside the target.
    - No two consecutive samples share a position.
    - The same seed gives the same path.
  - Replace the `bezier_path_seed_42` snapshot.
  - Tags: `unit`.
- [ ] **3. `click` and `hover` end where the cursor comes to rest** (decision 3).
  - **First fix `Element::bounding_box`.** It takes its origin from the content box's top-left corner (`DOM.getBoxModel`'s `content` quad), but its width and height from the border box (`model.width/height`). Its box is therefore shifted by the padding and border, and that already moves today's "center" off the center.
  - **Order of steps:**
    1. Gate on visible, stable and enabled.
    2. Read the box.
    3. Generate the path.
    4. Hit-test the rest point. If it fails, draw a new path up to 5 times, then fall back to the center.
    5. Move.
    6. Hit-test the rest point again just before pressing, since `mousemove` handlers can change the page.
    7. Press and release there.

    `hover` uses the same order, with its own check and no press.
  - **Wording:** drop `ClickOptions`' "None clicks at the bbox center" wording.
  - **Explicit offsets:** an offset (`ClickOptions` with `Some((dx, dy))`) still ends exactly on that offset.
  - **Shadow roots:** the hit test doesn't descend into shadow roots, so an element inside one fails it. Fix it here, or file it in TODO.md.
  - Tags: `chrome`.
- [ ] **4. Replace `InputProfile`'s mouse fields with the model's settings** (decision 2).
  - `native()` keeps a near-instant path; `spoofed()`/`coherent()` take cursorflow's defaults.
  - Regenerate the public-API baseline, and update the MCP coverage ledger.
  - Check whether zendriver-mcp's `InputProfile` options (in `state.rs`) need new fields.
  - Tick "`overshoot_rate` unused" in TODO.md (Frame/input).
- [ ] **5. Realistic drags.**
  - **Today's drags:** `Tab::mouse_drag(from, to, steps)` is documented as the raw, straight-line drag, kept faithful to nodriver. `Element::mouse_drag` and `browser_mouse`'s `drag` action use it, and `steps` of 0 or 1 makes a single hop.
  - **Open question for the user:** either keep it unchanged and add a realistic drag beside it (`steps` keeps its meaning), or move `mouse_drag` itself to the model and decide what `steps` then means. The second changes a public contract and the MCP schema.
  - **Overshoot:** a realistic drag uses the model with a lower speed factor and higher precision, and doesn't overshoot by default. Overshooting during a drag fires `dragenter`/`dragover` on neighbouring drop zones, or moves a slider past its value.
  - Tags: `chrome`.
- [ ] **6. Check mouse events' screen coordinates under the persona.**
  - **Probe:** with stealth on, at page scale 1, check whether `e.screenX − e.clientX == window.screenX` and `e.screenY − e.clientY == window.screenY + (outerHeight − innerHeight)` hold.
  - **Control:** first run the same probe against unpatched Chrome with the same window and emulation settings, since scaling changes these relations natively.
  - **If they disagree under stealth only:** add a `KNOWN_DISAGREEMENTS` row, then make the spoofed window geometry agree with what Chrome puts in events.
  - **Don't patch `MouseEvent.prototype.screenX`/`screenY` instead.** That would replace native getters with JS functions, which is a worse tell.
  - Tags: `probe`.
- [ ] **7. Replace `patches/mouse.js` with real idle moves** (decision 4), after items 1 and 2.
  - **Gate:** the same as `mouse.js`'s: `spoofed` stealth, whatever the `InputProfile`. Record in the changelog that native/off stealth with a humanized `InputProfile` still gets no idle moves.
  - **Gesture ownership:** add a per-tab gesture lock.
    - A foreground gesture holds it for the whole gesture: a click's approach, press and release, and all of a drag. Today `click_at` releases `InputController`'s lock between those steps, and `mouse_drag` dispatches without it.
    - The idle task takes the gesture lock only when it's free, and gives it up between waypoints.
    - Callers set a "gesture waiting" flag before they wait for the lock, and the idle task stops at the next waypoint when it sees the flag. tokio's `Mutex` doesn't report waiters.
  - **Trigger:** `mouse.js` starts when the page adds a `mousemove` listener on `document`, which also covers sensors that load late. A one-time wander after load would miss those. Options:
    - (a) Wander once after each document loads.
    - (b) Have Rust poll `DOMDebugger.getEventListeners` on `document` after load, and wander when a `mousemove` listener appears. The page can't see this.

    Decide with a fixture that adds its listener late.
  - **Lifetime:** at most one idle job per tab. Cancel it when the document changes, and when the tab or connection shuts down.
  - **Side effects:** real moves hit real elements. They can open hover menus and tooltips, fire analytics, or cover the next click target.
    - Pick waypoints whose hit test lands on non-interactive elements: no link, button, form control or ARIA role up the tree.
    - Keep wanders short (one to three waypoints).
    - Document the remaining risk in the rustdoc.
  - **Test that fails first:** under `spoofed`, a fixture page records zero trusted `mousemove` events after load today, and at least one after this item (also for a listener added late).
  - **Delete:** remove `mouse.js` and its line in `patches.rs`.
  - **Check:** against a reese84/Incapsula fixture or site, which is what `mouse.js` was written for.
  - Tags: `chrome`.
- [ ] **8. Update the docs and MCP descriptions** (CLAUDE.md's three doc surfaces).
  - **READMEs:** `README.md` (e.g. line 130) and `crates/zendriver-mcp/README.md`, or record why one doesn't apply.
  - **mdBook:** `input.md` (it says "Bezier" on six lines), `quickstart.md:175`, `multi-tab.md:98` and `cloudflare.md:90`.
  - **MCP:**
    - The descriptions of `browser_click`, `browser_hover` and `browser_mouse` in [server.rs](crates/zendriver-mcp/src/server.rs).
    - `MouseAction::Move`'s doc in `tools/mouse.rs:25`, which feeds `schema_snapshots__mouse_in.snap` and `schema_snapshots__mouse_action.snap`.
    - The docs in `tools/actions.rs:252` and `:293`.
  - **Rustdoc:**
    - `Element::click`/`hover`/`click_fast`/`hover_fast`, and the `element/actions.rs` module docs.
    - `ClickOptions::realistic`.
    - `Tab::mouse_move`/`mouse_click`, both `mouse_drag`s, and `InputProfile`.
- [ ] **9. (Optional, needs a person) Calibrate against a real hand.**
  - Add a fixture page that asks a person to click a series of targets in headed Chrome, and records `pointermove` with `getCoalescedEvents()`. Serve it from 127.0.0.1.
  - Compare its distributions with the model's: duration against Fitts' index of difficulty, peak/mean speed, where the peak falls, and how often moves overshoot.

## Not pursuing

- **Porting HumanMoveMouse, windmouse-rs or ghost-cursor,** for the reasons above.
- **Setting CDP's `timestamp`.** Fake timestamps make `event.timeStamp` disagree with when the page actually handles the event, and pacing in real time (item 1) makes them unnecessary.

**Simplifications this enables:** `input/bezier.rs` goes away, and `InputProfile` loses three fields that misdescribe the behaviour; one of them does nothing at all.
