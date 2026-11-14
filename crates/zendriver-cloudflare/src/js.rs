//! The JavaScript this crate runs in the page, templated from
//! [`TurnstileSelectors`].
//!
//! Both sources run in an isolated world of the main frame (see
//! `detection::World`), whose built-ins no page script can reach, so the
//! page sees none of the calls they make:
//!
//! - [`poll_expr`], evaluated once per poll tick. It reads the light DOM
//!   only: the token input and the container, which Turnstile leaves outside
//!   its shadow root. The widget's `<iframe>` is not looked for here: it
//!   sits in a closed shadow root, which no path through the DOM leads
//!   into, so the driver finds it through CDP instead (see `detection`).
//! - [`MEASURE_FN`], called on that `<iframe>` element on a tick that may
//!   click, before and after scrolling it into view.
//!
//! [`poll_expr`] is one IIFE, so that its `var`s stay locals rather than
//! piling up on the world's global object, tick after tick.

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
/// - `token` — the first non-empty value of an element matching one of the
///   [`token_inputs`](TurnstileSelectors::token_inputs), trying the
///   selectors in order, else null.
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
    var token = null;
    var hasInput = false;
    for (var k = 0; k < tokenSelectors.length && !token; k++) {{
        if (!tokenSelectors[k]) continue;
        var inputs = document.querySelectorAll(tokenSelectors[k]);
        for (var i = 0; i < inputs.length && !token; i++) {{
            hasInput = true;
            if (inputs[i].value) token = inputs[i].value;
        }}
    }}
    var containerSelector = {container};
    var hasContainer = containerSelector ? !!document.querySelector(containerSelector) : false;
    return {{ token: token, markers: hasContainer || hasInput, loading: document.readyState === "loading" }};
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

    /// The poll evaluator is one IIFE: nothing it declares outlives the
    /// tick. `MEASURE_FN` is a bare function declaration, which is what
    /// `Runtime.callFunctionOn` takes.
    #[test]
    fn the_sources_have_the_shapes_their_cdp_calls_take() {
        let poll = poll_expr(&TurnstileSelectors::default());
        assert!(poll.starts_with("(function(){") && poll.ends_with("})()"));
        assert!(MEASURE_FN.starts_with("function() {"));
    }
}
