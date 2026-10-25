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

Graphr exposes nine MCP tools:

- `inspect_root` cheaply authorizes an explicit worktree and returns its Git
  identity and status without indexing.
- `index` queues an asynchronous immutable snapshot build for an explicit
  worktree, range, target, and dependency mode.
- `index_status` reports progress and returns the completed `snapshot_id`.
- `cancel_index` requests cooperative cancellation of a job.
- `search`, `view`, `impact`, `path`, and `changes` query one required
  `snapshot_id`; there is no default snapshot.

## Snapshot workflow

Queue the exact review selection. The `index` call authorizes the selected
root and returns its canonical identity. This example captures committed,
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
impact({"snapshot_id": "<digest>", "node_ref": "<opaque>", "depth": 3, "max_nodes": 30})
path({"snapshot_id": "<digest>", "from_ref": "<opaque>", "to_ref": "<opaque>", "max_hops": 6, "direction": "forward"})
```

`impact` lists what reaches a node in reverse, breadth first: `caller <-`,
`test <-`, `importer <-`, `impl <-`, and `dispatch <-` (a supertype member that
the node overrides). `path` returns the shortest static route between two nodes
as `path hops=N direction=forward|any` followed by one line per hop
(`call ->`, `test-call ->`, `import ->`, `implements ->`, `dispatch ->`, or the
reverse labels when `direction` is `any`), or
`no path within max_hops=N direction=...` when none exists inside the bound.
Ties resolve to the lowest node ids. Both answer from one in-memory adjacency
built on first use per snapshot; on a 911-file C++ snapshot the first call took
24 ms and later calls about 0.3 ms. `[truncated]` marks bounded output.

Review is the default. For one complete bounded DOT change-impact graph, opt in:

```text
changes({
  "snapshot_id": "<digest>",
  "depth": 6,
  "max_nodes": 50,
  "format": "dot"
})
```

DOT mode has no cursor; its bounds apply to the whole visual graph, and
omissions and completeness remain explicit. Save the returned text and render
it outside Graphr, for example:

```bash
dot -Tsvg impact.dot -o impact.svg
```

Graphr neither invokes nor depends on Graphviz. The graph is static
resolved-call evidence, not runtime execution.

Call `changes` once without a cursor. Each response emits one
`review_next_cursor` when any section has remaining content. Split its line on
the first `=`, pass the complete remaining value verbatim with the same
snapshot, depth, and max-nodes, and continue until that cursor is exhausted.
Each continuation packs the next page for every unfinished section in fixed
files, diff, artifacts, graph, evidence order under the 8 KiB review budget.
`max_nodes` changes graph page size, not snapshot coverage. Every initial and
continuation page repeats three independent terminal facts:
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
index generation, base graph image ID, and snapshot ID. A genuine empty review
includes a structured reason such as `identical_commit_oids`; an unqualified
“no changes” is invalid.

Graphr reads Git and worktree state without modifying HEAD, refs, objects,
indexes, tracked files, or untracked files. Cache writes are confined to the
validated common Git directory's `graphr/v6` namespace and isolated by
repository, worktree, resolved revisions, target state, dependency mode, dirty
digest, and format versions. Linked worktrees may reuse immutable Git-object
analysis but keep distinct workspace and snapshot identities. Published
snapshots are immutable. Graphr performs no automatic cache garbage
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

For Rust, method calls on receivers typed `&dyn Trait`, `impl Trait`, or a
generic parameter with exactly one non-marker trait bound resolve to the trait
method. Several candidate traits leave the call unresolved.

For Python, class bases, `self.method()` and `cls.method()` calls inside class
methods resolve to the enclosing class member first, then the same member on
each direct base. Other attribute receivers produce a dispatch gap.

For JavaScript and TypeScript, graph semantics include definitions, ESM and
CommonJS imports, direct re-exports, resolvable calls, conventional tests,
JSX component calls, class `extends`, TypeScript `implements`, and interface
`extends`. Module resolution is limited to relative repository-local
specifiers. There is no package or `tsconfig` resolution and no type checker;
ambiguous module aliases produce no edge.

For C++, graph semantics include type and function definitions, repository-local
quoted includes, inheritance, direct and qualified calls, `this` calls, and
GoogleTest or Catch2/doctest test cases. Graphr does not preprocess source, read
`compile_commands.json`, resolve system headers, instantiate templates, infer
receiver types, or choose between overloads; ambiguous targets produce no edge.

Affected-flow discovery follows `CALLS` edges up to 15 hops. A method also
reaches the same-name members of its type's transitive supertypes (trait
implementations, base classes, `extends`/`implements`), so a call bound to a
trait or base method can reach a changed override. Such hops render as ` ~> `,
the flow line carries `dispatch_hops=N`, and DOT draws them dotted. These are possible source-level call chains, not recorded runtime call stacks. Risk output states that higher is riskier and includes flow, test, security-name, and caller component scores plus a short rationale. The caller component tiers direct production fan-in, including callers of overridden members, at 1, 2–3, 4–15, and 16 or more callers; 16 or more adds `high-fan-in` to the rationale. `test_path_confidence=heuristic` and `test_path_provenance=resolved-static-call-graph` describe bounded static evidence, not runtime test proof; community and churn factors are not used.

Each changed file inside a file-level import cycle adds an `import_cycle files=N route=a -> b -> a` line naming the shortest import cycle through it; at most 16 such lines are emitted (`import_cycles_omitted=N` counts the rest), and the summary's `import_cycles=N` counts the cycles containing a changed file.

Comments whose text starts with `SAFETY:`, `WHY:`, `NOTE:`, or `INVARIANT:`, or that cite `ADR-N`/`RFC-N` (tags `ADR`/`RFC`), are indexed as source notes in every supported language; Python docstrings are not. A changed symbol's notes, and notes on changed lines, render after the root and neighbor lines as `note tag=SAFETY path="src/lib.rs" line=2 text="caller guarantees ptr is valid"`, ordered by path and line; at most 16 are emitted (`notes_omitted=N` counts the rest), and the summary's `notes=N` counts them all. `view` of a type, function, or test lists up to 8 notes inside its span.
The graph section also compares the head graph with a graph image of the base
commit's supported sources, built during indexing and cached under the same
content key as any other graph image, so a later review from the same base
parses no base files (`base_stats.files_parsed=0`). Symbols match by file path,
node kind, and scope-qualified name, ignoring line positions. After the flow
lines, it emits:

```text
api_removed kind=function symbol=old_api path=src/api.rs line=1 head_callers=1
broken_reference source=caller@src/caller.rs:2 target=rust:function:api::old_api
api_signature_changed symbol=compute path=src/lib.rs line=1 before="pub fn compute(a: u32) -> u32" after="pub fn compute(a: u32, b: u32) -> u32"
api_added kind=function symbol=helper path=src/lib.rs line=4
```

`api_removed` uses the base line; `head_callers` counts head references left
missing or ambiguous whose candidate keys name the removed symbol, each listed
as a `broken_reference` at its call site. Signatures are quoted and shortened to
120 bytes. `broken_reference` and `api_added` records stop at 32, followed by
`broken_references_omitted=N` or `api_added_omitted=N`. A renamed or moved
symbol is reported as removed plus added. The `risk` line ends with
`api_removed=N api_signature_changed=N broken_references=N` and
`base_graph=present` or `base_graph=absent:identical_sources` when the base
sources match the head image. Boundary mode leaves `.cargo/vendor` out of the
comparison.

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

## Codex and Claude review skill

The review skill is source-repository content, not part of the Cargo package.
Codex discovers `.agents/skills` in this repository. Install it globally:

```text
$skill-installer Install the graphr-review skill from https://github.com/f3rmion/graphr/tree/main/.agents/skills/graphr-review
```

Claude Code discovers the same skill from `.claude/skills` in a project. This
repository links `.claude/skills/graphr-review` to the canonical skill. For a
personal install from a checkout:

```bash
mkdir -p ~/.claude/skills
cp -R .agents/skills/graphr-review ~/.claude/skills/
```

Both clients need the separately registered Graphr MCP server. The skill
selects the review base, exhausts every bounded `changes` continuation page,
checks snapshot freshness, follows targeted graph remediation, and keeps the
final review under 220 words.

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
