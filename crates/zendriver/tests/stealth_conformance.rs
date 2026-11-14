//! The stealth JS patches must keep what real Chrome guarantees: each
//! invariant below holds in a stock Chrome (`StealthProfile::off()`, which
//! checks the invariant itself), and must still hold under every profile
//! that patches the page.
//!
//! An invariant broken by a known, open bug is listed in [`KNOWN_BUGS`]:
//! the test fails on any broken invariant that isn't listed, and on any
//! listed one that holds (fixed, so delete its row).
//!
//! Linux only, like `stealth_probe.rs`: which patches apply depends on the
//! host OS, and CI runs these on Linux.

#![cfg(all(feature = "integration-tests", target_os = "linux"))]
#![allow(clippy::panic)]

use std::collections::BTreeMap;

use serde_json::Value;
use zendriver::stealth::{Platform, StealthProfile};
use zendriver::{Browser, BrowserBuilder, Persona};
use zendriver_testkit::{FixtureServer, eval, launch_for_test, load};

/// `(name, body)`: the body of an async JS function that returns `null` if
/// the invariant holds, or else a string saying what's wrong.
const INVARIANTS: &[(&str, &str)] = &[
    (
        "blank_canvas_reads_zeros",
        r#"
        const canvas = document.createElement('canvas');
        canvas.width = 16;
        canvas.height = 16;
        const data = canvas.getContext('2d').getImageData(0, 0, 16, 16).data;
        const at = data.findIndex(v => v !== 0);
        return at < 0 ? null : `byte ${at} is ${data[at]}; first pixel is ${[...data.slice(0, 4)]}`;
        "#,
    ),
    (
        "notification_permission_states_are_valid",
        r#"
        const permission = Notification.permission;
        const status = await navigator.permissions.query({ name: 'notifications' });
        const queried = { default: 'prompt', granted: 'granted', denied: 'denied' };
        if (!(permission in queried)) return `Notification.permission is ${permission}`;
        if (!(status instanceof PermissionStatus)) return 'query() result is not a PermissionStatus';
        if (status.state !== queried[permission]) {
          return `Notification.permission is ${permission} but query() says ${status.state}`;
        }
        return null;
        "#,
    ),
    (
        "plugins_are_a_plugin_array_of_5",
        r#"
        const plugins = navigator.plugins;
        if (!(plugins instanceof PluginArray)) return 'navigator.plugins is not a PluginArray';
        if (plugins.length !== 5) return `navigator.plugins.length is ${plugins.length}`;
        if (plugins.item(0) !== plugins[0]) return 'plugins.item(0) is not plugins[0]';
        if (plugins.namedItem(plugins[0].name) !== plugins[0]) {
          return 'plugins.namedItem(plugins[0].name) is not plugins[0]';
        }
        return null;
        "#,
    ),
    (
        "client_rects_are_a_dom_rect_list",
        r#"
        const rects = document.body.getClientRects();
        if (!(rects instanceof DOMRectList)) return `getClientRects() is a ${rects.constructor.name}`;
        const box = document.body.getBoundingClientRect();
        if (!(box instanceof DOMRect)) return `getBoundingClientRect() is a ${box.constructor.name}`;
        return null;
        "#,
    ),
    (
        "patched_functions_are_not_constructors",
        r#"
        const getter = (proto, name) => Object.getOwnPropertyDescriptor(proto, name)?.get;
        const functions = {
          'Function.prototype.toString': Function.prototype.toString,
          'HTMLCanvasElement.prototype.toDataURL': HTMLCanvasElement.prototype.toDataURL,
          'CanvasRenderingContext2D.prototype.getImageData':
            CanvasRenderingContext2D.prototype.getImageData,
          'CanvasRenderingContext2D.prototype.measureText':
            CanvasRenderingContext2D.prototype.measureText,
          'WebGLRenderingContext.prototype.getParameter':
            WebGLRenderingContext.prototype.getParameter,
          'Element.prototype.getClientRects': Element.prototype.getClientRects,
        };
        const wrong = [];
        for (const [name, f] of Object.entries(functions)) {
          if (typeof f !== 'function') {
            wrong.push(`${name} is a ${typeof f}`);
            continue;
          }
          if (Object.prototype.hasOwnProperty.call(f, 'prototype')) wrong.push(`${name} has a prototype`);
          // Throws unless f is a constructor, without calling f.
          try {
            Reflect.construct(Object, [], f);
            wrong.push(`${name} is a constructor`);
          } catch (e) {}
        }
        return wrong.length ? wrong.join('; ') : null;
        "#,
    ),
    (
        // A page can replace these built-ins and watch for calls to its
        // replacement. Real Chrome's `toString` calls none of them.
        "to_string_calls_no_replaceable_builtin",
        r#"
        const apply = Reflect.apply;
        const toString = Function.prototype.toString;
        const targets = {
          'WeakMap.prototype.get': [WeakMap.prototype, 'get'],
          'Function.prototype.call': [Function.prototype, 'call'],
          'Function.prototype.apply': [Function.prototype, 'apply'],
          'Reflect.apply': [Reflect, 'apply'],
        };
        const originals = {};
        const called = new Set();
        for (const [name, [object, key]] of Object.entries(targets)) {
          const original = object[key];
          originals[name] = original;
          object[key] = function (...args) {
            called.add(name);
            return apply(original, this, args);
          };
        }
        try {
          apply(toString, toString, []);
          apply(toString, function f() {}, []);
        } finally {
          for (const [name, [object, key]] of Object.entries(targets)) object[key] = originals[name];
        }
        return called.size ? `toString called ${[...called].join(', ')}` : null;
        "#,
    ),
    (
        "properties_live_on_prototypes",
        r#"
        const objects = {
          navigator,
          screen,
          'screen.orientation': screen.orientation,
          'navigator.permissions': navigator.permissions,
          'navigator.mediaDevices': navigator.mediaDevices,
          speechSynthesis,
          'document.fonts': document.fonts,
        };
        const wrong = Object.entries(objects)
          .map(([name, o]) => [name, Object.getOwnPropertyNames(o)])
          .filter(([, own]) => own.length)
          .map(([name, own]) => `${name} has own ${own.join(', ')}`);
        for (const name of ['addEventListener', 'removeEventListener']) {
          if (Object.prototype.hasOwnProperty.call(Document.prototype, name)) {
            wrong.push(`Document.prototype has own ${name}`);
          }
        }
        return wrong.length ? wrong.join('; ') : null;
        "#,
    ),
    (
        // Real Chrome's `check` is true for any local font, even a missing
        // one. A persona's font allow-list makes the patched `check` false
        // for unlisted families, but the profile's list has both of these.
        "fonts_check_finds_listed_families",
        r#"
        const missing = ['16px "Segoe UI"', '16px Arial'].filter(font => !document.fonts.check(font));
        return missing.length ? `document.fonts.check is false for ${missing.join(', ')}` : null;
        "#,
    ),
    (
        "measure_text_is_stable",
        r#"
        const text = 'The quick brown fox jumps over the lazy dog';
        const context = () => document.createElement('canvas').getContext('2d');
        const first = context();
        const alignedRight = context();
        alignedRight.textAlign = 'right';
        const widths = [first.measureText(text).width, first.measureText(text).width,
          context().measureText(text).width, alignedRight.measureText(text).width];
        if (!widths.every(w => w === widths[0])) return `widths ${widths.join(', ')}`;
        const empty = first.measureText('').width;
        return empty === 0 ? null : `no text is ${empty} wide`;
        "#,
    ),
    (
        "inner_size_covers_client_size",
        r#"
        const root = document.documentElement;
        if (innerHeight < root.clientHeight) {
          return `innerHeight ${innerHeight} < documentElement.clientHeight ${root.clientHeight}`;
        }
        if (innerWidth < root.clientWidth) {
          return `innerWidth ${innerWidth} < documentElement.clientWidth ${root.clientWidth}`;
        }
        return null;
        "#,
    ),
];

