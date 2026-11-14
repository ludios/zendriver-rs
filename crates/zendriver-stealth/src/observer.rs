//! StealthObserver: applies a [`StealthProfile`] to each new attached target.
//!
//! Installed in the [`zendriver_transport`] actor's observer chain. On
//! `Target.attachedToTarget`, the actor pauses the new target, walks every
//! observer serially, then releases the debugger via
//! `Runtime.runIfWaitingForDebugger`. The observer's job is to push every
//! UA/timezone/locale override and (for `Spoofed`) the CPU-count and
//! CPU-tier overrides and the bootstrap script _before_ the debugger releases, so the first
//! script the page runs sees the patched globals.

use serde_json::json;
use zendriver_transport::{CallError, ObserverError, PausedSession, TargetObserver};

use crate::patches::{bootstrap_script, bootstrap_script_native_webgl};
use crate::persona::GeoPos;
use crate::persona::specs::{ScreenSpec, UaMetadata};
use crate::{Fingerprint, Persona, ProfileKind, StealthError, StealthProfile};

/// The height that [`headless_screen_switches`] leaves for a taskbar when the
/// screen has no `avail_height`; the default that [`ScreenSpec`] documents.
const DEFAULT_TASKBAR_HEIGHT: u32 = 48;

/// The largest size, in physical pixels, that [`headless_screen_switches`]
/// takes: over 4 times the width of a 16K display. Chrome dumps core on a
/// size beyond `i32::MAX`.
const MAX_PHYSICAL: u64 = 1 << 16;

/// Whether zendriver measured headless Chrome's rounding at fractional
/// device pixel ratios (see [`chrome_axis`]) on the OS it runs on: on Linux
/// and Windows. macOS's headless screen is said to truncate a fractional
/// ratio, and wasn't measured.
const FRACTIONAL_RATIOS_MEASURED: bool = cfg!(any(target_os = "linux", target_os = "windows"));

/// Whether [`headless_screen_switches`] takes the device pixel ratio of
/// `quarters` quarters. Where [`FRACTIONAL_RATIOS_MEASURED`], it takes 1,
/// 1.25, 1.5, … 4, which cover Windows' scaling steps. Elsewhere it takes
/// only 1 and 2 (macOS's), whose `f32` reciprocals are exact, so that
/// Chrome's rounding can't matter.
fn ratio_taken(quarters: u64) -> bool {
    if FRACTIONAL_RATIOS_MEASURED {
        (4..=16).contains(&quarters)
    } else {
        quarters == 4 || quarters == 8
    }
}

/// The CSS pixels that headless Chrome reports for `physical` physical
/// pixels at the ratio `dpr`, for a window's size and for some of the
/// screen's sizes (see [`chrome_axis`]): it multiplies by the `f32`
/// reciprocal of the ratio and rounds up. The reciprocal can be inexact, so
/// the result can be 1 more than expected: at 1.75, 84 physical pixels come
/// to 48.000004 CSS pixels, reported as 49.
fn chrome_css(physical: u32, dpr: f32) -> u32 {
    (physical as f32 * (1.0 / dpr)).ceil() as u32
}

/// The error for a size of `css` CSS pixels (of the kind `what`) that
/// headless Chrome can't report at the ratio `dpr`.
fn unreportable(what: &str, css: u32, dpr: f32) -> StealthError {
    StealthError::InvalidOverride(format!(
        "headless Chrome can't report a {what} of {css} at a device_pixel_ratio of {dpr}"
    ))
}

/// The physical pixels that headless Chrome reports as `css` CSS pixels at
/// the ratio `dpr`, as [`chrome_css`] says: `exact` (`css` times the ratio,
/// a whole number), or one physical pixel less. `what` names the kind of
/// size, for the error.
fn chrome_physical(what: &str, css: u32, exact: u32, dpr: f32) -> Result<u32, StealthError> {
    [exact, exact.saturating_sub(1)]
        .into_iter()
        .find(|&physical| chrome_css(physical, dpr) == css)
        .ok_or_else(|| unreportable(what, css, dpr))
}

/// The physical size, and the physical inset of the work area, that
/// `--screen-info` takes along one axis (the width or the height) for
/// headless Chrome to report a screen `css` CSS pixels long, whose work
/// area (from its left or top edge) is `avail` CSS pixels long, at the
/// ratio `dpr`. `exact` and `exact_avail` are `css` and `avail` times the
/// ratio, which are whole numbers, with `avail` at most `css`.
///
/// Measured with Chrome 157: on Linux, Chrome divides the screen's size by
/// the ratio, which is exact, and scales the inset by the ratio's `f32`
/// reciprocal; on Windows, it scales the screen's size and the work area's
/// size by that reciprocal instead, and the inset is their difference. So
/// for each value that Chrome scales by the reciprocal (the inset on
/// Linux; the screen's size and the work area's size on Windows), this
/// passes the physical pixels that [`chrome_physical`] finds.
fn chrome_axis(
    css: u32,
    avail: u32,
    exact: u32,
    exact_avail: u32,
    dpr: f32,
) -> Result<[u32; 2], StealthError> {
    assert!(
        avail <= css && exact_avail <= exact,
        "work area {avail} ({exact_avail} physical) beyond the screen {css} ({exact} physical)"
    );
    if cfg!(target_os = "windows") {
        let size = chrome_physical("screen size", css, exact, dpr)?;
        let work = chrome_physical("work-area size", avail, exact_avail, dpr)?;
        // `work` is at most `size`: if `avail` is `css`, the two are the
        // same, and if `avail` is less, `exact_avail` is at most
        // `exact - 1`.
        assert!(work <= size, "work area {work} beyond the screen {size}");
        Ok([size, size - work])
    } else {
        let inset = chrome_physical("work-area inset", css - avail, exact - exact_avail, dpr)?;
        Ok([exact, inset])
    }
}

/// The `--window-size` (in CSS pixels) for which headless Chrome reports a
/// window size of `css` at the ratio `dpr`: `css` or 1 less. Chrome rounds
/// the size up to physical pixels, and back as [`chrome_css`] says.
fn chrome_window_size(css: u32, dpr: f32) -> Result<u32, StealthError> {
    [css, css.saturating_sub(1)]
        .into_iter()
        .find(|&size| chrome_css((size as f32 * dpr).ceil() as u32, dpr) == css)
        .ok_or_else(|| unreportable("window size", css, dpr))
}

/// The switches that give headless Chrome the screen `screen`, with each
/// window filling its work area: `--screen-info` for the screen, and
/// `--window-size` plus `--window-position=0,0` for the windows.
///
/// `--screen-info` takes the size and the work area in physical pixels:
/// the CSS pixels of `screen` times its device pixel ratio, or 1 less where
/// [`chrome_axis`] makes up for Chrome's rounding. The work area is
/// `avail_width` by `avail_height`, at the top left; an unset `avail_width`
/// keeps the full width, and an unset `avail_height` leaves
/// [`DEFAULT_TASKBAR_HEIGHT`]. `--window-size` takes CSS pixels.
///
/// Chrome scales some of the screen's sizes and a window's size by the
/// ratio's `f32` reciprocal, which can be inexact (see [`chrome_css`]), so
/// the switches give the sizes that come back as intended ([`chrome_axis`],
/// [`chrome_window_size`]). A test sweeps every ratio and size taken, so
/// those always exist.
///
/// The window isn't maximized (`--start-maximized`), because Chrome then
/// ignores the size of the first `Browser.setWindowBounds`, which only
/// restores the window from its maximized state.
///
/// # Errors
/// Returns [`StealthError::InvalidOverride`] for a screen whose ratio
/// [`ratio_taken`] doesn't take; whose work area is larger than the screen,
/// or empty; or one of whose sizes (width, height, and the work area's width
/// and height), times the ratio, isn't a whole number of physical pixels
/// from 1 to [`MAX_PHYSICAL`]. Chrome would report a different screen for
/// most of those, and dumps core on some.
fn headless_screen_switches(screen: &ScreenSpec) -> Result<Vec<String>, StealthError> {
    let ratio = screen.device_pixel_ratio;
    let quarters = ratio * 4.0;
    if !(quarters.is_finite() && quarters.fract() == 0.0 && ratio_taken(quarters as u64)) {
        let taken = if FRACTIONAL_RATIOS_MEASURED {
            "1, 1.25, 1.5, 1.75, 2, 2.25, 2.5, 2.75, 3, 3.25, 3.5, 3.75 or 4"
        } else {
            "1 or 2 (zendriver takes 1.25, 1.5, 1.75, 2.25, … 4 only on Linux and Windows)"
        };
        return Err(StealthError::InvalidOverride(format!(
            "the screen's device_pixel_ratio must be {taken}, not {ratio}"
        )));
    }
    let quarters = quarters as u64;
    let dpr = ratio as f32;
    if screen.avail_height.is_none() && screen.height <= DEFAULT_TASKBAR_HEIGHT {
        return Err(StealthError::InvalidOverride(format!(
            "the screen's height of {} leaves no work area above the default taskbar of \
             {DEFAULT_TASKBAR_HEIGHT}; set avail_height",
            screen.height
        )));
    }
    let avail_width = screen.avail_width.unwrap_or(screen.width);
    let avail_height = screen
        .avail_height
        .unwrap_or_else(|| screen.height - DEFAULT_TASKBAR_HEIGHT);
    if avail_width > screen.width || avail_height > screen.height {
        return Err(StealthError::InvalidOverride(format!(
            "the screen's work area {avail_width}x{avail_height} is larger than the screen {}x{}",
            screen.width, screen.height
        )));
    }
    // `css` CSS pixels in physical pixels: exactly `css` times the ratio,
    // which `chrome_axis` may make 1 less for `--screen-info`.
    let physical = |name: &str, css: u32| -> Result<u32, StealthError> {
        let quarter_pixels = u64::from(css) * quarters;
        if css == 0 || quarter_pixels % 4 != 0 || quarter_pixels / 4 > MAX_PHYSICAL {
            return Err(StealthError::InvalidOverride(format!(
                "the screen's {name} of {css}, times its device_pixel_ratio of {ratio}, must be \
                 a whole number of physical pixels from 1 to {MAX_PHYSICAL}"
            )));
        }
        Ok((quarter_pixels / 4) as u32)
    };
    let [width, right] = chrome_axis(
        screen.width,
        avail_width,
        physical("width", screen.width)?,
        physical("work-area width", avail_width)?,
        dpr,
    )?;
    let [height, bottom] = chrome_axis(
        screen.height,
        avail_height,
        physical("height", screen.height)?,
        physical("work-area height", avail_height)?,
        dpr,
    )?;
    let screen_info = format!(
        "--screen-info={{{width}x{height} devicePixelRatio={ratio} workAreaRight={right} \
         workAreaBottom={bottom}}}"
    );
    Ok(vec![
        screen_info,
        format!(
            "--window-size={},{}",
            chrome_window_size(avail_width, dpr)?,
            chrome_window_size(avail_height, dpr)?
        ),
        "--window-position=0,0".to_string(),
    ])
}

