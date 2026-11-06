//! CDP-style URL pattern matching.
//!
//! Compiles a CDP wildcard pattern (`*` matches any sequence, `?` matches
//! zero or one character, `\` makes the next character literal) into a
//! [`regex::Regex`]. All other regex metacharacters in the input are escaped
//! so the user-facing surface stays the simple CDP syntax used by
//! `Fetch.RequestPattern.urlPattern`, with the meaning Chrome gives it.

use regex::Regex;

use crate::error::InterceptionError;

/// A compiled CDP URL pattern.
///
/// Constructed via [`UrlPattern::new`] from a string using CDP wildcard
/// syntax. Use [`UrlPattern::matches`] to test URLs against it, and
/// [`UrlPattern::pattern_str`] to recover the original pattern (e.g. when
/// forwarding to `Fetch.enable { patterns }`).
#[derive(Debug, Clone)]
pub struct UrlPattern {
    pattern: String,
    regex: Regex,
}

impl UrlPattern {
    /// Compile a CDP-style URL pattern.
    ///
    /// `*` matches any sequence of characters (including empty), `?` matches
    /// zero or one character, `\` makes the next character literal (so `\*`
    /// matches a `*`), and every other character matches itself: what Chrome
    /// gives a `Fetch.RequestPattern`'s `urlPattern` (`base::MatchPattern`).
    ///
    /// Returns [`InterceptionError::InvalidPattern`] if the resulting regex
    /// fails to compile.
    #[allow(clippy::result_large_err)] // InterceptionError shape fixed by error.rs; boxing is a cross-cutting decision tracked separately.
    pub fn new(pattern: impl Into<String>) -> Result<Self, InterceptionError> {
        let pattern = pattern.into();
        let regex_src = compile_to_regex(&pattern);
        let regex = Regex::new(&regex_src)
            .map_err(|e| InterceptionError::InvalidPattern(format!("{pattern}: {e}")))?;
        Ok(Self { pattern, regex })
    }

    /// Test whether `url` matches this pattern.
    pub fn matches(&self, url: &str) -> bool {
        self.regex.is_match(url)
    }

    /// The original pattern string passed to [`UrlPattern::new`].
    ///
    /// Useful when forwarding the pattern to CDP via
    /// `Fetch.enable { patterns: [{ urlPattern: ... }] }`.
    pub fn pattern_str(&self) -> &str {
        &self.pattern
    }
}

/// Convert a CDP wildcard pattern into an anchored regex source.
///
/// `*` → `.*`, `?` → `.?`, `\c` → a literal `c` (a trailing `\` is
/// dropped, as Chrome drops it), and all other regex metacharacters are
/// escaped via [`regex::escape`]. The result is anchored at both ends so
/// `matches` behaves as a full-URL match.
fn compile_to_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    out.push('^');
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => out.push_str(".*"),
            '?' => out.push_str(".?"),
            '\\' => {
                if let Some(literal) = chars.next() {
                    out.push_str(&regex::escape(&literal.to_string()));
                }
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');
    out
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_star_matches_all() {
        let p = UrlPattern::new("*").unwrap();
        assert!(p.matches("https://example.com/"));
        assert!(p.matches("http://foo.bar/baz?qux=1"));
        assert!(p.matches(""));
        assert_eq!(p.pattern_str(), "*");
    }

    #[test]
    fn subdomain_wildcard_matches() {
        let p = UrlPattern::new("*.example.com/*").unwrap();
        assert!(p.matches("https://api.example.com/foo"));
        assert!(p.matches("ws://cdn.example.com/socket"));
        assert!(!p.matches("https://example.org/foo"));
    }

    /// As in Chrome's `base::MatchPattern`: `?` matches zero or one
    /// character, and `\` makes the next character literal.
    #[test]
    fn question_mark_and_backslash_match_as_in_chrome() {
        let p = UrlPattern::new("https://a.test/x?.js").unwrap();
        assert!(p.matches("https://a.test/x.js"));
        assert!(p.matches("https://a.test/x1.js"));
        assert!(!p.matches("https://a.test/x12.js"));

        let p = UrlPattern::new(r"https://a.test/\*\?").unwrap();
        assert!(p.matches("https://a.test/*?"));
        assert!(!p.matches("https://a.test/ab"));
    }

    #[test]
    fn invalid_pattern_errors() {
        // Our wildcard expansion escapes every regex metachar in the input
        // so syntax-level failures (`[`, `\`, etc.) cannot happen via user
        // input. The deterministic failure path is `regex`'s default
        // 10 MiB compiled-size limit: enough `*` wildcards expand into
        // enough `.*` repetition to trip it.
        let huge = "*".repeat(50_000);
        let err = UrlPattern::new(huge).expect_err("expected size-limit failure");
        match err {
            InterceptionError::InvalidPattern(msg) => {
                assert!(
                    msg.contains("exceeds size limit") || msg.contains("size"),
                    "expected size-limit message, got: {msg}",
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
