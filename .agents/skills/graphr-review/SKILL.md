---
name: graphr-review
description: Use when reviewing Git working-tree, staged, commit, branch, or pull-request changes through a Graphr MCP server.
---

# Review changes with Graphr

## Core rule

Review one explicitly selected canonical worktree through one immutable snapshot. Never substitute another allowed root, the server's checkout, an older snapshot, a live Git diff, or a repository scan.

Use only Graphr's exact MCP tool names: `inspect_root`, `index`, `index_status`, `cancel_index`, `changes`, `search`, and `view`. A client may render its server prefix differently; do not invent aliases such as `index_repository` or rename any field below.

## Select the snapshot

Choose the root, range, and target before reading source:

| Review | `base` | `head` | `target` |
| --- | --- | --- | --- |
| Working tree | `HEAD` | `HEAD` | `{"kind":"worktree","include_untracked":true}` unless untracked files are explicitly excluded |
| Staged | `HEAD` | `HEAD` | `{"kind":"index"}` |
| Commit | first parent | commit | `{"kind":"commit"}` |
| Branch or pull request | merge base with named target branch | review head | `{"kind":"commit"}` |

Ask for a missing target branch. Stop if a required base does not exist, including a root commit.

1. Call `inspect_root({"worktree_root": ROOT})`. Require the returned canonical `worktree_root` to equal the selected root.
2. Call exactly:

   ```text
   index({"worktree_root": ROOT, "base": BASE, "head": HEAD,
          "target": TARGET, "dependency_mode": "boundary"})
   ```

   Use `dependency_mode: "full"` only when dependency internals are explicitly requested. Require the queued job to report the selected root and resolved `base_oid`/`head_oid`; a mismatch is terminal.
3. Poll `index_status({"job_id": JOB_ID})` while the state is `queued`, `capturing`, `selecting_seed`, `indexing`, `resolving_graph`, or `publishing`. Continue until `completed`, then retain `completion.snapshot_id`. On `failed`, `cancelled`, missing, or malformed status, stop and report incomplete coverage.

## Review the immutable snapshot

1. Call `changes({"snapshot_id": SNAPSHOT_ID, "depth": 6, "max_nodes": 50})` exactly once without `cursor`.
2. Exhaust every `files_next_cursor`, `diff_next_cursor`, `artifacts_next_cursor`, and `graph_next_cursor` returned by any page. A cursor line is `name=value`: split only on the first `=` and pass the entire value, including every later `=`, verbatim:

   ```text
   changes({"snapshot_id": SNAPSHOT_ID, "depth": 6,
            "max_nodes": 50, "cursor": TOKEN})
   ```

   Keep the same snapshot, depth, and max-nodes. Never make a second cursorless `changes` call. Increasing `max_nodes` changes page size, not coverage.
3. Review only returned manifest, source diff, artifact, and graph pages. Artifact pages include bounded generic text plus Markdown requirement/link/fence and TSV schema/key/row/duplicate/width semantics; do not re-read captured files. Treat each `flow` as a possible static path, not a runtime stack, and support findings with `file:line` evidence.
4. Use `search` or `view` only when an emitted graph `coverage` line names that exact remediation. Pass the same `snapshot_id`; pass returned `node_ref` values verbatim. `file-mapped` ranges need no remediation. Do not search unrelated names or read the repository.

`analysis_complete` is analyzer-local. Conclude only after all four cursor streams and named remediation terminate, no explicit omission or failure occurred, and `review_complete_when_pages_exhausted=true`. Binary, oversized, unsafe, non-regular, type-changed, unmerged, cursor, and remediation failures make coverage incomplete. Complete artifact coverage does not add indexed source languages: Graphr indexes Rust, Python, JavaScript/JSX, and TypeScript/TSX.

If `inspect_root` reports `snapshot_matches_worktree=false`, the snapshot remains historical and immutable but no longer proves the selected live worktree was reviewed. Explicitly queue a fresh `index` for the same selected root/range/target and restart from its completed snapshot. If divergence repeats, stop; never fall back to a live diff or default checkout.

## Common failures

| Temptation | Required response |
| --- | --- |
| “The main checkout is already warm.” | Inspect and index the selected root; cache warmth never selects a root. |
| “The job is slow, so use an existing snapshot.” | Keep polling the requested job or stop incomplete. |
| “The cursor looks encoded; normalize it.” | Preserve every byte after the first `=`. |
| “The worktree diverged; review the live diff.” | Build a fresh snapshot or stop incomplete. |

Stop rather than conclude on any structured root, job, snapshot, cursor, provenance, or completeness error. No fallback is equivalent coverage.

## Report

Before concluding, account for every changed symbol and behavioral substitution, risk score and affected static flow, bounded graph callers and tests, visible entry-point or registration macros, production or public-surface impact, deterministic outputs, and static test-path heuristics (`test_path_confidence=heuristic`, `test_path_provenance=resolved-static-call-graph`). Distinguish “Graphr showed no edge within six hops” from “no caller exists.”

Return findings from critical to low severity, then one line summarizing changed symbols, risk, affected flows, blast radius, related tests, and any incomplete coverage. If no bug is found, say so explicitly. Keep the response under 220 words.
