//! The JavaScript this crate runs in the page, templated from
//! [`TurnstileSelectors`].
//!
//! Two sources, both in the page's main world:
//!
//! - [`poll_expr`], evaluated once per poll tick. It reads the light DOM
//!   only: the token input and the container, which Turnstile leaves outside
//!   its shadow root. The widget's `<iframe>` is *not* looked for here — it
//!   sits in a closed shadow root that no page script can enter, so the
//!   driver finds it through CDP instead (see `detection`).
//! - [`MEASURE_FN`], called on that `<iframe>` element once CDP has named
//!   it.
//!
//! [`poll_expr`] is composed as *one IIFE*. That is a stealth requirement,
//! not formatting. `Runtime.evaluate` with no `contextId` runs a classic
//! script in the page's main world, where a top-level `var foo` becomes a
//! property of the global object — so an unwrapped evaluator would publish
//! its locals to the page, names belonging to an automation library sitting
//! in the same realm as the challenge script. Inside the IIFE they are
//! ordinary locals. [`MEASURE_FN`] is a function body by construction
//! (`Runtime.callFunctionOn` takes a function declaration), so it binds
//! nothing either.
//!
//! # JavaScript style
//! Injected scripts declare with `var` and iterate with indexed loops, never
//! `for...of`, which enters through the page-redefinable
//! `Array.prototype[Symbol.iterator]`. This removes one signal and no more:
//! `document.querySelector`, `getBoundingClientRect` and `getComputedStyle`
//! are ordinary writable methods, and a page that wraps them sees every call
//! this crate makes, the selectors it passes included. The convention is
//! about declarations and iteration only; it is not cover.

use crate::options::TurnstileSelectors;

/// Render `value` as a JavaScript string literal, escaped.
///
/// Mandatory rather than cosmetic: the default token selectors contain
/// double quotes (`[name="cf-turnstile-response"]`), so naive interpolation
/// terminates the literal early and injects whatever follows. JSON string
/// syntax is a subset of JavaScript's, and serializing a `&str` cannot fail
/// — the fallback is unreachable and exists only to keep this total.
fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Render `values` as a JavaScript array-of-strings literal. Same escaping
/// argument as [`js_string`].
fn js_string_array(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

/// The poll evaluator. Returns, in one round-trip:
/// - `token` — the first non-empty value among
///   [`token_inputs`](TurnstileSelectors::token_inputs), else null.
/// - `markers` — true when the light DOM holds the
///   [`container`](TurnstileSelectors::container) or a token input.
/// - `loading` — true while the document is still being parsed
///   (`readyState == "loading"`).
pub(crate) fn poll_expr(selectors: &TurnstileSelectors) -> String {
    let container = js_string(&selectors.container);
    let token_selectors = js_string_array(&selectors.token_inputs);
    format!(
        r#"(function(){{
    var tokenSelectors = {token_selectors};
    var input = null;
    for (var k = 0; k < tokenSelectors.length && !input; k++) {{
        if (tokenSelectors[k]) input = document.querySelector(tokenSelectors[k]);
    }}
    var token = (input && input.value) ? input.value : null;
    var containerSelector = {container};
    var hasContainer = containerSelector ? !!document.querySelector(containerSelector) : false;
    return {{ token: token, markers: hasContainer || !!input, loading: document.readyState === "loading" }};
}})()"#
    )
}