/// Observer that applies a [`StealthProfile`] + [`Fingerprint`] to every page
/// target, and to every out-of-process iframe target: such an iframe runs in
/// another renderer process than its page's, so nothing set on the page's
/// session reaches it. (An iframe in its page's process isn't a target; the
/// page's setup covers it.) Workers and worklets get no setup.
#[derive(Debug)]
pub struct StealthObserver {
    profile: StealthProfile,
    fingerprint: Fingerprint,
    /// Pre-rendered bootstrap source: the patches bundle for `Spoofed`, and
    /// empty for `Off` and `Native`, which never send
    /// `Page.addScriptToEvaluateOnNewDocument`.
    bootstrap: String,
    /// Mock geolocation coordinates from the resolved [`Persona`], sent via
    /// `Emulation.setGeolocationOverride`. Unlike
    /// `timezone`/`locale`/`languages`/`screen` (folded into `fingerprint` by
    /// [`Fingerprint::overlay_persona`]), geolocation has no `Fingerprint`
    /// counterpart, so it's captured here straight off the persona at
    /// construction time.
    geolocation: Option<GeoPos>,
    /// Custom UA-CH from the resolved [`Persona`]'s `ua.ua_metadata`. When
    /// set, [`UaMetadata::resolve`] fills any unset sub-field from
    /// `fingerprint.ua_metadata` and the result drives
    /// `Emulation.setUserAgentOverride.userAgentMetadata` in place of the
    /// fingerprint-derived value outright. Field-wise rather than
    /// whole-value, which is why it is not folded into `fingerprint`.
    ua_metadata: Option<UaMetadata>,
    /// The `navigator.cpuPerformance` tier that `Spoofed` reports, or `None`
    /// to leave Chrome's (see [`cpu_tier_override`]).
    cpu_tier: Option<&'static str>,
}

impl StealthObserver {
    /// Build a new observer. Bootstrap source is composed eagerly so the
    /// per-target hot path only pays a `clone`/borrow.
    ///
    /// The bootstrap is driven by a [`Persona`] (surface-spoofing config) plus
    /// the [`Fingerprint`] (coherent UA / Chrome identity). This constructor
    /// uses [`Persona::default`] — no surface overrides, identity-only patches
    /// keep their current behavior. The launch path (a later task) will thread
    /// a caller-supplied persona via [`StealthObserver::with_persona`].
    #[must_use]
    pub fn new(profile: StealthProfile, fingerprint: Fingerprint) -> Self {
        Self::with_persona(profile, fingerprint, Persona::default())
    }

    /// Build a new observer with an explicit [`Persona`] driving the surface
    /// patches. `fingerprint` still supplies the coherent UA / Chrome version.
    ///
    /// This is where the persona and the fingerprint are combined, so it is
    /// where they are merged: the persona's `timezone` / `locale` /
    /// `languages` / `screen` / `hardware_concurrency` / `device_memory_gb`
    /// (and, under `spoofed`, its `platform`) are folded into `fingerprint`
    /// up front (see [`Fingerprint::overlay_persona`]), and every consumer
    /// below — the
    /// `Emulation.set*Override` calls, the `Accept-Language` header, the
    /// bootstrap, and the launch switches of
    /// [`StealthObserver::headless_flags`] — then reads that one merged
    /// value. An explicitly-set persona field wins over
    /// the fingerprint's; `None` inherits the fingerprint's.
    #[must_use]
    pub fn with_persona(
        profile: StealthProfile,
        mut fingerprint: Fingerprint,
        persona: Persona,
    ) -> Self {
        fingerprint.overlay_persona(&persona);
        // Under `spoofed`, a persona's platform also picks the UA and the UA
        // client hints, which then agree with the GPU that the bootstrap
        // presents for it. `native` presents the real GPU, so another OS
        // couldn't agree with it; it keeps the fingerprint's platform. The
        // profile's UA pins still win, as they do over its own platform pin.
        if let Some(platform) = persona.platform
            && profile.kind() == ProfileKind::Spoofed
        {
            fingerprint.claim_platform(platform);
            profile.apply_ua_pins(&mut fingerprint);
        }

        let bootstrap = match profile.kind() {
            ProfileKind::Spoofed => {
                if profile.native_webgl_enabled() {
                    bootstrap_script_native_webgl(&persona, &fingerprint)
                } else {
                    bootstrap_script(&persona, &fingerprint)
                }
            }
            ProfileKind::Native | ProfileKind::Off => String::new(),
        };
        let geolocation = persona.geolocation;
        let ua_metadata = persona.ua.as_ref().and_then(|u| u.ua_metadata.clone());
        let claimed = match &ua_metadata {
            Some(custom) => custom.resolve(&fingerprint.ua_metadata),
            None => fingerprint.ua_metadata.clone(),
        };
        let apple_silicon = claimed.platform == "macOS" && claimed.architecture == "arm";
        let host_cores = u32::try_from(num_cpus::get()).unwrap_or(u32::MAX);
        let cpu_tier = cpu_tier_override(fingerprint.cpu_count, host_cores, apple_silicon);
        Self {
            profile,
            fingerprint,
            bootstrap,
            geolocation,
            ua_metadata,
            cpu_tier,
        }
    }

    /// The screen this observer presents: the merged fingerprint's (the
    /// persona's when it set one, else the [`StealthProfile::screen`] pin),
    /// or else 1920x1080 at a device pixel ratio of 1.
    fn screen(&self) -> ScreenSpec {
        self.fingerprint
            .screen
            .unwrap_or_else(|| ScreenSpec::new(1920, 1080, 1.0))
    }

    /// The switches to launch a headless Chrome with, for this observer's
    /// profile and screen. Empty if the profile is `Off`.
    ///
    /// For `Native` and `Spoofed`, they are `--screen-info`, which gives
    /// headless Chrome
    /// the screen that the persona or the [`StealthProfile::screen`] pin
    /// configures (else 1920x1080, with a 48-pixel taskbar), and
    /// `--window-size` plus `--window-position=0,0`, which make each window
    /// fill that screen's work area. Chrome then reports a screen, window and
    /// viewport that agree, with no `Emulation.setDeviceMetricsOverride` and
    /// no JS patch, to the page in front of each window. (On Linux, Chrome
    /// gives a background tab's page its viewport as its window.) A headed
    /// Chrome needs none of these switches, since it reports the real
    /// display.
    ///
    /// # Errors
    /// Returns [`StealthError::InvalidOverride`] for a screen whose
    /// `device_pixel_ratio` isn't one that zendriver takes (1, 1.25, 1.5,
    /// … 4 on Linux and Windows; 1 or 2 elsewhere); whose work area is
    /// empty or larger than the screen; or one of whose sizes (`width`,
    /// `height`, and the work area's width and height), times
    /// `device_pixel_ratio`, isn't a whole number of physical pixels from 1
    /// to 65536. Headless Chrome would report a different screen for most of
    /// those, and dumps core on some.
    pub fn headless_flags(&self) -> Result<Vec<String>, StealthError> {
        if self.profile.kind() == ProfileKind::Off {
            return Ok(Vec::new());
        }
        headless_screen_switches(&self.screen())
    }
}

