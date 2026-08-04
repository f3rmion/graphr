# Graphr

Fast, compact Rust and Python code-graph views for Codex and Claude over MCP
stdio.

Graphr is an independent Rust implementation inspired by [code-review-graph](https://github.com/tirth8205/code-review-graph) and its idea of using code graphs to focus AI review context. Credit and thanks go to @tirth8205 and the code-review-graph contributors for the nice project.

```text
cargo build --locked --release
target/release/graphr index /absolute/repository --rebuild
```

Register the same binary with either client:

```text
codex mcp add graphr -- /absolute/path/to/graphr serve /absolute/repository
claude mcp add --scope project graphr -- /absolute/path/to/graphr serve /absolute/repository
```

The binary detects Rust and Python sources automatically and exposes four MCP tools: `index`, `search`, `view`, and `changes`. `index` hashes only dirty, untracked, or Git-OID-changed files and reparses only changed sources. `search` returns compact `node_ref` values consumed by `view`. For reviews, `changes` returns one bounded 8 KiB response containing the compact diff, changed symbols, and their graph impact. After editing, call `index` once and then `changes`; do not fan out through `search` and `view` unless the result is truncated or reports an `unmapped PATH:LINES` range.

## Review skill

Codex discovers the repo-local `$graphr-review` skill in `.agents/skills/graphr-review`. Install it once for use from any Rust repository:

```text
ln -s "$PWD/.agents/skills/graphr-review" "$HOME/.agents/skills/graphr-review"
```

The skill chooses the review base, makes exactly one `changes` call, prohibits repository-wide fallback scans, and keeps the final review under 220 words.

## Token benchmark

Isolated reviews of `rust-random/rand` commit `bb1262f7` used Codex CLI 0.146.0 with `gpt-5.6-sol` at medium reasoning and a read-only checkout. No tests were run. The first two runs were the original paired experiment; the third forward-tested the fixed `changes` response and invoked `$graphr-review` explicitly.

| Mode | Input | Cached input | Uncached input | Output | Total | Rubric coverage |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Plain Codex | 134,780 | 102,912 | 31,868 | 2,305 | 137,085 | 7/10 |
| Unguided Graphr | 294,000 | 248,064 | 45,936 | 3,388 | 297,388 | 7/10 |
| Graphr + `$graphr-review` | 82,244 | 64,256 | 17,988 | 2,725 | 84,969 | 9/10 |

The old workflow read the full diff, made two `changes` attempts plus 14 search/view calls, and re-read source. The fixed run made one `changes` call, zero search/view calls, and one bounded fallback for two named unmapped files. It used 52,116 fewer total tokens than plain Codex (-38.0%) and 212,419 fewer than unguided Graphr (-71.4%); uncached input fell 43.6% versus plain Codex.

These are single stochastic trials on one small commit, and the guided prompt differs by invoking the skill, so the numbers are a directional workflow result rather than a universal performance claim.

## Comparison with code-review-graph (CRG)

The same commit was checked against a source-verified eight-item review oracle. Graphr's raw `changes` response scored 5/8, CRG 2.3.7 `detect_changes` scored 2.5/8, and CRG's larger `get_review_context` scored 3.5/8. Graphr found both paths, exactly the seven changed functions, the old-to-new RNG substitutions, and the deterministic seed/assertion behavior without claiming an unchanged nested function was modified. Both tools still missed public reexport and related-test evidence in their bounded review context; Graphr also delegated the Criterion registration macro to the skill's line-bounded fallback.

A common stdio MCP harness used warm indexes, 20 fresh starts, and 100 measured calls after warmup:

| Metric | Graphr | CRG 2.3.7 | Graphr advantage |
| --- | ---: | ---: | ---: |
| Startup p50 / p95 | 31.835 / 32.266 ms | 521.904 / 537.368 ms | 16.39x / 16.65x faster |
| Warm review call p50 / p95 | 6.148 / 6.867 ms | 11.720 / 12.787 ms | 1.91x / 1.86x faster |
| Review text | 5,456 bytes | 39,783 bytes | 7.29x smaller |
| MCP response | 5,695 bytes | 82,482 bytes | 14.48x smaller |

In a separate 20-run interleaved rebuild benchmark on the same checkout, bounded file-parsing workers reduced Graphr's p50 from 87.847 to 61.290 ms (-30.2%) and p95 from 97.689 to 64.871 ms (-33.6%). No-op indexing stayed effectively flat at 23.729 versus 23.822 ms p50.

This is a parity gate on one pinned Rust change, not a claim of universal accuracy. Add more source-verified commits before generalizing the result.
