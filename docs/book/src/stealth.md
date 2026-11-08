# Stealth

zendriver-rs ships with three stealth profiles selecting different
tradeoffs between what the page sees and how detectable the changes are.
Pick the profile that matches your target site's detection layer; tweak
the fingerprint with builder methods when you need to pin a specific
identity.

None of these profiles is a guarantee of invisibility — no automation
stack has one against a determined, adaptively-defended site. What they
provide is a coherent, plausible browser identity plus explicit controls
over the specific tells that mainstream fingerprint checks probe for.

## The three profiles

| Profile     | Launch flags | UA scrub | Emulation overrides | JS bootstrap   | Bypass CSP | Use case                                     |
|-------------|--------------|----------|---------------------|----------------|------------|----------------------------------------------|
| `off()`     | none         | no       | no                  | none           | no         | Reproducing issues in vanilla Chrome.       |
| `native()`  | yes          | yes      | yes                 | none           | opt-in     | Most sites, bot-protected ones included. Default recommendation. |
| `spoofed()` | yes          | yes      | yes                 | device values  | opt-in     | Pages that must see a device other than this one. |

### `StealthProfile::off()`

```rust,no_run
use zendriver::{Browser, StealthProfile};

# async fn ex() -> zendriver::Result<()> {
let browser = Browser::builder()
    .stealth(StealthProfile::off())
    .launch()
    .await?;
# Ok(()) }
```

No launch flags. No UA scrub. No CDP overrides. `Page.setBypassCSP` is
not called. This is what you get from a stock `chromiumoxide` launch.
Use this when you're debugging whether a bug reproduces in a vanilla
Chrome — if it does, the cause is unrelated to zendriver's stealth
machinery.

### `StealthProfile::native()`

```rust,no_run
use zendriver::{Browser, StealthProfile};

# async fn ex() -> zendriver::Result<()> {
let browser = Browser::builder()
    .stealth(StealthProfile::native())
    .launch()
    .await?;
# Ok(()) }
```

The default recommendation. Patches the layer that protocol-level
fingerprinters see, with launch flags and CDP commands, and runs no JS
in the page:

- **Launch flags** — `--disable-blink-features=AutomationControlled`,
  `--disable-features=IsolateOrigins,site-per-process` (toggleable),
  and a curated list of flags that turn off the "this browser is
  controlled by automation" infobar plus various leaks.