/// The `navigator.cpuPerformance` tier that Chrome would most likely report
/// on a machine with `cores` logical processors, as
/// `Emulation.setCPUPerformanceOverride` names it. `apple_silicon`: whether
/// the CPU is one of Apple's M series.
///
/// Chrome derives the tier from the processor count and the CPU's model
/// (`GetTierFromCpuInfo` in Chromium's
/// `content/browser/cpu_performance/cpu_performance.cc`): 1 or 2 is low, 3
/// or 4 mid, 5 to 10 high, and 11 or more ultra, as is an M-series (or
/// Intel Core Ultra) CPU with 8 to 10. A fingerprint names no CPU model, so
/// this leaves out the rest of Chrome's exceptions for particular models:
/// low for some old CPUs with up to 4, mid for an Intel or AMD CPU with 2
/// that isn't one of those, and high for a Ryzen or Intel N-series with 4.
fn cpu_performance_tier(cores: u32, apple_silicon: bool) -> &'static str {
    match cores {
        0 => "unknown",
        1 | 2 => "low",
        3 | 4 => "mid",
        8..=10 if apple_silicon => "ultra",
        5..=10 => "high",
        _ => "ultra",
    }
}

/// The `navigator.cpuPerformance` tier to report for a page told that it
/// has `cores` logical processors, on a host with `host_cores`; or `None`
/// to leave Chrome's own tier. `apple_silicon`: whether the identity claims
/// an Apple M-series CPU.
///
/// Chrome's own tier is the right one when `cores` is the host's count,
/// since Chrome also weighs the host's CPU model, which a fingerprint
/// doesn't name. Otherwise the page would pair the persona's count with the
/// host's tier, which Chrome would derive from neither.
fn cpu_tier_override(cores: u32, host_cores: u32, apple_silicon: bool) -> Option<&'static str> {
    if cores == host_cores {
        return None;
    }
    Some(cpu_performance_tier(cores, apple_silicon))
}