/// Function declaration for `Runtime.callFunctionOn` on the widget's
/// `<iframe>`: returns its viewport rect *only if it is a real click
/// target* — non-zero size and not hidden by `visibility` / `display` /
/// `opacity` — else null.
///
/// A zero-size or hidden iframe is not a target: invisible Turnstile mounts
/// a 0×0 iframe whose token is populated with no click, and clicking it
/// would dispatch mouse events at a meaningless point.
pub(crate) const MEASURE_FN: &str = r#"function() {
    var r = this.getBoundingClientRect();
    if (!(r.width > 0 && r.height > 0)) return null;
    var view = this.ownerDocument && this.ownerDocument.defaultView;
    var style = view && view.getComputedStyle ? view.getComputedStyle(this) : null;
    if (style && (style.visibility === "hidden" || style.display === "none" || style.opacity === "0")) {
        return null;
    }
    return { x: r.left, y: r.top, width: r.width, height: r.height };
}"#;

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    use crate::options::TurnstileSelectors;

    /// Selectors sharing no substring with any Cloudflare default, so an
    /// assertion that the defaults are absent cannot be satisfied by a
    /// source that ignored the overrides.
    fn alien() -> TurnstileSelectors {
        TurnstileSelectors {
            iframe_src_contains: "gate.alien.invalid".into(),
            container: ".alien-widget".into(),
            token_inputs: vec!["[name=\"alien-token\"]".into()],
        }
    }

    /// The poll evaluator must be built from the caller's markers. The
    /// negative half is the load-bearing one: a source still carrying
    /// `.cf-turnstile` is a source with the marker baked in.
    #[test]
    fn custom_markers_template_into_the_poll_evaluator() {
        let poll = poll_expr(&alien());
        assert!(poll.contains(".alien-widget"), "container marker missing");
        assert!(poll.contains("alien-token"), "token marker missing");
        for default in [
            "challenges.cloudflare.com",
            ".cf-turnstile",
            "cf-turnstile-response",
        ] {
            assert!(
                !poll.contains(default),
                "the poll evaluator still carries the hardcoded {default:?}"
            );
        }
    }

    /// An empty `container` must switch the container signal off, not ask the
    /// page for `document.querySelector("")` — which is a `SyntaxError`, and
    /// throwing takes the whole poll evaluator down rather than just this one
    /// marker. The ternary is the guard; without it the empty string reaches
    /// `querySelector` directly.
    ///
    /// Asserted on where the guard sits, not merely that it exists: a check
    /// placed after the call guards nothing.
    #[test]
    fn an_empty_container_marker_disables_the_signal_rather_than_raising() {
        let src = poll_expr(&TurnstileSelectors {
            container: String::new(),
            ..TurnstileSelectors::default()
        });
        let guard = src
            .find("containerSelector ?")
            .expect("an empty container must short-circuit before querySelector");
        let call = src
            .find("querySelector(containerSelector)")
            .expect("the container lookup itself went missing");
        assert!(
            guard < call,
            "the empty-container guard must precede the lookup it guards"
        );
    }

    /// Selectors are caller data, and the default token selectors already
    /// contain double quotes. Interpolating them raw would terminate the JS
    /// string literal early and inject whatever followed.
    ///
    /// `token_inputs` carries two entries deliberately. Both `options.rs` and
    /// the book promise the modern input is preferred over the legacy one, and
    /// a single-element fixture cannot see order at all — reversing the emitted
    /// array would satisfy it just as well as preserving it.
    #[test]
    fn selector_strings_are_escaped_into_js_literals() {
        let sel = TurnstileSelectors {
            iframe_src_contains: String::new(),
            container: "[data-x=\"quote\\\"]".into(),
            token_inputs: vec!["[name=\"tok\"]".into(), "[name=\"legacy-tok\"]".into()],
        };
        let src = poll_expr(&sel);

        assert!(
            src.contains(&serde_json::to_string(&sel.token_inputs).unwrap()),
            "token selectors must reach the page as one escaped array, in order"
        );
        let escaped = serde_json::to_string(&sel.container).unwrap();
        assert!(
            src.contains(&escaped),
            "{:?} was not emitted as the JS literal {escaped}",
            sel.container
        );
        assert!(
            !src.contains(&format!("\"{}\"", sel.container)),
            "the container was interpolated raw — the literal terminates early"
        );
    }

    /// Names every declaration in `src` at `{}` depth 0 that a classic
    /// script publishes on the page's global object: `var` and *named*
    /// `function`. Both become enumerable `window` properties (verified on
    /// Chrome 151); `let` / `const` / `class` are script-scoped and never
    /// do, so they are not leaks and are not counted. An anonymous
    /// `function (` — the IIFE wrapper itself — is an expression and binds
    /// nothing.
    ///
    /// Brace counting is only sound because no injected source puts a brace
    /// inside a string literal. Keep it that way; the alternative is a JS
    /// parser for a cookie-name-sized job. That includes caller-supplied
    /// selectors, which is why the fixtures here use quotes and backslashes
    /// rather than braces. Comment text is scanned like any other, which can
    /// only ever produce a loud false positive, never a silent pass.
    fn top_level_page_globals(src: &str) -> Vec<String> {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        let mut found = Vec::new();
        let mut depth = 0i32;
        for (i, ch) in src.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth != 0 {
                continue;
            }
            // A keyword that is really the tail of a longer identifier
            // declares nothing: `myvar`, `refunction`.
            if src[..i].chars().next_back().is_some_and(is_ident) {
                continue;
            }
            for keyword in ["function", "var"] {
                let Some(rest) = src[i..].strip_prefix(keyword) else {
                    continue;
                };
                // `var` needs a separator to be a declaration. `function`
                // does not: `function(` is an expression, and the empty-name
                // check below is what rejects it.
                if keyword == "var" && !rest.starts_with(char::is_whitespace) {
                    break;
                }
                let name: String = rest
                    .trim_start()
                    .chars()
                    .take_while(|c| is_ident(*c))
                    .collect();
                if !name.is_empty() {
                    found.push(name);
                }
                break;
            }
        }
        found
    }

    /// The leak guard has to be able to fail. Fed the shape it guards
    /// against — a helper evaluated beside the IIFE rather than inside it —
    /// it must name the declaration; fed the shipped shape it must stay
    /// quiet. The `var` case is here because the scan is named for page
    /// globals, and a top-level `var` is one just as much as a `function`
    /// is.
    #[test]
    fn top_level_scan_catches_every_leaked_page_global() {
        assert_eq!(
            top_level_page_globals(
                "function clickableRect(el) { return null; }\n(function(){ return 1; })()"
            ),
            vec!["clickableRect".to_string()],
        );
        assert_eq!(
            top_level_page_globals("var input = document.querySelector('input');"),
            vec!["input".to_string()],
            "a top-level `var` is a `window` property too"
        );
        assert!(
            top_level_page_globals("(function(){ function nested(){} var local = 1; })()")
                .is_empty(),
            "a declaration inside the wrapper is a local, not a global"
        );
        // Script-scoped forms never reach `window`, so flagging them would
        // be a false positive, and a keyword inside a longer identifier is
        // not a keyword at all.
        assert!(
            top_level_page_globals("let a = 1; const b = 2; class C {}\nvarnish.myvar = 3;")
                .is_empty(),
        );
    }

    /// `Runtime.evaluate` with no `contextId` runs a classic script in the
    /// page's main world, where a top-level `var bar` becomes `window.bar`.
    /// Every source this crate injects keeps its locals inside a function
    /// scope.
    #[test]
    fn injected_sources_declare_no_page_globals() {
        let poll = poll_expr(&TurnstileSelectors::default());
        for (name, src) in [
            ("poll evaluator", poll.as_str()),
            ("measure function", MEASURE_FN),
        ] {
            let leaked = top_level_page_globals(src);
            assert!(
                leaked.is_empty(),
                "{name} publishes {leaked:?} onto the page's global object"
            );
        }
        assert!(
            poll.starts_with("(function(){") && poll.ends_with("})()"),
            "the poll evaluator must be exactly one IIFE"
        );
        assert!(
            MEASURE_FN.starts_with("function() {"),
            "callFunctionOn takes an anonymous function declaration"
        );
    }
}
