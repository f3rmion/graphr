# Graphr

Fast, compact Rust, Python, JavaScript/JSX, TypeScript/TSX, and C++ code-graph
views for Codex and Claude over MCP stdio.

Graphr is inspired by [code-review-graph](https://github.com/tirth8205/code-review-graph)'s approach to focusing AI review context. Thanks to @tirth8205 and its contributors for originating that work.

## Install

```text
cargo install graphr --locked
```

Supported platforms are Linux and macOS. Windows is not supported.

Register the installed binary with either client:

```text
codex mcp add graphr -- graphr serve --allow-root /absolute/repository
claude mcp add --scope project graphr -- graphr serve --allow-root /absolute/repository
```

Repeat `--allow-root PATH` to authorize additional ordinary or linked
worktrees. Authorization is only a boundary: every operation still names the
exact canonical worktree it intends to use. Graphr never selects the server's
current checkout or falls back to another allowed root.

Graphr exposes seven MCP tools:

- `inspect_root` cheaply authorizes an explicit worktree and returns its Git
  identity and status without indexing.
- `index` queues an asynchronous immutable snapshot build for an explicit
  worktree, range, target, and dependency mode.
- `index_status` reports progress and returns the completed `snapshot_id`.
- `cancel_index` requests cooperative cancellation of a job.
- `search`, `view`, and `changes` query one required `snapshot_id`; there is no
  default snapshot.

## Snapshot workflow

Inspect the selected root first:

```text
inspect_root({"worktree_root": "/tmp/project-feature"})
```

Then queue the exact review selection. This example captures committed,
staged, unstaged, and untracked changes:

```text
index({
  "worktree_root": "/tmp/project-feature",
  "base": "main",
  "head": "HEAD",
  "target": {"kind": "worktree", "include_untracked": true},
  "dependency_mode": "boundary"
})
```

The queued result includes the canonical root and the exact resolved
`base_oid` and `head_oid`. Poll until completion and retain the returned
snapshot ID; use cancellation only when the build is no longer wanted:

```text
index_status({"job_id": "job-1"})
cancel_index({"job_id": "job-1"})
```

Query only the completed snapshot:

```text
changes({"snapshot_id": "<digest>", "depth": 6, "max_nodes": 50})
search({"snapshot_id": "<digest>", "query": "AuditRecord", "limit": 10})
view({"snapshot_id": "<digest>", "node_ref": "<opaque>", "depth": 6, "max_nodes": 50})
```

Call `changes` once without a cursor. Every continuation is a standalone
`name=value` line: split on the first `=`, pass the complete remaining value
verbatim with the same snapshot, depth, and max-nodes, and continue until all
`files_next_cursor`, `diff_next_cursor`, `artifacts_next_cursor`, and
`graph_next_cursor` and `evidence_next_cursor` values are exhausted. `max_nodes`
changes graph page size, not snapshot coverage. Every initial and continuation
page repeats three independent terminal facts:
`content_complete_when_pages_exhausted`, `static_evidence_status`, and
`dynamic_evidence_status`.

`inspect_root` may also receive a `snapshot_id`. If it reports
`snapshot_matches_worktree=false`, the old snapshot and its cursors remain
immutable, but a review of the new live state requires an explicit new index
job. Graphr never silently refreshes a snapshot or substitutes a live Git diff.

Targets have exact meanings:

| Target | Captured state |
| --- | --- |
| `{"kind":"commit"}` | `base_oid..head_oid` from immutable Git objects; `head` may be an unchecked-out branch or commit. |
| `{"kind":"index"}` | Committed changes through `head_oid`, then the worktree-specific staged state. |
| `{"kind":"worktree","include_untracked":false}` | Index target plus unstaged tracked files. |
| `{"kind":"worktree","include_untracked":true}` | Index target plus unstaged and untracked files. |

Index and worktree targets require `head_oid` to equal that worktree's current
HEAD. The standalone blocking wrapper uses the same selection model:

```text
graphr index --worktree-root /tmp/project-feature --base main --head HEAD \
  --target worktree --include-untracked --dependency-mode boundary
```

Every snapshot-backed response carries repository/workspace identity, common
and per-worktree Git directories, canonical root, branch, exact refs and OIDs,
target state, selected layers, dirty digest, commit and changed-file counts,
index generation, and snapshot ID. A genuine empty review includes a structured
reason such as `identical_commit_oids`; an unqualified “no changes” is invalid.

Graphr reads Git and worktree state without modifying HEAD, refs, objects,
indexes, tracked files, or untracked files. Cache writes are confined to the
validated common Git directory's `graphr/v6` namespace and isolated by
repository, worktree, resolved revisions, target state, dependency mode, dirty
digest, and format versions. Linked worktrees may reuse immutable Git-object
analysis but keep distinct workspace and snapshot identities. Published
snapshots are immutable. Graphr 0.6.0 performs no automatic cache garbage
collection.

## External execution evidence

First run `index` without evidence and retain its source-only `snapshot_id`.
Run generators, tests, and coverage tools outside Graphr, write their outputs
under the authorized worktree, then create a manifest bound to that source
snapshot. Finally run the same `index` selection with
`"evidence_manifest":"evidence.json"`. Graphr validates and imports the files
into a new immutable evidence-bearing snapshot; it never executes a producer.

The closed v1 manifest is:

```json
{
  "format_version": 1,
  "source_snapshot_id": "<64 lowercase hex>",
  "generated": [
    {
      "input": {
        "path": "proto/message.proto",
        "blake3": "<64 lowercase hex>",
        "line_start": 18,
        "line_end": 18
      },
      "generator": {
        "path": "src/generator.rs",
        "line_start": 70,
        "line_end": 78
      },
      "output": {
        "path": "target/debug/build/example/out/message.rs",
        "blake3": "<64 lowercase hex>",
        "line_start": 42,
        "line_end": 67
      }
    }
  ],
  "coverage": [
    {
      "format": "llvm",
      "path": "target/graphr/strict-roundtrip.json",
      "blake3": "<64 lowercase hex>",
      "run_label": "strict-roundtrip",
      "test_name": "strict_roundtrip"
    }
  ]
}
```

V1 accepts LLVM coverage-export JSON major versions 2 and 3 (`format: "llvm"`)
and Coverage.py JSON with `meta.format=3` (`format: "coverage_py"`). The fixed
bounds are 64 KiB for the manifest, 2 MiB per input or generated artifact,
64 MiB per coverage report, 64 generated mappings, eight coverage reports,
128 MiB of unique evidence bytes total, and 200 line-safe bytes per `run_label`
or `test_name`; paths retain the existing repository-path bound.

Generated Rust enters the static graph only when its verified output has one
unique lexical `include!(concat!(env!("OUT_DIR"), "/file.rs"))` site. The
manifest is a producer attestation: Graphr verifies its source snapshot, paths,
spans, digests, generated syntax, and coverage contents, but not that a declared
process caused an output.

An `observed` result means a positive count in one declared run;
`not-observed` means a mapped executable region had zero count in that run;
`unknown` means the imported evidence cannot answer. Static heuristic test
paths use `basis=resolved-static-call-graph`; they are possible source paths,
not execution observations. Manifest provenance uses
`basis=verified-generated-manifest`, not causal build proof.

This milestone has no process execution, causal build trace, runtime call
ordering, mutation proof, JavaScript runtime coverage, before/after trust query,
or normative citation mapping.

## Review output

Graphr detects Rust, Python, JavaScript/JSX, TypeScript/TSX, and C++ sources
automatically. Rust uses `.rs`, Python uses `.py`, JavaScript and TypeScript use
`.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`, `.mts`, `.cts`, and `.d.ts`, and
C++ uses `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, and `.h`. `changes`
returns bounded 8 KiB review pages with every safe changed path, an aggregate
count of unsafe paths, supported source diffs, bounded non-source text diffs,
Markdown/TSV semantics, explicit artifact omissions, risk-ranked changed
symbols, affected static execution paths, and graph impact. `.cargo/vendor`
changes collapse to deterministic package boundaries by default; select
`dependency_mode="full"` while indexing to inspect dependency internals.

For JavaScript and TypeScript, graph semantics include definitions, ESM and
CommonJS imports, direct re-exports, resolvable calls, conventional tests, and
JSX component calls. Module resolution is limited to relative repository-local
specifiers. There is no package or `tsconfig` resolution and no type checker;
ambiguous module aliases produce no edge.

For C++, graph semantics include type and function definitions, repository-local
quoted includes, inheritance, direct and qualified calls, `this` calls, and
GoogleTest or Catch2/doctest test cases. Graphr does not preprocess source, read
`compile_commands.json`, resolve system headers, instantiate templates, infer
receiver types, or choose between overloads; ambiguous targets produce no edge.

Affected-flow discovery follows `CALLS` edges up to 15 hops. These are possible source-level call chains, not recorded runtime call stacks. Risk output states that higher is riskier and includes flow, test, security-name, and caller component scores plus a short rationale. `test_path_confidence=heuristic` and `test_path_provenance=resolved-static-call-graph` describe bounded static evidence, not runtime test proof; community and churn factors are not used.

Artifact text and semantics belong to the immutable snapshot. Non-symbol source
ranges map to their indexed file node and are reported as `file-mapped`;
targeted `search` or `view` remediation is named for unresolved graph coverage.
Binary, oversized, unsafe, non-regular, type-changed, unmerged, and other
explicit artifact omissions keep `content_complete_when_pages_exhausted=false`.
This is complete artifact coverage for every supported source language.

JavaScript, TypeScript, and C++ use the existing incremental indexing and rename
detection pipeline. Rename detection runs independently within regular source
diffs and within non-source artifact diffs. Renames crossing those streams are
conservatively represented as a deletion plus an addition.

## Codex review skill

Install the skill globally by entering this prompt in Codex:

```text
$skill-installer Install the graphr-review skill from https://github.com/f3rmion/graphr/tree/main/.agents/skills/graphr-review
```

The skill selects the review base, exhausts every bounded `changes` continuation page, follows targeted graph remediation, and keeps the final review under 220 words.

## Token benchmark

Isolated reviews of `rust-random/rand` commit `bb1262f7` used Codex CLI 0.146.0 with `gpt-5.6-sol` at medium reasoning and a read-only checkout. Tests were excluded. Plain Codex and unguided Graphr were measured as a pair; the guided mode was measured separately with `$graphr-review` explicitly invoked.

| Mode | Input | Cached input | Uncached input | Output | Total | Rubric coverage |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Plain Codex | 134,780 | 102,912 | 31,868 | 2,305 | 137,085 | 7/10 |
| Unguided Graphr | 294,000 | 248,064 | 45,936 | 3,388 | 297,388 | 7/10 |
| Graphr + `$graphr-review` | 82,244 | 64,256 | 17,988 | 2,725 | 84,969 | 9/10 |

This benchmark predates complete artifact coverage. In that historical run, the unguided mode read the full diff, made two `changes` calls and 14 `search`/`view` calls, and re-read source; the guided mode made one `changes` call, no `search`/`view` calls, and one bounded fallback for two unmapped files. It used 52,116 fewer total tokens than plain Codex (-38.0%) and 212,419 fewer than unguided Graphr (-71.4%); uncached input fell 43.6% versus plain Codex.

Each mode was measured once on one small commit, and the guided mode included additional skill instructions, so the results are directional. Token counts were collected before affected-flow and risk fields; for this fixture, those fields add 206 bytes to the raw response.

## Comparison with code-review-graph (CRG)

This comparison also predates complete artifact coverage. The same commit was evaluated against an eight-item, source-verified review checklist. Graphr's raw `changes` response scored 5/8, CRG 2.3.7 `detect_changes` scored 2.5/8, and CRG's larger `get_review_context` scored 3.5/8. Graphr identified both changed paths, all seven changed functions, the RNG substitutions, and the deterministic seed/assertion behavior while excluding an unchanged nested function. Both tools missed public re-export and related-test evidence in bounded context; macro-generated Criterion registration required a targeted source fallback in that historical run.

A common stdio MCP harness used warm indexes, 20 fresh starts, and 100 measured calls after warmup:

| Metric | Graphr | CRG 2.3.7 | Graphr advantage |
| --- | ---: | ---: | ---: |
| Startup p50 / p95 | 31.539 / 32.114 ms | 521.904 / 537.368 ms | 16.55x / 16.73x faster |
| Warm review call p50 / p95 | 6.704 / 7.872 ms | 11.720 / 12.787 ms | 1.75x / 1.62x faster |
| Review text | 5,662 bytes | 39,783 bytes | 7.03x smaller |
| MCP response | 5,903 bytes | 82,482 bytes | 13.97x smaller |

Across 20 interleaved rebuild runs, bounded parallel parsing measured 61.290 ms p50 and 64.871 ms p95, versus 87.847 ms and 97.689 ms for sequential parsing. No-op indexing measured 23.822 ms versus 23.729 ms p50.

These results cover one pinned Rust change and are not universal accuracy estimates.
