//! A query whose JavaScript throws, such as one given an invalid CSS
//! selector, XPath or regex, fails at once with `JsException` instead of
//! polling until its timeout and then reporting that nothing matched.

#![cfg(feature = "integration-tests")]

use std::future::Future;
use std::time::{Duration, Instant};

use zendriver::{Browser, ZendriverError};
use zendriver_testkit::{FixtureServer, SITE_A, launch_for_test, load};

/// The timeout every query below is given. A query that polled would take
/// all of it, so it is far longer than [`AT_ONCE`].
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a query that fails on its first poll may take, with slack for a
/// loaded machine.
const AT_ONCE: Duration = Duration::from_secs(5);

/// Panic unless `query` fails within [`AT_ONCE`] with a `JsException` whose
/// message contains `needle`.
///
/// `name` is the query as written, for the panic messages.
async fn assert_fails_at_once<T>(
    name: &str,
    needle: &str,
    query: impl Future<Output = zendriver::Result<T>>,
) {
    let started = Instant::now();
    let result = query.await;
    let elapsed = started.elapsed();
    match result {
        Err(ZendriverError::JsException(message)) => assert!(
            message.contains(needle),
            "{name}: the exception should mention {needle:?}, got {message:?}"
        ),
        Err(e) => panic!("{name}: expected a JsException, got {e:?}"),
        Ok(_) => panic!("{name}: expected a JsException, but the query succeeded"),
    }
    assert!(
        elapsed < AT_ONCE,
        "{name}: failed after {elapsed:?}, so it polled instead of failing at once"
    );
}

#[tokio::test]
async fn a_query_whose_script_throws_fails_at_once_with_the_exception() {
    let server = FixtureServer::start().await;
    let browser = launch_for_test(Browser::builder()).await;
    let tab = browser.main_tab();
    load(&tab, &server.url(SITE_A, "/plain")).await;
    let body = tab
        .find()
        .css("body")
        .one()
        .await
        .expect("/plain has a body");

    assert_fails_at_once(
        r#"tab.find().css("div[").one()"#,
        "is not a valid selector",
        tab.find().css("div[").timeout(QUERY_TIMEOUT).one(),
    )
    .await;
    // Element scope runs `Runtime.callFunctionOn` rather than
    // `Runtime.evaluate`; `one_or_none` must not turn the error into `None`.
    assert_fails_at_once(
        r#"body.find().css("div[").one_or_none()"#,
        "is not a valid selector",
        body.find().css("div[").timeout(QUERY_TIMEOUT).one_or_none(),
    )
    .await;
    assert_fails_at_once(
        r#"tab.find().xpath("//div[").one()"#,
        "is not a valid XPath expression",
        tab.find().xpath("//div[").timeout(QUERY_TIMEOUT).one(),
    )
    .await;
    assert_fails_at_once(
        r#"tab.find().text_regex_with_flags("(?i)plain", "").one()"#,
        "Invalid group",
        tab.find()
            .text_regex_with_flags("(?i)plain", "")
            .timeout(QUERY_TIMEOUT)
            .one(),
    )
    .await;
    assert_fails_at_once(
        r#"tab.find().tag("h1").attr_regex("class", "(?i)x").one_or_none()"#,
        "Invalid group",
        tab.find()
            .tag("h1")
            .attr_regex("class", "(?i)x")
            .timeout(QUERY_TIMEOUT)
            .one_or_none(),
    )
    .await;
    // A predicate's regex fails the query even when no element reaches it:
    // when no element has the tag, or when an earlier predicate rejects
    // every element.
    assert_fails_at_once(
        r#"tab.find().tag("no-such-tag").attr_regex("class", "(?i)x").one_or_none()"#,
        "Invalid group",
        tab.find()
            .tag("no-such-tag")
            .attr_regex("class", "(?i)x")
            .timeout(QUERY_TIMEOUT)
            .one_or_none(),
    )
    .await;
    assert_fails_at_once(
        r#"tab.find().containing_text("no such text").text_matches("(?i)x").one_or_none()"#,
        "Invalid group",
        tab.find()
            .containing_text("no such text")
            .text_matches("(?i)x")
            .timeout(QUERY_TIMEOUT)
            .one_or_none(),
    )
    .await;
    // `many_or_empty` must not turn the error into an empty `Vec`.
    assert_fails_at_once(
        r#"tab.find_all().text_matches("(?i)plain").many_or_empty()"#,
        "Invalid group",
        tab.find_all()
            .text_matches("(?i)plain")
            .timeout(QUERY_TIMEOUT)
            .many_or_empty(),
    )
    .await;

    drop(body);
    browser.close().await;
}
