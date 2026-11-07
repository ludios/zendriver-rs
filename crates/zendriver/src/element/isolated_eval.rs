//! [`Element::evaluate`] — JavaScript on the element in zendriver's
//! isolated world.
//!
//! Every element handle comes from a query, a traversal or a refresh, and
//! all of those run in zendriver's isolated world of the element's own
//! frame (see `QueryScope::execution_context_id`). So the handle already
//! lives there, and evaluating is one `Runtime.callFunctionOn` on it.
//! [`Element::evaluate_main`] (the frame's main world) lives in
//! `element/mod.rs`.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::element::Element;
use crate::error::{Result, ZendriverError};

impl Element {
    /// Run JS on this element in zendriver's isolated world of the
    /// element's frame.
    ///
    /// Wraps `js` as `function(el){ return (<js>) }`, with `el` (and `this`)
    /// bound to the element. The returned value is deserialized into `T`.
    ///
    /// One exception to "of the element's frame": an element found in an
    /// iframe whose handle has gone stale is re-found by running its query
    /// again on the top-level page, so the call runs there.
    ///
    /// Use this for stealth-safe reads — the isolated world doesn't share
    /// scope with page scripts, so monkeypatched DOM prototypes don't lie
    /// to you. For cases where you need page globals (e.g. reading a custom
    /// element's JS-only state), use [`Element::evaluate_main`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`ZendriverError::JsException`] when the expression raises;
    /// [`ZendriverError::Serde`] when the result cannot be decoded.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn ex() -> zendriver::Result<()> {
    /// # let browser = zendriver::Browser::builder().launch().await?;
    /// # let tab = browser.main_tab();
    /// let h1 = tab.find().css("h1").one().await?;
    /// let tag: String = h1.evaluate("el.tagName").await?;
    /// assert_eq!(tag, "H1");
    /// # Ok(()) }
    /// ```
    pub async fn evaluate<T: DeserializeOwned>(&self, js: impl AsRef<str>) -> Result<T> {
        let function = format!("function(el){{ return ({}) }}", js.as_ref());
        let function = function.as_str();
        self.with_refresh(|| async move {
            let result = self.call_on_main(function, json!([])).await?;
            let value = result.get("value").cloned().unwrap_or(Value::Null);
            serde_json::from_value(value).map_err(ZendriverError::Serde)
        })
        .await
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tab::Tab;
    use crate::test_support::expect;
    use zendriver_transport::SessionHandle;
    use zendriver_transport::testing::MockConnection;

    /// `evaluate` is one `Runtime.callFunctionOn` on the element's own
    /// handle, which is already in the isolated world: no
    /// `Page.createIsolatedWorld` or `DOM.resolveNode` first.
    #[tokio::test]
    async fn evaluate_calls_the_function_on_the_elements_own_handle() {
        let (mut mock, conn) = MockConnection::pair();
        let sess = SessionHandle::new(conn.clone(), "S1");
        let tab = Tab::new_for_test(sess);
        let el = Element::from_jsret(tab, 314, "R_ISO".to_string());

        let fut = tokio::spawn({
            let e = el.clone();
            async move { e.evaluate::<String>("el.tagName").await }
        });

        let id_call = expect(&mut mock, "Runtime.callFunctionOn").await;
        let sent = mock.last_sent();
        assert_eq!(sent["params"]["objectId"], "R_ISO");
        assert_eq!(
            sent["params"]["arguments"][0]["objectId"], "R_ISO",
            "the element must be bound as `el`",
        );
        let decl = sent["params"]["functionDeclaration"].as_str().unwrap();
        assert!(
            decl.contains("function(el)") && decl.contains("el.tagName"),
            "function declaration must wrap user JS; got: {decl}",
        );
        assert_eq!(sent["params"]["returnByValue"], true);
        assert_eq!(sent["params"]["awaitPromise"], true);
        mock.reply(
            id_call,
            json!({ "result": { "type": "string", "value": "DIV" } }),
        )
        .await;

        assert_eq!(fut.await.unwrap().unwrap(), "DIV");
        conn.shutdown();
    }
}
