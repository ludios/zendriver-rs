//! ARIA role enum, for the accessibility-aware queries
//! [`crate::query::FindBuilder::role`] /
//! [`crate::query::FindBuilder::role_named`].

/// An ARIA role for use with [`crate::query::FindBuilder::role`].
///
/// Covers the common WAI-ARIA roles. For roles not enumerated here, use
/// [`AriaRole::Other`] with a `&'static str` of the role name.
///
/// # Examples
///
/// ```
/// use zendriver::AriaRole;
/// assert_eq!(AriaRole::Button.to_css(), r#"[role="button"]"#);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AriaRole {
    /// `role="button"`.
    Button,
    /// `role="link"`.
    Link,
    /// `role="textbox"`.
    Textbox,
    /// `role="combobox"`.
    Combobox,
    /// `role="checkbox"`.
    Checkbox,
    /// `role="radio"`.
    Radio,
    /// `role="tab"`.
    Tab,
    /// `role="menu"`.
    Menu,
    /// `role="menuitem"`.
    Menuitem,
    /// `role="dialog"`.
    Dialog,
    /// `role="heading"`.
    Heading,
    /// `role="banner"`.
    Banner,
    /// `role="navigation"`.
    Navigation,
    /// `role="main"`.
    Main,
    /// `role="article"`.
    Article,
    /// `role="list"`.
    List,
    /// `role="listitem"`.
    Listitem,
    /// `role="row"`.
    Row,
    /// `role="cell"`.
    Cell,
    /// `role="columnheader"`.
    Columnheader,
    /// `role="rowheader"`.
    Rowheader,
    /// Escape hatch for ARIA roles not enumerated above.
    Other(&'static str),
}

impl AriaRole {
    /// The role's ARIA name, as in a `role` attribute.
    pub(crate) fn aria_name(self) -> &'static str {
        match self {
            AriaRole::Button => "button",
            AriaRole::Link => "link",
            AriaRole::Textbox => "textbox",
            AriaRole::Combobox => "combobox",
            AriaRole::Checkbox => "checkbox",
            AriaRole::Radio => "radio",
            AriaRole::Tab => "tab",
            AriaRole::Menu => "menu",
            AriaRole::Menuitem => "menuitem",
            AriaRole::Dialog => "dialog",
            AriaRole::Heading => "heading",
            AriaRole::Banner => "banner",
            AriaRole::Navigation => "navigation",
            AriaRole::Main => "main",
            AriaRole::Article => "article",
            AriaRole::List => "list",
            AriaRole::Listitem => "listitem",
            AriaRole::Row => "row",
            AriaRole::Cell => "cell",
            AriaRole::Columnheader => "columnheader",
            AriaRole::Rowheader => "rowheader",
            AriaRole::Other(s) => s,
        }
    }

    /// Compile to a CSS attribute selector, which matches only an explicit
    /// `role` attribute: `<button>` doesn't match `[role="button"]`.
    /// [`crate::query::FindBuilder::role`] doesn't use it, and also matches
    /// the roles that elements have implicitly.
    ///
    /// # Examples
    ///
    /// ```
    /// use zendriver::AriaRole;
    /// assert_eq!(AriaRole::Button.to_css(), r#"[role="button"]"#);
    /// assert_eq!(AriaRole::Other("tooltip").to_css(), r#"[role="tooltip"]"#);
    /// ```
    #[must_use]
    pub fn to_css(self) -> String {
        format!("[role=\"{}\"]", self.aria_name())
    }

    /// The role's name in Chrome's accessibility tree, which
    /// `Accessibility.queryAXTree` matches. It is the ARIA name, except
    /// that Chrome calls `img` by its ARIA 1.3 synonym, `image`.
    pub(crate) fn ax_name(self) -> &'static str {
        match self.aria_name() {
            "img" => "image",
            name => name,
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn button_compiles_to_attribute_selector() {
        assert_eq!(AriaRole::Button.to_css(), r#"[role="button"]"#);
    }

    #[test]
    fn other_role_uses_passed_name() {
        assert_eq!(AriaRole::Other("tooltip").to_css(), r#"[role="tooltip"]"#);
    }

    #[test]
    fn all_roles_compile_snapshot() {
        let all = [
            AriaRole::Button,
            AriaRole::Link,
            AriaRole::Textbox,
            AriaRole::Combobox,
            AriaRole::Checkbox,
            AriaRole::Radio,
            AriaRole::Tab,
            AriaRole::Menu,
            AriaRole::Menuitem,
            AriaRole::Dialog,
            AriaRole::Heading,
            AriaRole::Banner,
            AriaRole::Navigation,
            AriaRole::Main,
            AriaRole::Article,
            AriaRole::List,
            AriaRole::Listitem,
            AriaRole::Row,
            AriaRole::Cell,
            AriaRole::Columnheader,
            AriaRole::Rowheader,
        ];
        let css: Vec<String> = all.iter().map(|r| r.to_css()).collect();
        insta::assert_yaml_snapshot!("aria_role_css_compilation", css);
    }
}