- **A mouse for headless pages on Linux** — headless Chrome on Linux
  has no input devices, so its pages match `(pointer: none)`,
  `(hover: none)`, `(any-pointer: none)` and `(any-hover: none)`: rare
  on a desktop, and flagged by bot detectors. No CDP command can make
  these media features report a mouse, so when zendriver launches a
  headless Chrome on Linux, it passes `--blink-settings` to make every
  frame see one: `fine` and `hover`, in both `matchMedia` and CSS
  `@media` rules. `navigator.maxTouchPoints` stays 0. Chrome reads only
  the last of several `--blink-settings` switches, so if you pass your
  own with `BrowserBuilder::arg`, zendriver puts its settings in front
  of those of your last switch, and your values win where both name a
  setting. zendriver adds no such settings to a headed launch, or on
  macOS (where Chrome always reports a mouse) or Windows (where Chrome
  reports the host's real devices, even when headless). Sending
  `Emulation.setTouchEmulationEnabled` with `enabled: false` yourself
  puts the page back at no pointer, until it navigates again.
- **UA scrub** — strips the `HeadlessChrome` segment from the User-Agent
  string and the Sec-CH-UA brand list.
- **Emulation overrides** — `Emulation.setUserAgentOverride` sets the
  User-Agent, the UA client hints (`navigator.userAgentData` and the
  `Sec-CH-UA-*` headers), `navigator.platform`, and the
  `Accept-Language` header and `navigator.languages`, so that Chrome's
  own getters report one coherent identity. The timezone, locale and
  geolocation overrides follow the profile and the persona.
- **A screen for headless Chrome** — headless Chrome's own screen is
  800x600, with no taskbar. So when zendriver launches a headless
  Chrome, it passes `--screen-info` for a 1920x1080 screen with a
  48-pixel taskbar (or the screen you configure with
  `StealthProfile::screen` or a persona's `screen`), and
  `--window-size` plus `--window-position`, which make every window fill
  that screen's work area. Chrome itself then reports a screen, window
  and viewport that agree: by default a 1920x1080 screen, a 1920x1032
  window at (0, 0), and a 1920x945 viewport (the window minus the
  browser's toolbars, which headless Chrome emulates; on Windows,
  1904x937, since headless Chrome also gives the window a border, 8
  pixels wide on each side with Windows' default window metrics, as
  Windows does to a window that isn't maximized). That holds for the tab
  in front of each window. On Linux, Chrome 157 gives a background tab's page its
  viewport as its window (by default at (0, 87), with `outerHeight`
  equal to `innerHeight`), headless or headed, even with no stealth.
  (Chrome 157 on Windows 11, under `native`, doesn't; macOS wasn't
  measured.) Chrome without stealth also hides a background tab, but
  zendriver's focus emulation keeps every tab visible and focused, on
  every platform. On Linux, a background tab's page is therefore visible
  and focused while it has a window that Chrome without stealth gives
  only to a hidden page. So on Linux, open each page that you work in at
  the same time as others with `Browser::new_window`, which puts it in
  front of a window of its own, not with `Browser::new_tab`. A
  `--screen-info` or `--window-size`
  that you pass with `BrowserBuilder::arg` wins. A headed launch gets
  none of these switches, since Chrome then reports the real display and
  window.
  Headless Chrome takes `--screen-info` from version 135 on Linux, 136
  on Windows and 138 on macOS. Chrome takes the screen in
  physical pixels, so a launch with a configured screen (and with no
  `--screen-info` of yours) fails unless the screen's
  `device_pixel_ratio` is 1, 1.25, 1.5, … or 4 on Linux and Windows
  (which covers Windows' scaling steps), or 1 or 2 elsewhere (fractional
  ratios were measured only on Linux and Windows); its work area isn't
  empty; and its sizes (`width`, `height`, and the work area's width and
  height) times that ratio are whole numbers of physical pixels from 1 to
  65536 (1536x864 at 1.25 is 1920x1080). Otherwise Chrome would report a
  different screen.

Since it runs no JS in the page, there's no patched function for a
`Function.prototype.toString` check to catch. Chrome itself already has
what bot detectors used to find missing in headless Chrome: the 5 PDF
plugins in a real `PluginArray`, `window.chrome`, and a
`Notification.permission` that agrees with `permissions.query`. On
[sannysoft], headless Chrome 157 under `native()` passed every check but
"WebGL Renderer", which flags its software renderer (SwiftShader); see
[GPU backend](./gpu-backend.md) for running it on the host's real GPU
instead.

[sannysoft]: https://bot.sannysoft.com/

### `StealthProfile::spoofed()`

```rust,no_run
use zendriver::{Browser, StealthProfile};

# async fn ex() -> zendriver::Result<()> {
let browser = Browser::builder()
    .stealth(StealthProfile::spoofed())
    .launch()
    .await?;
# Ok(()) }
```

`native()` plus what Chrome can't be told over CDP or at launch, patched
in the page by a JS bootstrap that `Page.addScriptToEvaluateOnNewDocument`
injects:

- WebGL vendor / renderer, plus every other readable WebGL value,
  resolved from one capability tier (defaults to a captured Apple Metal
  device: `"Google Inc. (Apple)"` / `"ANGLE (Apple, ANGLE Metal
  Renderer: Apple M4 Pro, Unspecified Version)"`). The WebGPU adapter is
  derived from that same renderer string, so the two APIs never name
  different GPUs.
- `navigator.deviceMemory`, which no CDP command overrides.
- Noise on canvas, audio and client-rect readback, and the fonts and
  hardware values, as the persona's surfaces say (see
  [Fingerprint spoofing](./fingerprint.md)).

It also sends `Emulation.setHardwareConcurrencyOverride`, so Chrome's own
`navigator.hardwareConcurrency` reports the persona's CPU count, and
`Emulation.setCPUPerformanceOverride`, so `navigator.cpuPerformance`
reports the tier that Chrome gives that count (from 1 for low to 4 for
ultra) instead of the host's tier. A dedicated worker still reports the
host's count and memory: no override or patch reaches it.

Everything else comes from Chrome itself, as under `native()`: the
launch flags hide `navigator.webdriver` and stop WebRTC from gathering
candidates that bypass a proxy; the Emulation overrides set the UA
client hints, `navigator.platform` and `navigator.languages`; and the
launch switches give headless Chrome a screen, window and viewport that
agree. A JS replacement of any of those would only add a detectable
difference from Chrome's own function.

Each patch that `spoofed()` does install replaces a Chrome function with
a JS one, and lie-detecting scripts such as creepjs find such
replacements by how they differ from Chrome's: for example, a getter
that doesn't throw for the wrong `this`, a function that can be called
with `new`, or a `toString` that an iframe's own `Function.prototype`
shows unmasked. So under such scripts, `spoofed()` is the more detectable
profile.

`Page.setBypassCSP` is off: the bootstrap installs even on a page whose
Content Security Policy forbids every script, because Chrome runs it
through DevTools. `.bypass_csp(true)`, under `spoofed()` or `native()`,
turns the bypass on, which a page can detect by loading a script that its
policy forbids.

On [sannysoft], headless Chrome 157 under `spoofed()` passed every check.
Pays a small per-navigation cost (the JS bootstrap runs on every new
document).

## Customizing the fingerprint

All three profiles return a builder that lets you override individual
fingerprint fields. The values are validated and clamped at resolve
time (e.g. `memory_gb` is clamped to a plausible W3C-rounded value;
`cpu_count` is clamped to `2..=32`).

```rust,no_run
use zendriver::{Browser, StealthProfile};
use zendriver::stealth::Platform;

# async fn ex() -> zendriver::Result<()> {
let profile = StealthProfile::spoofed()
    .memory_gb(8)               // navigator.deviceMemory
    .cpu_count(8)               // navigator.hardwareConcurrency
    .chrome_version(126)        // Chrome major in UA + Sec-CH-UA
    .platform(Platform::Win32)  // navigator.platform + OS in UA
    .locale("en-US")            // navigator.language + Accept-Language + --lang
    .timezone("America/New_York");

let browser = Browser::builder()
    .stealth(profile)
    .launch()
    .await?;
# Ok(()) }
```

`locale` alone is usually enough. It sets the `--lang` launch flag, the
JS-visible locale that `navigator.language` and `Intl` read, and an
`Accept-Language` derived from it (`en-US` gives `en-US,en;q=0.9`). Reach
for `.languages([...])` when you need the header to advertise a longer or
differently-weighted list. Its first entry then becomes
`navigator.language` as well, because Chrome always reports that as
`navigator.languages[0]`, and a locale sitting outside the advertised
list is a mismatch no real browser produces.

A [`Persona`](https://docs.rs/zendriver-stealth/latest/zendriver_stealth/struct.Persona.html)
carries the same `platform` / `timezone` / `locale` / `languages` /
`screen` fields, plus the CPU count and memory size
(`hardware_concurrency`, `device_memory_gb`), and whichever ones it sets
take precedence over the profile's. Anything
it leaves unset falls through to the values above, so a persona can pin
one axis without restating the rest.

You can also override the User-Agent string verbatim — useful when
you need an exact UA that doesn't match the auto-composed one:

```rust,no_run
use zendriver::{Browser, StealthProfile};

# async fn ex() -> zendriver::Result<()> {
let profile = StealthProfile::native()
    .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ...");

let browser = Browser::builder()
    .stealth(profile)
    .launch()
    .await?;
# Ok(()) }
```

`user_agent()` skips the auto-composition step entirely — prefer
`platform()` + `chrome_version()` unless you need a bit-for-bit
specific UA.

## Opting into real site isolation + real WebGL (`native_isolation`)

`native()` and `spoofed()` both disable Chrome's render-process site
isolation (`--disable-features=IsolateOrigins,...,site-per-process`),
and `spoofed()` additionally patches
`WebGLRenderingContext.getParameter()` /`getSupportedExtensions()` to
report a coherent GPU identity — every readable parameter resolved from
one capability tier, defaulting to an Apple Metal device — regardless of
the host's actual GPU. `.native_isolation(true)` opts a profile **out** of
both:

```rust,no_run
use zendriver::{Browser, StealthProfile};

# async fn ex() -> zendriver::Result<()> {
let profile = StealthProfile::spoofed().native_isolation(true);

let browser = Browser::builder()
    .stealth(profile)
    .launch()
    .await?;
# Ok(()) }
```

With this set:

- The launch flags omit `IsolateOrigins`/`site-per-process` from
  `--disable-features=...` — Chrome runs with its normal render-process
  isolation boundary. (The unrelated
  `DisableLoadExtensionCommandLineSwitch` feature name stays disabled
  either way — it controls `--load-extension`, not isolation.)
- For `spoofed()`, the bootstrap script omits the WebGL
  vendor/renderer patch entirely — `getParameter(UNMASKED_VENDOR_WEBGL)`
  etc. return the host's real values instead of the spoofed default.

**This is a trade-off, not a strict stealth improvement.** The defaults
it opts out of exist as anti-detection measures: the WebGL patch in
particular is an anti-WAF *coherence* defense — some WAFs
(Imperva/Incapsula) cross-check the WebGL identity against the rest of
the fingerprint and flag a real, host-specific renderer string as a bot
tell when it doesn't match. Reach for `native_isolation(true)` when you
need the host's actual GPU behavior (WebGL-heavy rendering, screenshot
fidelity, visual regression testing) or want Chrome's stock
process-isolation security boundary, and evasion isn't the priority —
not because it's "more stealthy." It isn't; it removes a defense.

It's off by default on every profile, so existing `native()` /
`spoofed()` callers see no behavior change unless they opt in
explicitly.

**WebGL and WebGPU stay coherent.** When `native_isolation` drops the
WebGL patch, the WebGPU **value** adapter spoof (`navigator.gpu`,
driven by the
[`Persona`](https://docs.rs/zendriver-stealth/latest/zendriver_stealth/struct.Persona.html)
`webgpu` surface) is skipped along with it, so `navigator.gpu` reports
the real host adapter instead of one derived from a renderer the WebGL
patch no longer applies — no cross-API mismatch. The same holds for the
per-surface `Strategy::Native` on `Webgl`, which leaves the real renderer
in place for one persona rather than the whole profile. An explicit
`Webgpu` `Block` (hiding `navigator.gpu`) is renderer-neutral, so it is
still honored if you set it.

## End-to-end example

This example launches with a custom UA, locale, and platform, then
reads them back via `navigator.*` to prove the overrides took:

```rust,no_run
{{#include ../../../crates/zendriver/examples/set_user_agent.rs}}
```

Expected output:

```text
My user agent
de
Win32
```

The override surface is intentionally narrow — anything that would let
you set incoherent values (e.g. Linux UA + `navigator.platform = Win32`)
goes through the Fingerprint resolver, which composes a coherent
identity.

## When to use which

- **Headless scraping of public sites** — `native()`.
- **Sites with active bot detection** (Cloudflare, PerimeterX,
  DataDome, Akamai) — start with `native()`, which gives such a site
  nothing to catch in the page, and consider a
  [real GPU](./gpu-backend.md) over headless Chrome's software renderer.
  Pair it with the `cloudflare` Cargo feature when you specifically need
  Turnstile bypass — see [Cloudflare](./cloudflare.md).
- **Pages that must see a device other than this one** (another GPU,
  memory size or canvas output) — `spoofed()`, accepting that its
  patches are JS replacements that a lie-detecting script can find.
- **Sites with strict CSP** — every profile works; the `spoofed()`
  bootstrap needs no `bypass_csp`.
- **Sites that check functions for tampering** (their
  `Function.prototype.toString` output, or their shape) — `native()`
  rather than `spoofed()`: `native()` replaces no function, while
  `spoofed()` replaces `Function.prototype.toString` and many WebGL,
  canvas and other functions. zendriver's bootstrap papers over the
  obvious differences, but a determined adversary will still find
  drift.
