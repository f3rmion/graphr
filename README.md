# Graphr

Fast, compact Rust and Python code-graph views for Codex and Claude over MCP
stdio.

```text
cargo build --locked --release
target/release/graphr index /absolute/repository --rebuild
```

Register the same binary with either client:

```text
codex mcp add graphr -- /absolute/path/to/graphr serve /absolute/repository
claude mcp add --scope project graphr -- /absolute/path/to/graphr serve /absolute/repository
```

The binary detects Rust and Python sources automatically and exposes four MCP
tools: `index`, `search`, `view`, and `changes`.
`index` hashes only dirty, untracked, or Git-OID-changed files and reparses only
changed sources. `search` returns compact `node_ref` values consumed by `view`.
After editing, call `index` and then `changes` for a compact graph of changed
symbols relative to `HEAD` or another base commit.
