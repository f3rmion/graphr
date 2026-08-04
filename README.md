# Grapher

Fast, compact Rust code-graph views for Codex and Claude over MCP stdio.

```text
cargo build --locked --release
target/release/grapher index /absolute/repository --rebuild
```

Register the same binary with either client:

```text
codex mcp add grapher -- /absolute/path/to/grapher serve /absolute/repository
claude mcp add --scope project grapher -- /absolute/path/to/grapher serve /absolute/repository
```

The Rust-only slice exposes four MCP tools: `index`, `search`, `view`, and
`changes`.
`index` hashes only dirty, untracked, or Git-OID-changed files and reparses only
changed Rust sources. `search` returns compact `node_ref` values consumed by
`view`. After editing, call `index` and then `changes` for a compact graph of
changed symbols relative to `HEAD` or another base commit.