/// One JS expression that runs every invariant and resolves to
/// `{name: null | "what's wrong"}`. An invariant that throws reports that.
fn invariants_js() -> String {
    let mut js = String::from("(async () => {\n  const results = {};\n");
    for (name, body) in INVARIANTS {
        js.push_str(&format!(
            "  results[{name:?}] = await (async () => {{ {body} }})().catch(e => 'threw ' + e);\n"
        ));
    }
    js.push_str("  return results;\n})()");
    js
}

/// The profiles that patch the page with JS (`native()`, the default, patches
/// none).
const SPOOFED: &[&str] = &["spoofed", "spoofed win32", "spoofed fonts"];

/// `(invariant, profiles, why)`: an invariant broken under each of
/// `profiles` by an open bug, named by its TODO item. Recorded on Linux with
/// Chrome 156.
const KNOWN_BUGS: &[(&str, &[&str], &str)] = &[
    (
        "blank_canvas_reads_zeros",
        SPOOFED,
        "P1 Stealth: canvas farbling turns transparent pixels into (1,1,1,0) (ST #2)",
    ),
    (
        "client_rects_are_a_dom_rect_list",
        SPOOFED,
        "P1 Stealth: getClientRects() returns an Array (ST #11)",
    ),
    (
        "patched_functions_are_not_constructors",
        SPOOFED,
        "P1 Stealth: masked functions are constructible (ST #12)",
    ),
    (
        "properties_live_on_prototypes",
        &["spoofed fonts"],
        "P1 Stealth: own properties on instances (ST #13)",
    ),
    (
        "fonts_check_finds_listed_families",
        &["spoofed fonts"],
        "P1 Stealth: fonts.check keeps only the last word of the family name (ST #6)",
    ),
];

