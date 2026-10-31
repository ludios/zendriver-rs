<!-- clank-right
languages = ["rust"]
-->

# Project overview

Fork of [TurtIeSocks/zendriver-rs](https://github.com/TurtIeSocks/zendriver-rs): async Chrome automation over the DevTools Protocol, plus stealth / WAF-bypass crates and an MCP server. Upstream's CLAUDE.md has the workspace layout and what upstream expects of a public API change (MCP coverage ledger, schema snapshots, docs sync); read it.

# Before committing

At the workspace root:

	cargo fmt --all
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo test --workspace --lib --locked
	cargo test --workspace --doc --locked

If you touched anything Chrome drives, also run the integration tests against real Chrome: `cargo test --workspace --features integration-tests --locked`.
