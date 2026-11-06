# Waiting for navigation

Three waits cover the ways a tab gets to a page. Which one to use depends
on who starts the navigation.

| The navigation is started by | Wait with |
|---|---|
| zendriver: `goto`, `back`, `forward`, `reload`, `new_tab_at` | [`wait_for_load`], right after the call |
| the page: a click, a key press, a script | [`expect_navigation`], created **before** the action |
| anything, when you know the destination | [`wait_for_url`], called before or after the action |

## `wait_for_load`

`goto` arms a load tracker before it sends `Page.navigate`, so the next
`wait_for_load` waits for that navigation, or for one that began after it.
With no navigation armed, it waits only for the current document to finish
loading. A navigation that a click or an Enter starts leaves the old
document in place until the new one commits, so `wait_for_load` right after
the click may return at once, on the old page.

## `expect_navigation`

Create the expectation before the action, and await it after. It
subscribes to the main frame's events when it is created, so the navigation
cannot slip past, however fast it is:

```rust,ignore
let input = tab.find().css("input[name=q]").one().await?;
input.type_text("zendriver").await?;
let navigation = tab.expect_navigation();
input.press(Key::Special(SpecialKey::Enter)).await?;
let reached = navigation.await?;
println!("the results are at {}", reached.url);
```

It resolves after the next navigation of the main frame has committed and,
if that navigation brought a new document, after that document has loaded.
A navigation that stays in the document (a fragment change, or
`history.pushState`) resolves it at once. The [`Navigation`] it resolves
with carries the URL reached, and `same_document`, which tells the two
cases apart. Any navigation counts, even one to the same URL, such as a
form that posts back to its own page.

## `wait_for_url`

Wait until the tab is at a URL matching a pattern, and the page there has
loaded. A `&str` matches every URL that contains it; a `regex::Regex`
matches by regex, so anchor one for an exact URL:

```rust,ignore
tab.find().css("#login").one().await?.click().await?;
// Through the identity provider and back; a human may be typing.
let home = tab
    .wait_for_url(Regex::new(r"^https://app\.example\.com/$")?)
    .timeout(Duration::from_secs(300))
    .await?;
```

Being at a URL is a state, so this one is safe to call after the action.
If the tab is already there, the wait ends as soon as that page has loaded
(at once, if it has). Pages at other URLs on the way, such as a login's
bounce through another site, don't end it. Mind a pattern that the page you start
from also matches: the wait ends there at once.

## Timeouts and lost events

Both waits give up after 30 s unless `.timeout(duration)` says otherwise.
If the event stream loses events (a lagging subscriber, or a reconnect),
`expect_navigation` fails with `EventStreamIncomplete`, because the
navigation may have happened unseen; `wait_for_url` reads the URL again and
keeps waiting.

## In the MCP server

`browser_wait_for_url` is `wait_for_url` with `url_substr` or `url_regex`,
`timeout_ms`, and `return_snapshot`. `browser_click` and `browser_press`
take `wait_for_navigation: true`, which arms `expect_navigation` before the
action, so the reply (and its snapshot) describes the page the action led
to.

[`wait_for_load`]: https://docs.rs/zendriver/latest/zendriver/struct.Tab.html#method.wait_for_load
[`expect_navigation`]: https://docs.rs/zendriver/latest/zendriver/struct.Tab.html#method.expect_navigation
[`wait_for_url`]: https://docs.rs/zendriver/latest/zendriver/struct.Tab.html#method.wait_for_url
[`Navigation`]: https://docs.rs/zendriver/latest/zendriver/struct.Navigation.html
