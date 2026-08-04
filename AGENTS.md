# Graphr Engineering Guide

## Product boundaries

- Build one Rust binary for Codex and Claude over MCP stdio.
- Support Rust and Python; JavaScript/TypeScript/TSX and Go follow.
- Do not add Java, VS Code/editor code, HTTP, UI, embeddings, plugins, or
  migrations.
- Keep tool output deterministic and compact.

## Engineering principles

- Do not preserve backward compatibility. Remove obsolete paths instead of
  adding compatibility layers, fallbacks, or migrations.
- Choose the simplest implementation that fully meets current requirements.
- Grow in working vertical slices; do not trade a working product for unfinished
  complexity.
- Keep concerns separate, but do not add single-implementation abstractions,
  factories, or speculative configuration.
- Prefer the standard library, SQLite constraints/indexes, and dependencies
  already in `Cargo.toml` before adding code or packages.
- Make long-term architectural choices; do not add stopgaps intended for later
  replacement.
- Preserve trust-boundary validation, rollback safety, and tests that prevent
  data loss.

## Required checks

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```
