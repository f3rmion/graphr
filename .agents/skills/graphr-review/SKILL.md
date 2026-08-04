---
name: graphr-review
description: Review Rust working-tree, commit, or branch changes with the Graphr MCP server while keeping model context bounded. Use for token-efficient correctness and regression reviews that need changed symbols, direct callers, callees, and related tests without scanning the repository.
---

# Review Rust changes

1. Choose the Git base from the review request, without reading source:
   - For working-tree, staged, or unspecified current changes, use `HEAD`.
   - For a named or current commit, use its first parent.
   - For a branch or pull request, use its merge base with the named target branch. Ask for the target when it is missing.
   - Stop and report the limitation when the requested base does not exist, such as a root commit.
2. If this session changed Rust files after Graphr started, call `index` once. Otherwise skip it because Graphr indexes when the server starts. If `index` fails, stop and report the failure.
3. Call Graphr `changes` exactly once with `depth: 1` and `max_nodes: 50`.
4. Review only the returned file manifest, diff, and graph. Report correctness or regression findings only when supported by `file:line` evidence.
5. Do not call `search` or `view`. Except for the fallback below, do not read the full diff, search the repository, or browse unrelated files.
6. If the graph contains an `unmapped PATH:LINES` line, the diff is `[truncated]`, or `changes` reports an `untracked PATH` or skipped unsupported paths, make exactly one batched read-only shell fallback. Read at most ten surrounding lines around named unmapped ranges. For a truncated diff, the same fallback may read `git diff --unified=10 BASE -- PATHS` using only manifest paths. Combine required reads in one shell invocation and cap its total output at 200 lines. If the graph is truncated or the fallback cannot resolve every warning within that cap, report incomplete coverage instead of expanding scope. Never claim coverage is complete while a warning remains unresolved.
7. If Graphr is unavailable or `changes` fails, report that directly instead of silently replacing it with a repository scan.

Before concluding, account for every changed symbol and exact behavioral substitution, direct graph callers and tests, entry-point or registration macros visible in fallback source, production or public-surface impact, deterministic outputs, and test gaps. Distinguish "Graphr showed no direct edge" from "no caller exists."

Return findings from critical to low severity, followed by one line summarizing changed symbols, blast radius, and related tests. If no bug is found, say so explicitly. Put incomplete coverage in that summary. Keep the response under 220 words.
