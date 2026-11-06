# Stealth

zendriver-rs ships with three stealth profiles selecting different
tradeoffs between launch overhead, detectability, and CSP compatibility.
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
| `native()`  | yes          | yes      | yes                 | none           | no         | Most sites. Default recommendation.         |
| `spoofed()` | yes          | yes      | yes                 | Navigator JS   | yes (on)   | Sites with active fingerprint detection.    |

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
- **Emulation overrides** —
  `Emulation.setUserAgentOverride` /
  `Emulation.setHardwareConcurrencyOverride` set a coherent identity.
- **A screen for headless Chrome** — headless Chrome's own screen is
  800x600, with no taskbar. So when zendriver launches a headless
  Chrome, it passes `--screen-info` for a 1920x1080 screen with a
  48-pixel taskbar (or the screen you configure with
  `StealthProfile::screen` or a persona's `screen`), and
  `--window-size` plus `--window-position`, which make every window fill
  that screen's work area. Chrome itself then reports a screen, window
  and viewport that agree: by default a 1920x1080 screen, a 1920x1032 window at (0, 0),
  and a 1920x945 viewport (the window minus the browser's toolbars,
  which headless Chrome emulates). A `--screen-info` or `--window-size`
  that you pass with `BrowserBuilder::arg` wins. A headed launch gets
  none of these switches, since Chrome then reports the real display and
  window.
  `--screen-info` needs Chrome 133 or later. Chrome takes the screen in
  physical pixels, so a configured screen's `device_pixel_ratio` must be
  a multiple of 0.25 from 1 (as Windows' scaling steps and macOS's 2x
  are), and its sizes (`width`, `height`, and the work area's width and
  height) times that ratio must be whole numbers (1536x864 at 1.25 is
  1920x1080); otherwise the launch fails, since Chrome would report a
  different screen.

Since it runs no JS in the page, there's no patched function for a
`Function.prototype.toString` check to catch.
Passes most consumer site detectors. Doesn't pass [sannysoft]'s deeper
Navigator-prototype checks.

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

`native()` plus Navigator-prototype JS patches injected via
`Page.addScriptToEvaluateOnNewDocument`. Restores or overrides:

- `navigator.webdriver` (deletes it so `'webdriver' in navigator` is
  `false`).
- `navigator.permissions.query({ name: "notifications" })` (returns
  `"prompt"` instead of the headless-Chrome `"denied"`).
- `navigator.plugins` + `navigator.mimeTypes` (returns plausible-length
  arrays).
- `navigator.chrome` (installs the runtime object headless Chrome
  doesn't ship).
- WebGL vendor / renderer, plus every other readable WebGL value,
  resolved from one capability tier (defaults to a captured Apple Metal
  device: `"Google Inc. (Apple)"` / `"ANGLE (Apple, ANGLE Metal
  Renderer: Apple M4 Pro, Unspecified Version)"`). The WebGPU adapter is
  derived from that same renderer string, so the two APIs never name
  different GPUs.
- ChunkSplit + iframe-contentWindow guards so the patches survive
  cross-realm escape attempts.

Toggles `Page.setBypassCSP` **on by default** so the bootstrap script
can install on pages with strict CSP headers. Pass
`.bypass_csp(false)` to opt out when you want to test against a real
CSP-restricted page.

Passes [sannysoft], [areyouheadless], and most active detectors. Pays
a small per-navigation cost (the JS bootstrap runs on every new
document).

[areyouheadless]: https://arh.antoinevastel.com/bots/areyouheadless

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
carries the same `timezone` / `locale` / `languages` / `screen` fields,
and whichever ones it sets take precedence over the profile's. Anything
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

- **Headless scraping of public sites** — start with `native()`. Most
  sites don't actively probe Navigator prototypes; the cheaper profile
  is plenty.
- **Sites with active bot detection** (Cloudflare, PerimeterX,
  DataDome, Akamai) — use `spoofed()`. Pair with the `cloudflare`
  Cargo feature when you specifically need Turnstile bypass — see
  [Cloudflare](./cloudflare.md).
- **Sites with strict CSP** — `spoofed()` defaults to
  `bypass_csp = true`, which is normally what you want. If you're
  *testing* a real CSP, override it with `.bypass_csp(false)` (and
  expect the JS bootstrap to fail to install).
- **Sites that check functions for tampering** (their
  `Function.prototype.toString` output, or their shape) — `native()`
  rather than `spoofed()`: `native()` replaces no function, while
  `spoofed()` replaces `Function.prototype.toString` and dozens of
  `navigator`, WebGL, canvas and other functions. zendriver's bootstrap
  papers over the obvious differences, but a determined adversary will
  still find drift.
