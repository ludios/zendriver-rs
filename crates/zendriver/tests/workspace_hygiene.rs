//! Guards against re-introducing the shared-README release-misattribution bug.
//!
//! Seven sibling crates used to point their Cargo `readme` field at the shared
//! workspace-root `README.md`. release-plz scopes each crate's release to
//! commits whose changed files intersect that crate's *published file set*
//! (`cargo package --list`, which includes the `readme` target) — so every commit
//! touching the root README looked like a change to all seven crates. Each crate
//! now has its own short README; this test fails if the shared pointer returns.

// Setup failures in a hygiene test legitimately panic to fail the test loudly.
#![allow(clippy::panic)]

use std::path::Path;

const SIBLING_CRATES: &[&str] = &[
    "zendriver-transport",
    "zendriver-stealth",
    "zendriver-interception",
    "zendriver-fetcher",
    "zendriver-imperva",
    "zendriver-cloudflare",
    "zendriver-datadome",
];

#[test]
fn sibling_crates_have_their_own_readme_not_the_shared_root_one() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for crate_name in SIBLING_CRATES {
        let manifest_path = workspace_root
            .join("crates")
            .join(crate_name)
            .join("Cargo.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("failed to read {manifest_path:?}: {e}"));
        assert!(
            !manifest.contains(r#"readme = "../../README.md""#),
            "{crate_name} points `readme` at the shared workspace README again — \
             this makes release-plz misattribute releases to it. \
             Give it its own README.md instead."
        );
        let own_readme = workspace_root
            .join("crates")
            .join(crate_name)
            .join("README.md");
        assert!(
            own_readme.exists(),
            "{crate_name} is missing its own README.md"
        );
    }
}
