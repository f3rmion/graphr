# Graphr

Fast, compact Rust and Python code-graph views for Codex and Claude over MCP
stdio.

Graphr is inspired by [code-review-graph](https://github.com/tirth8205/code-review-graph)'s approach to focusing AI review context. Thanks to @tirth8205 and its contributors for originating that work.

## Install

```text
cargo install graphr --locked
```

Register the installed binary with either client:

```text
codex mcp add graphr -- graphr serve /absolute/repository
claude mcp add --scope project graphr -- graphr serve /absolute/repository
```

Graphr detects Rust and Python sources automatically and exposes four MCP tools:

- `index` reparses only dirty, untracked, or Git-OID-changed files.
- `search` finds symbols and returns compact `node_ref` values.
- `view` traverses callers, callees, and related tests up to six graph hops.
- `changes` returns a bounded 8 KiB review context with the diff, risk-ranked changed symbols, affected static execution paths, and graph impact.

Affected-flow discovery follows `CALLS` edges up to 15 hops. These are possible source-level call chains, not recorded runtime call stacks. Risk scores use flow, test, security-name, and caller signals; community and churn factors are not used.

The server indexes when it starts. Run `index` after source changes. For reviews, start with `changes`; use `search` and `view` for targeted exploration when the result is truncated or contains an `unmapped PATH:LINES` range.

## Codex review skill

Install the skill globally by entering this prompt in Codex:

```text
$skill-installer Install the graphr-review skill from https://github.com/f3rmion/graphr/tree/main/.agents/skills/graphr-review
```

The skill selects the review base, makes one bounded `changes` call, permits one targeted fallback for explicit coverage gaps, and keeps the final review under 220 words.

## Token benchmark

Isolated reviews of `rust-random/rand` commit `bb1262f7` used Codex CLI 0.146.0 with `gpt-5.6-sol` at medium reasoning and a read-only checkout. Tests were excluded. Plain Codex and unguided Graphr were measured as a pair; the guided mode was measured separately with `$graphr-review` explicitly invoked.

| Mode | Input | Cached input | Uncached input | Output | Total | Rubric coverage |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Plain Codex | 134,780 | 102,912 | 31,868 | 2,305 | 137,085 | 7/10 |
| Unguided Graphr | 294,000 | 248,064 | 45,936 | 3,388 | 297,388 | 7/10 |
| Graphr + `$graphr-review` | 82,244 | 64,256 | 17,988 | 2,725 | 84,969 | 9/10 |

The unguided run read the full diff, made two `changes` calls and 14 `search`/`view` calls, and re-read source. The guided run made one `changes` call, no `search`/`view` calls, and one bounded fallback for two unmapped files. It used 52,116 fewer total tokens than plain Codex (-38.0%) and 212,419 fewer than unguided Graphr (-71.4%); uncached input fell 43.6% versus plain Codex.

Each mode was measured once on one small commit, and the guided mode included additional skill instructions, so the results are directional. Token counts were collected before affected-flow and risk fields; for this fixture, those fields add 206 bytes to the raw response.

## Comparison with code-review-graph (CRG)

The same commit was evaluated against an eight-item, source-verified review checklist. Graphr's raw `changes` response scored 5/8, CRG 2.3.7 `detect_changes` scored 2.5/8, and CRG's larger `get_review_context` scored 3.5/8. Graphr identified both changed paths, all seven changed functions, the RNG substitutions, and the deterministic seed/assertion behavior while excluding an unchanged nested function. Both tools missed public re-export and related-test evidence in bounded context; macro-generated Criterion registration required a targeted source fallback.

A common stdio MCP harness used warm indexes, 20 fresh starts, and 100 measured calls after warmup:

| Metric | Graphr | CRG 2.3.7 | Graphr advantage |
| --- | ---: | ---: | ---: |
| Startup p50 / p95 | 31.539 / 32.114 ms | 521.904 / 537.368 ms | 16.55x / 16.73x faster |
| Warm review call p50 / p95 | 6.704 / 7.872 ms | 11.720 / 12.787 ms | 1.75x / 1.62x faster |
| Review text | 5,662 bytes | 39,783 bytes | 7.03x smaller |
| MCP response | 5,903 bytes | 82,482 bytes | 13.97x smaller |

Across 20 interleaved rebuild runs, bounded parallel parsing measured 61.290 ms p50 and 64.871 ms p95, versus 87.847 ms and 97.689 ms for sequential parsing. No-op indexing measured 23.822 ms versus 23.729 ms p50.

These results cover one pinned Rust change and are not universal accuracy estimates.