#[async_trait::async_trait]
impl TargetObserver for StealthObserver {
    fn name(&self) -> &'static str {
        "stealth"
    }

    async fn on_target_attached(&self, session: PausedSession<'_>) -> Result<(), ObserverError> {
        // An out-of-process iframe is a target of its own, in another
        // renderer process than its page's, so no override or script of the
        // page's session reaches it: it gets the page's whole setup. Workers
        // and worklets get none.
        if !matches!(session.target_info.kind.as_str(), "page" | "iframe") {
            return Ok(());
        }
        if self.profile.kind() == ProfileKind::Off {
            return Ok(());
        }

        session.call("Page.enable", json!({})).await?;

        // UA override — Emulation.setUserAgentOverride carries the Client-Hints
        // metadata too, so we don't have to send Network.setUserAgentOverride
        // separately.
        let accept_language = {
            // The fingerprint is already merged with the persona (see
            // `with_persona`), so its language list is the effective one — a
            // caller's `Persona::languages` / `Persona::locale` reaches the
            // header from here.
            let langs = crate::lang::fingerprint_languages(&self.fingerprint);
            // `Emulation.setUserAgentOverride.acceptLanguage` wants a PLAIN
            // comma-separated locale list (e.g. `en-US,en`) — Chrome appends the
            // `;q=` weights itself. Passing an already-weighted string (the
            // `accept_language()` header form) makes Chrome double them, yielding
            // a malformed `Accept-Language: en-US,en;q=0.9;q=0.9`. Send the bare
            // list so the emitted header is a clean `en-US,en;q=0.9`.
            langs.join(",")
        };
        // Persona UA-CH wins when supplied — field-wise, falling back to the
        // fingerprint-derived value for any sub-field the persona left
        // unset. Absent persona UA-CH → today's behavior (fingerprint's
        // UAM verbatim).
        let user_agent_metadata = match &self.ua_metadata {
            Some(custom) => custom.resolve(&self.fingerprint.ua_metadata),
            None => self.fingerprint.ua_metadata.clone(),
        };
        session
            .call(
                "Emulation.setUserAgentOverride",
                json!({
                    "userAgent": &self.fingerprint.ua_string,
                    "acceptLanguage": accept_language,
                    // `js_string()`, NOT `ch_platform()`. This CDP parameter is defined as "the
                    // platform navigator.platform should return", so it takes the legacy JS value
                    // (`Win32` / `MacIntel` / `Linux x86_64`). The Client-Hints spelling
                    // (`Windows` / `macOS` / `Linux`) belongs to `userAgentMetadata.platform`,
                    // which is sent separately just below and derives it in `Fingerprint::new`.
                    //
                    // Sending the CH spelling here made pages report a `navigator.platform`
                    // no real browser emits, e.g. `"macOS"` where Chrome reports `"MacIntel"`.
                    // Measured against a real Chrome build, not inferred.
                    //
                    // Unlike the other overrides, Chrome keeps this one in the page's
                    // settings, which every DevTools session on the page rewrites with its
                    // own platform (empty if it set none) when it is restored after a
                    // cross-process navigation, and when it detaches.
                    "platform": self.fingerprint.platform.js_string(),
                    "userAgentMetadata": &user_agent_metadata,
                }),
            )
            .await?;

        // `navigator.hardwareConcurrency`, in the page and its same-process
        // iframes; a worker still reports the host's.
        //
        // `navigator.cpuPerformance` (Chrome 152 on; only on a window's
        // `navigator`, not a worker's, and only in a secure context) follows
        // the same count when it isn't the host's (see `cpu_tier_override`).
        // Chrome has the override only from 153 on; on an older Chrome the
        // page reports the host's tier.
        //
        // Chrome rejects a count outside 1 to `i32::MAX`, which would fail the
        // page's setup; the page then reports the host's count and tier.
        let cores = self.fingerprint.cpu_count;
        let reportable = i32::try_from(cores).is_ok_and(|n| n > 0);
        if self.profile.kind() == ProfileKind::Spoofed && !reportable {
            tracing::warn!(
                cpu_count = cores,
                "stealth: Chrome can't report this CPU count; the page reports the host's"
            );
        }
        if self.profile.kind() == ProfileKind::Spoofed && reportable {
            session
                .call(
                    "Emulation.setHardwareConcurrencyOverride",
                    json!({ "hardwareConcurrency": cores }),
                )
                .await?;
            if let Some(tier) = self.cpu_tier {
                let reply = session
                    .call(
                        "Emulation.setCPUPerformanceOverride",
                        json!({ "performanceTier": tier }),
                    )
                    .await;
                if matches!(reply, Err(CallError::Rpc(-32601, ..))) {
                    tracing::debug!(
                        tier,
                        "Chrome lacks Emulation.setCPUPerformanceOverride (added in 153); \
                         the page reports the host's CPU tier"
                    );
                } else {
                    reply?;
                }
            }
        }

        // Focus emulation keeps the (always-backgrounded) headless tab from
        // reporting `document.hasFocus()` false. An out-of-process iframe
        // needs its own: without it, the iframe's document stays unfocused
        // when its page focuses the iframe.
        session
            .call(
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
            )
            .await?;

        if let Some(ref tz) = self.fingerprint.timezone {
            session
                .call("Emulation.setTimezoneOverride", json!({ "timezoneId": tz }))
                .await?;
        }
        // Keep the JS-visible locale (navigator.language, Intl) coherent with
        // the Accept-Language sent above by resolving both from the same
        // helper: this is the head of that same list, so the two surfaces
        // cannot disagree. Resolving them separately is what let them —
        // `Intl` reported a locale the header never advertised. A fingerprint
        // configuring neither locale nor languages yields `None` and keeps
        // Chrome's native locale; we don't force one.
        let effective_locale = crate::lang::effective_locale(&self.fingerprint);
        if let Some(ref locale) = effective_locale {
            // A renderer process holds one `Emulation.setLocaleOverride` for
            // all of its targets. So a target that isn't the first in its
            // process (the second iframe of a site in a page) finds the
            // override already set by the first's session, and Chrome
            // replies `[-32000] Another locale override is already in
            // effect`. That is benign — the same locale is already applied —
            // and must NOT propagate: propagating detaches the target without
            // the rest of its setup (CSP bypass, the bootstrap script).
            // Tolerate that one reply; surface every other error. (Chrome
            // accepts a timezone override that repeats the one its process
            // holds.)
            match session
                .call("Emulation.setLocaleOverride", json!({ "locale": locale }))
                .await
            {
                Ok(_) => {}
                Err(CallError::Rpc(_, ref message, _))
                    if message.contains("locale override is already in effect") =>
                {
                    tracing::debug!(
                        %locale,
                        "locale override already in effect (set by an earlier session); tolerating"
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(ref geo) = self.geolocation {
            // Sets the value the Geolocation API *would* return — it does
            // NOT grant the `geolocation` permission. Chrome still gates the
            // API behind a permission prompt/grant; auto-granting it here
            // would be a separate (and itself suspicious) signal, so we
            // deliberately leave permissioning to the caller.
            let mut params = json!({
                "latitude": geo.latitude,
                "longitude": geo.longitude,
            });
            if let Some(accuracy) = geo.accuracy {
                params["accuracy"] = json!(accuracy);
            }
            session
                .call("Emulation.setGeolocationOverride", params)
                .await?;
        }

        // An opt-in: it weakens the page's own security policy, and pages can detect it. The
        // bootstrap doesn't need it (see `StealthProfile::bypass_csp`).
        if self.profile.bypass_csp_enabled() {
            session
                .call("Page.setBypassCSP", json!({ "enabled": true }))
                .await?;
        }

        // Injection is driven by whether there IS a bootstrap (only `Spoofed` has one), not by
        // a second, independent check of the profile kind.
        if !self.bootstrap.is_empty() {
            // Inject into the MAIN world (no `worldName`). The bootstrap's
            // patches mutate globals such as `Function.prototype`, `Navigator.prototype`
            // and `HTMLCanvasElement.prototype` — every isolated
            // world gets its own copy of these, so a patch applied in a
            // named/isolated world is invisible to the page's own scripts
            // (and to `evaluate_main`, the surface detection sites probe).
            // Running the bootstrap in the main world is the only way these
            // mutations actually affect the document under test.
            session
                .call(
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({
                        "source": &self.bootstrap,
                        "includeCommandLineAPI": false,
                        "runImmediately": true,
                    }),
                )
                .await?;
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Platform;
    use serde_json::json;
    use zendriver_transport::testing::MockConnection;

    /// This host's logical processor count. A fingerprint with this count
    /// gets no `Emulation.setCPUPerformanceOverride` (see
    /// [`cpu_tier_override`]), so the command sequences below don't depend
    /// on the host.
    fn host_cores() -> u32 {
        u32::try_from(num_cpus::get()).unwrap()
    }

    /// `Emulation.setUserAgentOverride` carries the platform on TWO distinct axes, and they
    /// must not be swapped:
    ///
    /// * top-level `platform` — CDP defines this as "the platform navigator.platform should
    ///   return", so it takes the legacy JS spelling `Win32` / `MacIntel` / `Linux x86_64`
    ///   ([`Platform::js_string`]).
    /// * `userAgentMetadata.platform` — the Client-Hints spelling `Windows` / `macOS` /
    ///   `Linux` ([`Platform::ch_platform`]).
    ///
    /// The two sets are disjoint, so this asserts membership rather than a fixture literal and
    /// therefore catches the swap on whatever platform the test host happens to be.
    ///
    /// Regression guarded: the top-level param was sent as `ch_platform()`, so pages reported
    /// a `navigator.platform` no real browser emits.
    fn assert_platform_axes_not_swapped(sent: &serde_json::Value) {
        const JS_SPELLINGS: [&str; 3] = ["Win32", "MacIntel", "Linux x86_64"];
        const CH_SPELLINGS: [&str; 3] = ["Windows", "macOS", "Linux"];

        let top = sent["params"]["platform"]
            .as_str()
            .expect("setUserAgentOverride.platform must be sent");
        assert!(
            JS_SPELLINGS.contains(&top),
            "setUserAgentOverride.platform sets navigator.platform, so it must be one of \
             {JS_SPELLINGS:?} (Platform::js_string). Got {top:?} — if that is one of \
             {CH_SPELLINGS:?} then the two platform axes are swapped, and no real browser \
             reports those for navigator.platform."
        );

        // The CH axis must still carry the CH spelling: a fix that swapped both would just
        // trade one impossible value for another.
        let ch = sent["params"]["userAgentMetadata"]["platform"]
            .as_str()
            .expect("userAgentMetadata.platform must be sent");
        assert!(
            CH_SPELLINGS.contains(&ch),
            "userAgentMetadata.platform is the Client-Hints axis and must be one of \
             {CH_SPELLINGS:?} (Platform::ch_platform). Got {ch:?}."
        );
    }

    #[tokio::test]
    async fn spoofed_observer_sends_expected_sequence_for_page_target() {
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let profile = StealthProfile::spoofed();
        let observer = std::sync::Arc::new(StealthObserver::new(profile, fp));

        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        // Emit a Target.attachedToTarget event.
        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        // Expected sequence (each followed by a reply so the observer
        // continues). The closing Runtime.runIfWaitingForDebugger is the
        // actor's debugger-release after every observer succeeds.
        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            // `acceptLanguage` must be a PLAIN locale list — Chrome adds the
            // `;q=` weights. A weighted value here makes Chrome double them into
            // a malformed `en-US,en;q=0.9;q=0.9` header.
            if expected == "Emulation.setUserAgentOverride" {
                let al = mock.last_sent()["params"]["acceptLanguage"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                assert!(!al.is_empty(), "acceptLanguage must be set");
                assert!(
                    !al.contains(";q="),
                    "acceptLanguage must be a bare locale list (no q-weights); got: {al}"
                );
                assert_platform_axes_not_swapped(mock.last_sent());
            }
            mock.reply(id, json!({})).await;
        }

        conn.shutdown();
    }

    #[tokio::test]
    async fn languages_without_locale_still_sets_coherent_locale_override() {
        // A fingerprint that pins `languages` but no `locale` previously sent a
        // spoofed Accept-Language while leaving the JS-visible locale
        // (navigator.language / Intl) at Chrome's default — an incoherent
        // cross-surface tell. The locale override must be derived from the
        // pinned languages so both agree.
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: Some(vec!["de-DE".into(), "de".into()]),
            screen: None,
        };
        let profile = StealthProfile::spoofed();
        let observer = std::sync::Arc::new(StealthObserver::new(profile, fp));
        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        let mut locale_override: Option<String> = None;
        let mut accept_language: Option<String> = None;
        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Emulation.setLocaleOverride",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            if expected == "Emulation.setUserAgentOverride" {
                accept_language = mock.last_sent()["params"]["acceptLanguage"]
                    .as_str()
                    .map(String::from);
                assert_platform_axes_not_swapped(mock.last_sent());
            }
            if expected == "Emulation.setLocaleOverride" {
                locale_override = mock.last_sent()["params"]["locale"]
                    .as_str()
                    .map(String::from);
            }
            mock.reply(id, json!({})).await;
        }
        conn.shutdown();

        assert_eq!(
            accept_language.as_deref(),
            Some("de-DE,de"),
            "Accept-Language must reflect the pinned languages"
        );
        assert_eq!(
            locale_override.as_deref(),
            Some("de-DE"),
            "locale override must be derived from pinned languages so JS locale stays coherent with Accept-Language"
        );
    }

    #[tokio::test]
    async fn locale_override_already_in_effect_does_not_detach_the_observer() {
        // A renderer process holds one `Emulation.setLocaleOverride` for all
        // of its targets. A second target in a process (a second iframe of a
        // site in a page) attaches this observer to its own session, where
        // the override is already in effect from the first, so Chrome
        // replies -32000.
        // The observer must tolerate that reply and keep applying the frame's
        // remaining patches (CSP bypass, the bootstrap script) instead of
        // detaching — otherwise the OOPIF loses its stealth surface.
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: Some(vec!["de-DE".into(), "de".into()]),
            screen: None,
        };
        let observer = std::sync::Arc::new(StealthObserver::new(StealthProfile::spoofed(), fp));
        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        // Reply to setLocaleOverride with the "already in effect"
        // error; OK to everything else. Seeing the commands AFTER setLocaleOverride
        // proves the observer tolerated the error rather than detaching.
        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Emulation.setLocaleOverride",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "observer stopped early — never saw {expected} within 2s \
                             (locale 'already in effect' error was not tolerated?)"
                        )
                    });
            if expected == "Emulation.setLocaleOverride" {
                mock.reply_err(id, -32000, "Another locale override is already in effect")
                    .await;
            } else {
                mock.reply(id, json!({})).await;
            }
        }
        conn.shutdown();
    }

    #[tokio::test]
    async fn native_webgl_spoofed_observer_omits_webgl_patch_in_bootstrap() {
        // End-to-end wiring check: a spoofed profile with the native_webgl
        // opt-in must send a bootstrap script that omits the WebGL
        // vendor/renderer patch, over the actual CDP payload.
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let profile = StealthProfile::spoofed().native_webgl(true);
        let observer = std::sync::Arc::new(StealthObserver::new(profile, fp));

        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            if expected == "Page.addScriptToEvaluateOnNewDocument" {
                let source = mock.last_sent()["params"]["source"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                assert!(
                    !source.contains("UNMASKED_VENDOR_WEBGL") && !source.contains("37445"),
                    "native_webgl bootstrap must omit the webgl patch block"
                );
            }
            mock.reply(id, json!({})).await;
        }

        conn.shutdown();
    }

    #[tokio::test]
    async fn native_isolation_alone_does_not_omit_webgl_patch_in_bootstrap() {
        // The split's other half: `native_isolation(true)` on its own (axis 1,
        // launch flags) must NOT silently drop the WebGL identity patch — the
        // emitted bootstrap must still carry it, since `native_webgl` is unset.
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let profile = StealthProfile::spoofed().native_isolation(true);
        let observer = std::sync::Arc::new(StealthObserver::new(profile, fp));

        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            if expected == "Page.addScriptToEvaluateOnNewDocument" {
                let source = mock.last_sent()["params"]["source"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                assert!(
                    source.contains("UNMASKED_VENDOR_WEBGL") || source.contains("37445"),
                    "native_isolation alone must keep the webgl patch block"
                );
            }
            mock.reply(id, json!({})).await;
        }

        conn.shutdown();
    }

    #[tokio::test]
    async fn spoofed_observer_emits_geolocation_override_when_persona_has_geo() {
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let persona = crate::Persona {
            geolocation: Some(crate::persona::GeoPos {
                latitude: 21.0285,
                longitude: 105.8542,
                accuracy: Some(50.0),
            }),
            ..crate::Persona::default()
        };
        let profile = StealthProfile::spoofed();
        let observer = std::sync::Arc::new(StealthObserver::with_persona(profile, fp, persona));

        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Emulation.setGeolocationOverride",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            if expected == "Emulation.setGeolocationOverride" {
                let params = mock.last_sent()["params"].clone();
                assert_eq!(params["latitude"].as_f64(), Some(21.0285));
                assert_eq!(params["longitude"].as_f64(), Some(105.8542));
                assert_eq!(params["accuracy"].as_f64(), Some(50.0));
            }
            mock.reply(id, json!({})).await;
        }

        conn.shutdown();
    }

    #[tokio::test]
    async fn spoofed_observer_emits_persona_ua_metadata_when_present() {
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let persona = crate::Persona {
            ua: Some(crate::UaSpec {
                ua_metadata: Some(crate::persona::specs::UaMetadata {
                    platform_version: Some("15.0.0".into()),
                    architecture: Some("arm".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..crate::Persona::default()
        };
        let profile = StealthProfile::spoofed();
        let observer = std::sync::Arc::new(StealthObserver::with_persona(profile, fp, persona));

        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            if expected == "Emulation.setUserAgentOverride" {
                let uam = mock.last_sent()["params"]["userAgentMetadata"].clone();
                // Persona-set sub-fields win.
                assert_eq!(uam["platformVersion"].as_str(), Some("15.0.0"));
                assert_eq!(uam["architecture"].as_str(), Some("arm"));
                // Unset sub-fields fall back to the fingerprint-derived UAM.
                assert_eq!(uam["platform"].as_str(), Some("macOS"));
                assert!(uam["brands"].is_array());
            }
            mock.reply(id, json!({})).await;
        }

        conn.shutdown();
    }

    #[tokio::test]
    async fn spoofed_observer_uses_fingerprint_uam_when_persona_absent() {
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let expected_uam = fp.ua_metadata.clone();
        let profile = StealthProfile::spoofed();
        // `Persona::default()` — no ua_metadata, no screen.
        let observer = std::sync::Arc::new(StealthObserver::new(profile, fp));

        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setFocusEmulationEnabled",
            "Page.addScriptToEvaluateOnNewDocument",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            if expected == "Emulation.setUserAgentOverride" {
                let uam = mock.last_sent()["params"]["userAgentMetadata"].clone();
                let expected_json = serde_json::to_value(&expected_uam).unwrap();
                assert_eq!(
                    uam, expected_json,
                    "absent persona UAM → fingerprint's verbatim"
                );
            }
            mock.reply(id, json!({})).await;
        }

        conn.shutdown();
    }

    #[tokio::test]
    async fn geolocation_override_omits_accuracy_when_unset() {
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let persona = crate::Persona {
            geolocation: Some(crate::persona::GeoPos {
                latitude: 1.0,
                longitude: 2.0,
                accuracy: None,
            }),
            ..crate::Persona::default()
        };
        let profile = StealthProfile::native();
        let observer = std::sync::Arc::new(StealthObserver::with_persona(profile, fp, persona));

        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer.clone()]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        for expected in [
            "Page.enable",
            "Emulation.setUserAgentOverride",
            // `Native` sends no bootstrap and no CPU-count override.
            "Emulation.setFocusEmulationEnabled",
            "Emulation.setGeolocationOverride",
            "Runtime.runIfWaitingForDebugger",
        ] {
            let id =
                tokio::time::timeout(std::time::Duration::from_secs(2), mock.expect_cmd(expected))
                    .await
                    .unwrap_or_else(|_| panic!("did not see {expected} within 2s"));
            if expected == "Emulation.setGeolocationOverride" {
                let params = mock.last_sent()["params"].clone();
                assert_eq!(params["latitude"].as_f64(), Some(1.0));
                assert_eq!(params["longitude"].as_f64(), Some(2.0));
                assert!(
                    params.get("accuracy").is_none(),
                    "accuracy must be omitted when unset, got: {params}"
                );
            }
            mock.reply(id, json!({})).await;
        }

        conn.shutdown();
    }

    #[tokio::test]
    async fn off_observer_skips_all_commands_just_releases_debugger() {
        let fp = Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: host_cores(),
            memory_gb: 8,
            ua_string: String::new(),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        };
        let observer = std::sync::Arc::new(StealthObserver::new(StealthProfile::off(), fp));
        let (mut mock, conn) = MockConnection::pair_with_observers(vec![observer]);

        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": "page",
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        // Off profile: only the actor's release-debugger call.
        let id = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            mock.expect_cmd("Runtime.runIfWaitingForDebugger"),
        )
        .await
        .unwrap();
        mock.reply(id, json!({})).await;
        conn.shutdown();
    }
}

