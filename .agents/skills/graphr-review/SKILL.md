---
name: graphr-review
description: Use for token-efficient correctness and regression reviews of Git working-tree, commit, or branch changes with the Graphr MCP server when bounded source, artifact, risk, static-flow, caller, callee, and related-test context is needed.
---

# Review changes with Graphr

1. Confirm the server is bound to the requested head, then choose the Git base without reading source. `changes(base=...)` has no separate head: for an unchecked-out `A..B` range, report the binding limitation or use an explicitly supplied server at `B` with `A` as base. Never claim an empty review or fall back to a repository-wide scan.
   - For working-tree, staged, or unspecified current changes, use `HEAD`.
   - For a named or current commit, use its first parent.
   - For a branch or pull request, use its merge base with the named target branch. Ask for the target when it is missing.
   - Stop and report the limitation when the requested base does not exist, such as a root commit.
2. If this session changed Rust or Python source after Graphr started, call `index` once. Otherwise use the startup index. If `index` fails, stop and report the failure.
3. Call Graphr `changes` once without a cursor with `depth: 6` and `max_nodes: 50`. Each continuation token is a standalone `name=value` line: split on the first `=` and pass the complete value after it verbatim. Exhaust every `files_next_cursor`, `diff_next_cursor`, `artifacts_next_cursor`, and `graph_next_cursor` returned by any page by calling `changes` with the same base, depth, max-nodes, and dependency-mode arguments plus that cursor. These pages share one immutable snapshot; `max_nodes` changes graph page size, never snapshot coverage. Do not make another cursorless call. If a cursor is stale or fails, report incomplete coverage.
4. Review only the returned manifest, source diff, artifact, and graph pages. Artifact pages provide generic text diffs plus Markdown requirement/link/fence and TSV schema/key/row/duplicate/width semantics; do not re-read captured Markdown, TSV, or generic text. Use risk values to prioritize, treat each `flow` as a possible static call path rather than a runtime stack, and support findings with `file:line` evidence.
5. Call `search` or `view` only when an emitted graph `coverage` line names that exact targeted remediation. `file-mapped` ranges are already covered. Do not read the full diff, search the repository, or browse unrelated files.
6. `analysis_complete` is analyzer-local. While continuation cursors remain, `review_complete=false` is transient and means keep exhausting them. After all four cursor names are absent and graph remediation is exhausted, terminal coverage is complete only when `review_complete_when_pages_exhausted=true`. Any explicit artifact omission—including binary, oversized, unsafe, non-regular, type-changed, or unmerged—or cursor/remediation failure means incomplete coverage. Never read an unsafe path or binary bytes as source or fallback. Complete artifact coverage does not add indexed source languages; Graphr indexes only Rust and Python.
7. If Graphr is unavailable or `changes` fails, report that directly instead of silently replacing it with a repository scan.

Before concluding, account for every changed symbol and exact behavioral substitution, risk score and affected static flow, bounded graph callers and tests, visible entry-point or registration macros, production or public-surface impact, deterministic outputs, and static test-path heuristics (`test_path_confidence=heuristic`, `test_path_provenance=resolved-static-call-graph`). Distinguish "Graphr showed no edge within six hops" from "no caller exists."

Return findings from critical to low severity, followed by one line summarizing changed symbols, risk, affected flows, blast radius, and related tests. If no bug is found, say so explicitly. Put incomplete coverage in that summary. Keep the response under 220 words.
