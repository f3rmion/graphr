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

The Rust-only slice exposes three MCP tools: `index`, `search`, and `view`.
`search` returns compact `node_ref` values consumed by `view`.