/// The persona has to reach the wire, not just the bootstrap script.
///
/// Every assertion here is over the CDP frames the observer actually sent, via
/// [`drive_attach`] rather than `expect_cmd`: `expect_cmd` silently discards
/// non-matching frames, so it can only prove "X arrived eventually", and it has
/// no timeout, so a command that stops being sent hangs the test instead of
/// failing it. The bugs in this area are precisely "the override was never
/// sent" and "the override carried the wrong value", so the test has to see the
/// whole ordered frame list.
#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod persona_reaches_cdp_tests {
    use super::*;
    use crate::Platform;
    use crate::persona::specs::ScreenSpec;
    use serde_json::{Value, json};
    use zendriver_transport::testing::MockConnection;

    /// A fingerprint with every persona-overlappable field left unset, so each
    /// test pins only the axis it is about.
    fn bare_fingerprint() -> Fingerprint {
        Fingerprint {
            platform: Platform::MacIntel,
            chrome_major: 120,
            chrome_full: "120.0.6099.234".into(),
            cpu_count: 10,
            memory_gb: 8,
            ua_string: crate::ua::compose_ua_string(Platform::MacIntel, "120.0.6099.234"),
            ua_metadata: crate::UserAgentMetadata::realistic(
                Platform::MacIntel,
                120,
                "120.0.6099.234",
            ),
            timezone: None,
            locale: None,
            languages: None,
            screen: None,
        }
    }

    /// Drive one page-target attach through `observer` and return every CDP
    /// frame it sent, in order, replying `{}` to each.
    ///
    /// Stops at the actor's terminal call — `Runtime.runIfWaitingForDebugger`
    /// on success, `Target.detachFromTarget` when an observer errored — so a
    /// missing override shows up as an absent entry in the returned list, and
    /// a wedged observer fails on the per-frame budget instead of hanging.
    async fn drive_attach(observer: StealthObserver) -> Vec<Value> {
        drive_attach_of("page", observer, &[]).await
    }

    /// [`drive_attach`], against a Chrome that lacks each command in
    /// `missing`: it answers them `-32601`, as Chrome answers a command it
    /// doesn't have.
    async fn drive_attach_without(observer: StealthObserver, missing: &[&str]) -> Vec<Value> {
        let refused: Vec<_> = missing
            .iter()
            .map(|method| (*method, -32601, "wasn't found"))
            .collect();
        drive_attach_of("page", observer, &refused).await
    }

    /// [`drive_attach`] for a target of type `kind`, against a Chrome that
    /// answers each command in `refused` (`(method, code, message)`) with
    /// that error.
    async fn drive_attach_of(
        kind: &str,
        observer: StealthObserver,
        refused: &[(&str, i32, &str)],
    ) -> Vec<Value> {
        const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

        let (mut mock, conn) =
            MockConnection::pair_with_observers(vec![std::sync::Arc::new(observer)
                as std::sync::Arc<dyn zendriver_transport::TargetObserver>]);
        mock.emit_event(
            "Target.attachedToTarget",
            json!({
                "sessionId": "S1",
                "targetInfo": {
                    "targetId": "T1",
                    "type": kind,
                    "url": "about:blank",
                    "attached": true,
                },
                "waitingForDebugger": true,
            }),
        )
        .await;

        let mut frames = Vec::new();
        while let Some((method, id)) = mock.recv_cmd_timeout(FRAME_BUDGET).await {
            frames.push(mock.last_sent().clone());
            match refused.iter().find(|(refused, ..)| *refused == method) {
                Some((_, code, message)) => mock.reply_err(id, *code, message).await,
                None => mock.reply(id, json!({})).await,
            }
            if method == "Runtime.runIfWaitingForDebugger" || method == "Target.detachFromTarget" {
                break;
            }
        }
        conn.shutdown();

        let methods = method_names(&frames);
        assert!(
            methods
                .iter()
                .any(|m| m == "Runtime.runIfWaitingForDebugger"),
            "the observer never completed — no debugger release in {methods:?}"
        );
        frames
    }

    fn method_names(frames: &[Value]) -> Vec<String> {
        frames
            .iter()
            .map(|f| f["method"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// The `params` of the single frame for `method`, panicking with the whole
    /// observed sequence when it was never sent.
    fn params<'a>(frames: &'a [Value], method: &str) -> &'a Value {
        frames
            .iter()
            .find(|f| f["method"] == method)
            .map(|f| &f["params"])
            .unwrap_or_else(|| {
                panic!(
                    "{method} was never sent; the observer sent {:?}",
                    method_names(frames)
                )
            })
    }

    /// A spoofed observer whose fingerprint pins a timezone and a locale, and
    /// whose persona pins a geolocation.
    fn observer_with_every_override() -> StealthObserver {
        let fingerprint = Fingerprint {
            timezone: Some("Europe/Berlin".into()),
            locale: Some("de-DE".into()),
            ..bare_fingerprint()
        };
        let persona = crate::Persona {
            geolocation: Some(crate::persona::GeoPos {
                latitude: 52.52,
                longitude: 13.405,
                accuracy: None,
            }),
            ..crate::Persona::default()
        };
        StealthObserver::with_persona(StealthProfile::spoofed(), fingerprint, persona)
    }

    /// An out-of-process iframe gets its page's whole setup.
    #[tokio::test]
    async fn an_out_of_process_iframe_gets_its_pages_setup() {
        let page = method_names(&drive_attach(observer_with_every_override()).await);
        let iframe =
            method_names(&drive_attach_of("iframe", observer_with_every_override(), &[]).await);
        assert_eq!(
            page,
            [
                "Page.enable",
                "Emulation.setUserAgentOverride",
                "Emulation.setHardwareConcurrencyOverride",
                "Emulation.setCPUPerformanceOverride",
                "Emulation.setFocusEmulationEnabled",
                "Emulation.setTimezoneOverride",
                "Emulation.setLocaleOverride",
                "Emulation.setGeolocationOverride",
                "Page.addScriptToEvaluateOnNewDocument",
                "Runtime.runIfWaitingForDebugger",
            ]
        );
        assert_eq!(iframe, page);
    }

    /// A worker is released without any setup.
    #[tokio::test]
    async fn a_worker_gets_no_setup() {
        let frames = drive_attach_of("worker", observer_with_every_override(), &[]).await;
        assert_eq!(method_names(&frames), ["Runtime.runIfWaitingForDebugger"]);
    }

    /// A persona's platform reaches the UA, the UA client hints and
    /// `navigator.platform` together, not only the GPU that the bootstrap
    /// presents.
    #[tokio::test]
    async fn a_persona_platform_recomposes_the_user_agent_and_its_client_hints() {
        let persona = Persona {
            platform: Some(Platform::Win32),
            ..Persona::default()
        };
        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            bare_fingerprint(),
            persona,
        ))
        .await;
        let sent = params(&frames, "Emulation.setUserAgentOverride");
        let user_agent = sent["userAgent"].as_str().unwrap_or_default();
        assert!(user_agent.contains("Windows NT"), "userAgent {user_agent}");
        assert_eq!(sent["platform"], "Win32");
        assert_eq!(sent["userAgentMetadata"]["platform"], "Windows");
    }

    /// `native` presents the real GPU, so it keeps the fingerprint's
    /// platform rather than claim another OS that the GPU contradicts.
    #[tokio::test]
    async fn native_ignores_a_persona_platform() {
        let persona = Persona {
            platform: Some(Platform::Win32),
            ..Persona::default()
        };
        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::native(),
            bare_fingerprint(),
            persona,
        ))
        .await;
        let sent = params(&frames, "Emulation.setUserAgentOverride");
        assert_eq!(sent["platform"], "MacIntel");
        assert_eq!(sent["userAgentMetadata"]["platform"], "macOS");
    }

    /// The profile's verbatim UA pin wins over the UA composed for a
    /// persona's platform, as it does over the one composed for its own.
    #[tokio::test]
    async fn a_profile_user_agent_pin_survives_a_persona_platform() {
        let persona = Persona {
            platform: Some(Platform::Win32),
            ..Persona::default()
        };
        let profile = StealthProfile::spoofed().user_agent("pinned UA");
        let frames = drive_attach(StealthObserver::with_persona(
            profile,
            bare_fingerprint(),
            persona,
        ))
        .await;
        let sent = params(&frames, "Emulation.setUserAgentOverride");
        assert_eq!(sent["userAgent"], "pinned UA");
        assert_eq!(sent["platform"], "Win32");
        assert_eq!(sent["userAgentMetadata"]["platform"], "Windows");
    }

    /// The CPU-count override takes the persona's count, else the
    /// fingerprint's, and is left out for a count that Chrome rejects.
    #[tokio::test]
    async fn spoofed_overrides_the_cpu_count_with_the_personas_else_the_fingerprints() {
        let fp = Fingerprint {
            cpu_count: 6,
            ..bare_fingerprint()
        };
        for (pinned, expected) in [
            (Some(12), Some(12)),
            (None, Some(6)),
            (Some(0), None),
            (Some(u32::MAX), None),
        ] {
            let persona = Persona {
                hardware_concurrency: pinned,
                ..Persona::default()
            };
            let frames = drive_attach(StealthObserver::with_persona(
                StealthProfile::spoofed(),
                fp.clone(),
                persona,
            ))
            .await;
            let Some(expected) = expected else {
                let methods = method_names(&frames);
                for absent in [
                    "Emulation.setHardwareConcurrencyOverride",
                    "Emulation.setCPUPerformanceOverride",
                ] {
                    assert!(
                        !methods.iter().any(|m| m == absent),
                        "persona hardware_concurrency {pinned:?}: sent {absent}"
                    );
                }
                continue;
            };
            assert_eq!(
                params(&frames, "Emulation.setHardwareConcurrencyOverride")["hardwareConcurrency"]
                    .as_u64(),
                Some(expected),
                "persona hardware_concurrency {pinned:?}"
            );
        }
    }

    /// This host's logical processor count.
    fn host_cores() -> u32 {
        u32::try_from(num_cpus::get()).unwrap()
    }

    #[test]
    fn cpu_tier_override_leaves_the_hosts_own_count_to_chrome() {
        assert_eq!(cpu_tier_override(8, 8, true), None);
        assert_eq!(cpu_tier_override(6, 32, false), Some("high"));
        assert_eq!(cpu_tier_override(8, 16, true), Some("ultra"));
    }

    /// The observer reports a tier for a CPU count other than the host's,
    /// and leaves Chrome's own tier for the host's count.
    #[tokio::test]
    async fn spoofed_reports_a_cpu_tier_only_for_a_count_other_than_the_hosts() {
        let host = host_cores();
        for (pinned, tier) in [
            (host, None),
            (host + 1, Some(cpu_performance_tier(host + 1, false))),
        ] {
            let persona = Persona {
                hardware_concurrency: Some(pinned),
                ..Persona::default()
            };
            let frames = drive_attach(StealthObserver::with_persona(
                StealthProfile::spoofed(),
                bare_fingerprint(),
                persona,
            ))
            .await;
            let sent = frames
                .iter()
                .find(|f| f["method"] == "Emulation.setCPUPerformanceOverride")
                .map(|f| f["params"]["performanceTier"].clone());
            assert_eq!(
                sent,
                tier.map(Value::from),
                "{pinned} of the host's {host} CPUs"
            );
        }
    }

    #[test]
    fn cpu_performance_tier_follows_chromes_table() {
        let expected = [
            (1, false, "low"),
            (2, false, "low"),
            (3, false, "mid"),
            (4, true, "mid"),
            (5, false, "high"),
            (8, false, "high"),
            (8, true, "ultra"),
            (10, false, "high"),
            (10, true, "ultra"),
            (11, false, "ultra"),
            (32, false, "ultra"),
        ];
        for (cores, apple_silicon, tier) in expected {
            assert_eq!(
                cpu_performance_tier(cores, apple_silicon),
                tier,
                "{cores} cores, apple_silicon {apple_silicon}"
            );
        }
    }

    /// A Mac persona whose UA-CH architecture is `arm` claims an M-series
    /// CPU, which Chrome rates a tier higher at 8 to 10 cores; an `arm`
    /// Windows persona doesn't.
    #[test]
    fn an_arm_mac_persona_gets_the_tier_of_apple_silicon() {
        let cores = if host_cores() == 8 { 9 } else { 8 };
        let cases = [
            (Platform::MacIntel, None, "high"),
            (Platform::MacIntel, Some("arm"), "ultra"),
            (Platform::Win32, Some("arm"), "high"),
        ];
        for (platform, architecture, tier) in cases {
            let ua = architecture.map(|a| crate::persona::specs::UaSpec {
                ua_metadata: Some(UaMetadata {
                    architecture: Some(a.into()),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let persona = Persona {
                platform: Some(platform),
                hardware_concurrency: Some(cores),
                ua,
                ..Persona::default()
            };
            let observer = StealthObserver::with_persona(
                StealthProfile::spoofed(),
                bare_fingerprint(),
                persona,
            );
            assert_eq!(
                observer.cpu_tier,
                Some(tier),
                "{platform:?}, {architecture:?}"
            );
        }
    }

    /// Chrome 152 and older lack `Emulation.setCPUPerformanceOverride`; the
    /// page must still get the rest of its setup.
    #[tokio::test]
    async fn spoofed_goes_on_without_the_cpu_tier_override() {
        let missing = ["Emulation.setCPUPerformanceOverride"];
        let persona = Persona {
            hardware_concurrency: Some(host_cores() + 1),
            ..Persona::default()
        };
        let observer =
            StealthObserver::with_persona(StealthProfile::spoofed(), bare_fingerprint(), persona);
        let frames = drive_attach_without(observer, &missing).await;
        let methods = method_names(&frames);
        assert!(
            methods
                .iter()
                .any(|m| m == "Emulation.setCPUPerformanceOverride"),
            "{methods:?}"
        );
        assert!(
            methods
                .iter()
                .any(|m| m == "Page.addScriptToEvaluateOnNewDocument"),
            "{methods:?}"
        );
    }

    /// `Persona::timezone` is the caller's explicit pin and must win over the
    /// fingerprint's. The two fixtures are deliberately different real zones:
    /// a persona value equal to the fingerprint's would be satisfied by the
    /// bug, which sent the fingerprint's unconditionally.
    #[tokio::test]
    async fn persona_timezone_wins_over_the_fingerprints_in_the_cdp_override() {
        let fp = Fingerprint {
            timezone: Some("America/New_York".into()),
            ..bare_fingerprint()
        };
        let persona = Persona {
            timezone: Some("Asia/Tokyo".into()),
            ..Persona::default()
        };

        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            fp,
            persona,
        ))
        .await;

        assert_eq!(
            params(&frames, "Emulation.setTimezoneOverride")["timezoneId"].as_str(),
            Some("Asia/Tokyo"),
            "an explicitly set Persona::timezone must reach Emulation.setTimezoneOverride"
        );
    }

    /// The persona is also the only source when the fingerprint pins nothing —
    /// the override was not sent at all in that case, so `Intl` reported the
    /// host's real zone while the rest of the identity claimed otherwise.
    #[tokio::test]
    async fn persona_timezone_is_sent_when_the_fingerprint_pins_none() {
        let persona = Persona {
            timezone: Some("Europe/Berlin".into()),
            ..Persona::default()
        };

        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            bare_fingerprint(),
            persona,
        ))
        .await;

        assert_eq!(
            params(&frames, "Emulation.setTimezoneOverride")["timezoneId"].as_str(),
            Some("Europe/Berlin"),
        );
    }

    /// One persona locale has to move both surfaces at once: the JS-visible
    /// locale (`Emulation.setLocaleOverride`) and the header
    /// (`setUserAgentOverride.acceptLanguage`). Splitting them is the
    /// cross-surface tell the coherence work exists to close.
    #[tokio::test]
    async fn persona_locale_drives_both_the_locale_override_and_accept_language() {
        let fp = Fingerprint {
            locale: Some("en-GB".into()),
            ..bare_fingerprint()
        };
        let persona = Persona {
            locale: Some("fr-FR".into()),
            ..Persona::default()
        };

        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            fp,
            persona,
        ))
        .await;

        assert_eq!(
            params(&frames, "Emulation.setLocaleOverride")["locale"].as_str(),
            Some("fr-FR"),
        );
        assert_eq!(
            params(&frames, "Emulation.setUserAgentOverride")["acceptLanguage"].as_str(),
            Some("fr-FR,fr"),
            "Accept-Language must be derived from the persona's locale, not the fingerprint's"
        );
    }

    /// `Persona::languages` is the more specific pin and outranks a
    /// fingerprint `languages` list on both surfaces.
    ///
    /// The fingerprint pins a *conflicting* `locale` on purpose. Leaving it
    /// `None` is the one case where the two surfaces happened to agree no
    /// matter which order they resolved in, so it could not tell a coherent
    /// implementation from an incoherent one — the locale override resolved
    /// locale-first and the header resolved languages-first, and only an
    /// unset locale hid that.
    #[tokio::test]
    async fn persona_languages_drive_accept_language_and_the_locale_override() {
        let fp = Fingerprint {
            locale: Some("en-GB".into()),
            languages: Some(vec!["en-GB".into(), "en".into()]),
            ..bare_fingerprint()
        };
        let persona = Persona {
            languages: Some(vec!["ja-JP".into(), "ja".into()]),
            ..Persona::default()
        };

        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            fp,
            persona,
        ))
        .await;

        assert_eq!(
            params(&frames, "Emulation.setUserAgentOverride")["acceptLanguage"].as_str(),
            Some("ja-JP,ja"),
        );
        assert_eq!(
            params(&frames, "Emulation.setLocaleOverride")["locale"].as_str(),
            Some("ja-JP"),
            "the locale override must stay coherent with the persona's language list"
        );
    }

    /// An empty persona list is the absence of a value, not a value. Every
    /// other language consumer in the crate already reads it that way
    /// (`lang::fingerprint_languages` filters it), so a merge that treated `Some(vec![])` as a pin let a
    /// persona with no languages destroy a configured one and drop both
    /// surfaces back to the `en-US` default.
    #[tokio::test]
    async fn an_empty_persona_language_list_leaves_the_fingerprints_pin_alone() {
        let fp = Fingerprint {
            languages: Some(vec!["de-DE".into(), "de".into()]),
            ..bare_fingerprint()
        };
        let persona = Persona {
            languages: Some(Vec::new()),
            ..Persona::default()
        };

        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            fp,
            persona,
        ))
        .await;

        assert_eq!(
            params(&frames, "Emulation.setUserAgentOverride")["acceptLanguage"].as_str(),
            Some("de-DE,de"),
            "an empty persona list must not wipe StealthProfile::languages"
        );
        assert_eq!(
            params(&frames, "Emulation.setLocaleOverride")["locale"].as_str(),
            Some("de-DE"),
        );
    }

    /// Browser truth, and the reason the two locale surfaces resolve
    /// languages-first: in a real Chrome `navigator.language` is always
    /// `navigator.languages[0]`. A caller who pins a `locale` and a
    /// disagreeing `languages` list is asking for something Chrome cannot
    /// produce, so the list wins on both surfaces rather than each surface
    /// picking its own answer.
    #[tokio::test]
    async fn the_locale_override_is_the_head_of_the_advertised_language_list() {
        let fp = Fingerprint {
            locale: Some("en-GB".into()),
            languages: Some(vec!["de-DE".into(), "de".into()]),
            ..bare_fingerprint()
        };

        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            fp,
            Persona::default(),
        ))
        .await;

        let accept_language = params(&frames, "Emulation.setUserAgentOverride")["acceptLanguage"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let locale = params(&frames, "Emulation.setLocaleOverride")["locale"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        assert_eq!(accept_language, "de-DE,de");
        assert_eq!(
            locale,
            accept_language.split(',').next().unwrap_or_default(),
            "the locale override must be the head of the list the header advertises; \
             got locale={locale:?} against acceptLanguage={accept_language:?}"
        );
    }

    /// `Native` takes its screen from launch switches (see
    /// [`StealthObserver::headless_flags`]), so whatever screen is configured,
    /// it sends no screen override and no bootstrap.
    #[tokio::test]
    async fn native_sends_no_screen_override_and_no_bootstrap() {
        let fp = Fingerprint {
            screen: Some(ScreenSpec::new(1440, 900, 2.0)),
            ..bare_fingerprint()
        };
        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::native(),
            fp,
            Persona::default(),
        ))
        .await;
        let methods = method_names(&frames);
        for absent in [
            "Emulation.setDeviceMetricsOverride",
            "Emulation.setHardwareConcurrencyOverride",
            "Emulation.setCPUPerformanceOverride",
            "Page.addScriptToEvaluateOnNewDocument",
        ] {
            assert!(
                !methods.iter().any(|m| m == absent),
                "native sent {absent}: {methods:?}"
            );
        }
    }

    /// `native` and `spoofed` send `Page.setBypassCSP` only when asked to,
    /// and `off` never does.
    #[tokio::test]
    async fn the_csp_bypass_is_sent_only_when_asked() {
        for (profile, expected) in [
            (StealthProfile::spoofed(), false),
            (StealthProfile::spoofed().bypass_csp(true), true),
            (StealthProfile::native(), false),
            (StealthProfile::native().bypass_csp(true), true),
            (StealthProfile::off().bypass_csp(true), false),
        ] {
            let frames = drive_attach(StealthObserver::new(profile, bare_fingerprint())).await;
            let sent = method_names(&frames)
                .iter()
                .any(|m| m == "Page.setBypassCSP");
            assert_eq!(sent, expected, "Page.setBypassCSP sent");
        }
    }

    /// `spoofed` takes its screen from the same launch switches as `native`,
    /// and sends no screen override.
    #[tokio::test]
    async fn spoofed_gets_the_screen_of_native() {
        let fp = Fingerprint {
            screen: Some(ScreenSpec::new(1366, 768, 1.0)),
            ..bare_fingerprint()
        };
        let persona = Persona {
            screen: Some(ScreenSpec::new(1440, 900, 2.0).with_avail(1400, 860)),
            ..Persona::default()
        };
        let observer =
            |profile| StealthObserver::with_persona(profile, fp.clone(), persona.clone());
        assert_eq!(
            observer(StealthProfile::spoofed())
                .headless_flags()
                .unwrap(),
            observer(StealthProfile::native()).headless_flags().unwrap(),
        );
        assert!(
            observer(StealthProfile::off())
                .headless_flags()
                .unwrap()
                .is_empty()
        );

        let frames = drive_attach(observer(StealthProfile::spoofed())).await;
        let methods = method_names(&frames);
        assert!(
            !methods
                .iter()
                .any(|m| m == "Emulation.setDeviceMetricsOverride"),
            "spoofed sent a screen override: {methods:?}"
        );
    }

    /// The headless switches of a `native()` observer whose fingerprint
    /// carries `fingerprint_screen` and whose persona carries `persona_screen`.
    fn native_headless_flags(
        fingerprint_screen: Option<ScreenSpec>,
        persona_screen: Option<ScreenSpec>,
    ) -> Result<Vec<String>, StealthError> {
        let fp = Fingerprint {
            screen: fingerprint_screen,
            ..bare_fingerprint()
        };
        let persona = Persona {
            screen: persona_screen,
            ..Persona::default()
        };
        StealthObserver::with_persona(StealthProfile::native(), fp, persona).headless_flags()
    }

    #[test]
    fn native_headless_flags_default_to_1920x1080_with_a_taskbar() {
        assert_eq!(
            native_headless_flags(None, None).unwrap(),
            [
                "--screen-info={1920x1080 devicePixelRatio=1 workAreaRight=0 workAreaBottom=48}",
                "--window-size=1920,1032",
                "--window-position=0,0",
            ]
        );
    }

    /// The persona's screen wins over the fingerprint's, and Chrome takes the
    /// size and the work area in physical pixels.
    #[test]
    fn native_headless_flags_give_the_persona_screen_in_physical_pixels() {
        let flags = native_headless_flags(
            Some(ScreenSpec::new(1366, 768, 1.0)),
            Some(ScreenSpec::new(1440, 900, 2.0).with_avail(1400, 860)),
        )
        .unwrap();
        assert_eq!(
            flags,
            [
                "--screen-info={2880x1800 devicePixelRatio=2 workAreaRight=80 workAreaBottom=80}",
                "--window-size=1400,860",
                "--window-position=0,0",
            ]
        );
    }

    /// A screen no taller than the default taskbar is taken when it sets
    /// its own work area: the default taskbar's height isn't subtracted.
    #[test]
    fn native_headless_flags_take_a_short_screen_with_its_own_work_area() {
        let flags = native_headless_flags(
            Some(ScreenSpec::new(100, 40, 1.0).with_avail(100, 40)),
            None,
        )
        .unwrap();
        assert_eq!(
            flags[0],
            "--screen-info={100x40 devicePixelRatio=1 workAreaRight=0 workAreaBottom=0}"
        );
    }

    /// Whether `native_headless_flags` rejects `screen` as an invalid
    /// override.
    fn rejected(screen: ScreenSpec) -> bool {
        matches!(
            native_headless_flags(Some(screen), None),
            Err(StealthError::InvalidOverride(_))
        )
    }

    /// Screens that headless Chrome can't take, or would report as different
    /// screens, on every platform.
    #[test]
    fn native_headless_flags_reject_a_screen_chrome_cant_take_or_would_misreport() {
        let below_one = ScreenSpec::new(1920, 1080, 0.75);
        // 1.3 isn't exact in Chrome's `f32`.
        let inexact_ratio = ScreenSpec::new(1000, 1000, 1.3);
        let nan_ratio = ScreenSpec::new(1920, 1080, f64::NAN);
        let tall_work_area = ScreenSpec::new(1920, 1080, 1.0).with_avail(1920, 1081);
        let zero_width = ScreenSpec::new(0, 1080, 1.0);
        let empty_work_area = ScreenSpec::new(1920, 1080, 1.0).with_avail(1920, 0);
        // The default taskbar of 48 leaves no work area.
        let too_short_for_the_taskbar = ScreenSpec::new(1920, 40, 1.0);
        let too_wide = ScreenSpec::new((1 << 16) + 1, 1080, 1.0);
        for screen in [
            below_one,
            inexact_ratio,
            nan_ratio,
            tall_work_area,
            zero_width,
            empty_work_area,
            too_short_for_the_taskbar,
            too_wide,
        ] {
            assert!(rejected(screen), "{screen:?} must be rejected");
        }
    }

    /// At a fractional ratio, a size that isn't a whole number of physical
    /// pixels: 2194 x 1.75 = 3839.5. Chrome reports a screen of 3840
    /// physical pixels as 2195, and makes a window 2196 wide.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn native_headless_flags_reject_a_fractional_size_of_physical_pixels() {
        assert!(rejected(ScreenSpec::new(2194, 1232, 1.75)));
        assert!(rejected(
            ScreenSpec::new(2192, 1232, 1.75).with_avail(2192, 1186)
        ));
    }

    /// Fractional ratios were measured only on Linux and Windows.
    #[test]
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    fn native_headless_flags_take_only_ratios_1_and_2_off_linux_and_windows() {
        assert!(rejected(ScreenSpec::new(1536, 864, 1.25)));
        assert!(!rejected(ScreenSpec::new(1440, 900, 2.0)));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn native_headless_flags_take_a_fractional_ratio_with_whole_physical_pixels() {
        let flags = native_headless_flags(
            Some(ScreenSpec::new(1536, 864, 1.25).with_avail(1536, 824)),
            None,
        )
        .unwrap();
        assert_eq!(
            flags[0],
            "--screen-info={1920x1080 devicePixelRatio=1.25 workAreaRight=0 workAreaBottom=50}"
        );
    }

    /// At 1.75, Chrome would report 84 physical pixels of inset as 49 CSS
    /// pixels on Linux, and on Windows, a screen of 2800 as 1601 (see
    /// `chrome_axis`).
    #[test]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn native_headless_flags_make_up_for_chromes_rounding_at_1_75() {
        let flags = |screen| native_headless_flags(Some(screen), None).unwrap();
        let screen_info = if cfg!(target_os = "windows") {
            [
                "--screen-info={3836x2156 devicePixelRatio=1.75 workAreaRight=0 workAreaBottom=84}",
                "--screen-info={2799x1567 devicePixelRatio=1.75 workAreaRight=7 workAreaBottom=84}",
            ]
        } else {
            [
                "--screen-info={3836x2156 devicePixelRatio=1.75 workAreaRight=0 workAreaBottom=83}",
                "--screen-info={2800x1568 devicePixelRatio=1.75 workAreaRight=7 workAreaBottom=83}",
            ]
        };
        assert_eq!(
            flags(ScreenSpec::new(2192, 1232, 1.75)),
            [
                screen_info[0],
                "--window-size=2192,1184",
                "--window-position=0,0",
            ]
        );
        assert_eq!(
            flags(ScreenSpec::new(1600, 896, 1.75).with_avail(1596, 848)),
            [
                screen_info[1],
                "--window-size=1595,847",
                "--window-position=0,0",
            ]
        );
    }

    /// Measured with Chrome 157: at 1.75, Chrome on Linux reports 84
    /// physical pixels of inset as 49 CSS pixels, and Chrome on Linux and
    /// Windows reports a `--window-size` of 1596 as 1597.
    #[test]
    fn chromes_rounding_is_made_up_for() {
        assert_eq!(chrome_physical("inset", 48, 84, 1.75).unwrap(), 83);
        assert_eq!(chrome_window_size(1596, 1.75).unwrap(), 1595);
        assert_eq!(chrome_window_size(2192, 1.75).unwrap(), 2192);
        assert_eq!(chrome_physical("inset", 40, 50, 1.25).unwrap(), 50);
        assert_eq!(chrome_window_size(1920, 1.0).unwrap(), 1920);
    }

    /// `chrome_physical` and `chrome_window_size` find a value for every
    /// size that `headless_screen_switches` can pass them on Linux and
    /// Windows (a superset of what it can pass elsewhere): every ratio from
    /// 1 to 4 in quarters, and every size of whole physical pixels up to
    /// `MAX_PHYSICAL`.
    #[test]
    fn chromes_rounding_can_always_be_made_up_for() {
        for quarters in 4..=16u32 {
            let dpr = quarters as f32 / 4.0;
            for css in 1..=(MAX_PHYSICAL as u32) * 4 / quarters {
                if css * quarters % 4 != 0 {
                    continue;
                }
                let exact = css * quarters / 4;
                assert!(
                    chrome_physical("size", css, exact, dpr).is_ok(),
                    "size {css} at {dpr}"
                );
                assert!(
                    chrome_window_size(css, dpr).is_ok(),
                    "window {css} at {dpr}"
                );
            }
        }
    }

    /// The documented fallback for both `Persona::timezone` and
    /// `Fingerprint::timezone`: nothing configured means no override, so
    /// Chrome keeps the host's zone rather than being pinned to a fabricated
    /// default. Only [`drive_attach`] can assert this — it returns the whole
    /// ordered frame list, where absence is observable.
    #[tokio::test]
    async fn no_timezone_or_locale_configured_sends_neither_override() {
        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            bare_fingerprint(),
            Persona::default(),
        ))
        .await;

        let methods = method_names(&frames);
        assert!(
            !methods.iter().any(|m| m == "Emulation.setTimezoneOverride"),
            "no timezone is configured, so no override may be sent; got {methods:?}"
        );
        assert!(
            !methods.iter().any(|m| m == "Emulation.setLocaleOverride"),
            "no locale and no languages are configured, so no override may be sent; \
             got {methods:?}"
        );
    }

    /// The symmetric case to
    /// [`persona_timezone_is_sent_when_the_fingerprint_pins_none`]: a
    /// `StealthProfile::timezone` pin with no persona at all still has to
    /// reach the wire. The merge is what both directions run through, so
    /// covering only the persona direction would leave half of it unheld.
    #[tokio::test]
    async fn fingerprint_timezone_reaches_cdp_when_the_persona_pins_none() {
        let fp = Fingerprint {
            timezone: Some("Australia/Sydney".into()),
            ..bare_fingerprint()
        };

        let frames = drive_attach(StealthObserver::with_persona(
            StealthProfile::spoofed(),
            fp,
            Persona::default(),
        ))
        .await;

        assert_eq!(
            params(&frames, "Emulation.setTimezoneOverride")["timezoneId"].as_str(),
            Some("Australia/Sydney"),
        );
    }
}
