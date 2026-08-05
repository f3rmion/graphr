---
name: graphr-review
description: Review Rust working-tree, commit, or branch changes with the Graphr MCP server while keeping model context bounded. Use for token-efficient correctness and regression reviews that need risk-ranked changed symbols, affected static execution flows, callers, callees, and related tests without scanning the repository.
---

# Review Rust changes

1. Choose the Git base from the review request, without reading source:
   - For working-tree, staged, or unspecified current changes, use `HEAD`.
   - For a named or current commit, use its first parent.
   - For a branch or pull request, use its merge base with the named target branch. Ask for the target when it is missing.
   - Stop and report the limitation when the requested base does not exist, such as a root commit.
2. If this session changed Rust files after Graphr started, call `index` once. Otherwise skip it because Graphr indexes when the server starts. If `index` fails, stop and report the failure.
3. Call Graphr `changes` once without a cursor with `depth: 6` and `max_nodes: 50`. Then exhaust every `files_next_cursor`, `diff_next_cursor`, and `graph_next_cursor` returned by any page by calling `changes` with the same base, depth, and max-nodes arguments plus the exact cursor. The cursors share one immutable snapshot, so do not make another cursorless `changes` call until pagination finishes. Six is the public graph-traversal ceiling; affected-flow discovery separately follows `CALLS` edges up to 15 hops. If a cursor is stale or fails, stop pagination and report incomplete coverage.
4. Review only the returned file manifest, diff, and graph pages. Use `risk overall=` and per-symbol `risk` values to prioritize review, and treat each `flow` as a possible static call path rather than a recorded runtime stack. Report correctness or regression findings only when supported by `file:line` evidence.
5. Do not call `search` or `view`. Except for the fallback below, do not read the full diff, search the repository, or browse unrelated files.
6. Only after exhausting every continuation cursor, if the graph contains an `unmapped PATH:LINES` line or `changes` reports an untracked or unsupported path, make exactly one batched read-only shell fallback using only manifest paths. Read at most ten surrounding lines around each named unmapped range. For tracked unsupported paths, use a bounded zero-context `git --literal-pathspecs diff --unified=0 --no-ext-diff --no-textconv BASE -- PATH...`. Pass each literal path as a separate argument; never evaluate or interpolate manifest paths as shell code. For untracked paths with known text statistics, use a no-follow reader and read only regular files whose canonical paths remain inside the repository. If those checks are unavailable, report incomplete coverage instead of reading the path. Combine all reads in one shell invocation and cap total output at 200 lines. Do not read unknown-stat binary or oversized content, use the fallback to replace a stale or failed cursor, inspect a skipped unsafe path, or compensate for an explicit analysis omission; report incomplete coverage instead. If the fallback cannot resolve every warning within the cap, report incomplete coverage instead of expanding scope. Review coverage is complete only after every cursor is exhausted, `review_complete_when_pages_exhausted=true`, and every fallback warning is resolved.
7. If Graphr is unavailable or `changes` fails, report that directly instead of silently replacing it with a repository scan.

Before concluding, account for every changed symbol and exact behavioral substitution, risk score and affected static flow, bounded graph callers and tests, entry-point or registration macros visible in fallback source, production or public-surface impact, deterministic outputs, and test gaps. Distinguish "Graphr showed no edge within six hops" from "no caller exists."

Return findings from critical to low severity, followed by one line summarizing changed symbols, risk, affected flows, blast radius, and related tests. If no bug is found, say so explicitly. Put incomplete coverage in that summary. Keep the response under 220 words.