/// Run every invariant in a page of a browser launched from `builder`, and
/// check that the broken ones are exactly `profile`'s rows in
/// [`KNOWN_BUGS`].
async fn check(profile: &str, builder: BrowserBuilder) {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(builder).await;
    let tab = browser.main_tab();
    // 127.0.0.1 is a secure context, which `Notification` needs.
    load(&tab, &server.url("127.0.0.1", "/plain")).await;
    let results = eval(&tab, &invariants_js()).await;
    browser.close().await;

    // A missing result (say, a body that returns undefined, which JSON
    // drops) counts as broken, not as holding.
    let missing = Value::String("no result".to_string());
    let broken: BTreeMap<&str, &Value> = INVARIANTS
        .iter()
        .map(|(name, _)| (*name, results.get(name).unwrap_or(&missing)))
        .filter(|(_, result)| !result.is_null())
        .collect();
    let known: Vec<&str> = KNOWN_BUGS
        .iter()
        .filter(|(_, profiles, _)| profiles.contains(&profile))
        .map(|(name, ..)| *name)
        .collect();
    let unexpected: Vec<String> = broken
        .iter()
        .filter(|(name, _)| !known.contains(name))
        .map(|(name, result)| format!("{name}: {result}"))
        .collect();
    let fixed: Vec<&str> = known
        .iter()
        .copied()
        .filter(|name| !broken.contains_key(name))
        .collect();
    assert!(
        unexpected.is_empty() && fixed.is_empty(),
        "under {profile}:\nbroken invariants not in KNOWN_BUGS:\n  {}\ninvariants in KNOWN_BUGS \
         that now hold (delete their rows): {fixed:?}\nall results: {results:#}",
        unexpected.join("\n  "),
    );
}

#[tokio::test]
async fn invariants_hold_with_stealth_off() {
    check("off", Browser::builder().stealth(StealthProfile::off())).await;
}

#[tokio::test]
async fn invariants_hold_by_default() {
    check("default", Browser::builder()).await;
}

#[tokio::test]
async fn invariants_hold_under_native() {
    check(
        "native",
        Browser::builder().stealth(StealthProfile::native()),
    )
    .await;
}

#[tokio::test]
async fn invariants_hold_under_spoofed() {
    check(
        "spoofed",
        Browser::builder().stealth(StealthProfile::spoofed()),
    )
    .await;
}

#[tokio::test]
async fn invariants_hold_under_spoofed_win32() {
    let builder = Browser::builder().stealth(StealthProfile::spoofed().platform(Platform::Win32));
    check("spoofed win32", builder).await;
}

/// The only profile with a font allow-list, which the host persona lacks.
#[tokio::test]
async fn invariants_hold_under_spoofed_with_a_font_list() {
    let fonts: Persona = r#"{"fonts": {"available": ["Segoe UI", "Arial"]}}"#
        .parse()
        .expect("persona JSON");
    let builder = Browser::builder()
        .stealth(StealthProfile::spoofed())
        .persona_overlay(fonts);
    check("spoofed fonts", builder).await;
}
