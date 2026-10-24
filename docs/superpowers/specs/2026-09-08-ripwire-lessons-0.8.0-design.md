# Graphr 0.8.0 Ripwire Lessons Design

## Goal

Improve Graphr's completeness, output honesty, retrieval quality, and language
coverage using lessons validated in Ripwire, while retaining Graphr's smaller
seven-tool MCP surface and conservative evidence model.

This design targets Graphr 0.8.0. It was prepared against Graphr commit
`4b4c192a9ec485eb2cd5a7c79a2e6c6529a57f66` and Ripwire commit
`321ee8394289d6d85e66f8909698d0cd3dfdd54c`.

## Context

Ripwire demonstrates four practices that fit Graphr's product:

- audit real tool output rather than trusting plausible-looking summaries;
- test every member of a supported family when one member changes;
- distinguish recoverable pagination from a deliberate result ceiling; and
- measure retrieval on a frozen known-item corpus before changing ranking.

It also contains a conservative Go parser whose negative cases are useful for
Graphr. In particular, it refuses to turn explicit generic/index-call syntax
into a guessed call edge when the syntax tree does not prove which form was
written.

Graphr already has the stronger trust model: explicit authorized roots,
immutable snapshot identities, deterministic output, no source execution, and
ambiguity represented as missing evidence rather than a guessed edge. Those
properties remain unchanged.

One existing defect shows why family-wide checks matter. `Language::Cpp` is a
supported parser variant, but the review assurance line still lists only Rust,
Python, JavaScript, and TypeScript. Existing tests assert that stale text, so a
passing suite currently preserves an inaccurate capability disclosure.

`search` and `view` have a related output-contract problem. Both append only
`[truncated]` when their byte or row budget is exhausted. The caller cannot
distinguish a byte page from a traversal ceiling and cannot request the
remaining selected records. `changes` already has snapshot-bound,
parameter-bound, checksummed continuations and detailed coverage accounting;
0.8.0 brings the same honesty to the two smaller query tools without adding a
tool.

Finally, `search` uses FTS5 but orders matching rows by insertion order. The
code deliberately gates BM25 on measurement. Ripwire's frozen known-item
evaluation supplies the useful pattern; its ranker and PageRank fusion do not.

## Supersession

This document supersedes the following parts of
`docs/superpowers/specs/2026-08-12-review-coverage-design.md`:

- the `Measurement` design, which proposed a new workspace member and live
  pinned repositories;
- the `Relevance Ordering` design, which selected BM25 before an evaluator
  existed; and
- the projected delivery sequence and 0.8.0 release assignment.

The older document's evidence tiers, inferred edges, weighted impact traversal,
and expanded entry-point work are neither adopted nor implemented by this
release. They remain proposals requiring a fresh design if revisited.

## Product Boundaries

- Continue to build one Rust binary for Codex and Claude over MCP stdio.
- Keep the existing seven MCP tools. Add no discovery, history, ranking, or Go
  specific tool.
- Support Rust, Python, JavaScript/JSX, TypeScript/TSX, C++, and Go.
- Add only the `tree-sitter-go` runtime dependency needed to parse Go.
- Add no Java, HTTP, UI, editor integration, embeddings, plugins, migrations,
  compatibility layers, or process execution.
- Keep repository-controlled output deterministic, bounded, compact,
  line-safe, and tied to one immutable snapshot.
- Preserve canonical-root validation, no-follow reads, regular-file and size
  validation, cancellation, SQLite rollback, and explicit gap reporting.
- Preserve the rule that ambiguous resolution produces no edge.

## Non-goals

- Natural-language or semantic task search.
- Enumerating every search match; `limit` remains an intentional ceiling.
- Removing the `view` traversal ceiling or raising `max_nodes` above 50.
- Co-change analysis, Git-history scoring, PageRank, embeddings, or learned
  ranking.
- Evidence tiers, inferred edges, risk-weight changes, or weighted traversal.
- Go workspaces, nested modules, `replace` directives, vendored-module
  resolution, build-tag evaluation, cgo analysis, generated-code detection, or
  type checking.
- Inferring a receiver type from arbitrary expressions or constructor calls.
- Resolving calls through a Go interface to implicit implementors.
- Copying Ripwire source. The implementation transfers behavior and tests by
  reimplementation. If code is ever copied deliberately, Apache-2.0 notice and
  attribution requirements must be handled explicitly. The Go query should be
  authored against tree-sitter-go and its upstream MIT-licensed grammar.

## Delivery Shape

0.8.0 is one release composed of independently testable vertical slices:

1. canonical language-family coverage;
2. recoverable `search` and `view` byte pagination;
3. a frozen retrieval evaluator and baseline;
4. conservative Go indexing;
5. an evidence-gated ranking decision; and
6. public-contract audit and release metadata.

Each slice leaves the binary working and may be reviewed independently. The
ranking decision occurs only after the evaluator and final Go corpus exist.

## Canonical Language Family

`Language` gains one ordered `ALL` constant containing every supported
language. `as_str` remains the canonical wire spelling. Capability disclosures
are generated by iterating `Language::ALL`; no supported-language string is
maintained separately.

Adding a language must make all exhaustive parser dispatch matches compile and
must pass one family test covering:

- representative extension detection;
- stable serialization and parsing of the language name;
- parser dispatch;
- database storage and reload;
- public capability disclosure; and
- isolation from an identically named symbol in every other language.

This fixes the missing C++ disclosure before Go joins the list. It does not add
a language registry abstraction, plugin interface, or dynamic parser factory.

## Search Continuation Contract

`search` keeps `snapshot_id`, `query`, `kind`, and `limit`, and adds an optional
`cursor` of at most 160 bytes. A continuation repeats the same arguments and
passes the returned token verbatim.

`limit` is the maximum selected result set, between 1 and 20. On the first call,
Graphr selects at most `limit` results under the active deterministic ordering
and checks whether another match exists. Byte pagination then operates only on
that selected set. A cursor retrieves selected records that did not fit the
1536-byte response budget; it does not bypass `limit`.

Every page begins with one machine-readable status line containing:

```text
search shown=N selected_total=N prior=N remaining_selected=N limit=N capped=true|false page_complete=true|false result_complete=true|false
```

- `shown` is the number of records on this page.
- `selected_total` is the number selected under `limit`.
- `prior` and `remaining_selected` refer to that selected set.
- `capped=true` means at least one additional database match exists beyond the
  selected set.
- `page_complete=true` means every selected record has been emitted.
- `result_complete=true` only when the page is complete and the result set was
  not capped.

When selected records remain, the response ends with
`search_next_cursor=<token>`. A zero-match response uses the same status line
followed by `no matches`; it does not use a special unaccounted path.

Node records remain whole. The node reference, kind, and line are never
shortened. If escaped display-only name or path content would prevent one
record from fitting, Graphr shortens those fields on a UTF-8 boundary and marks
the record `display_truncated=true`. This replaces an unrecoverable response
containing only `[truncated]`.

## View Continuation Contract

`view` keeps `snapshot_id`, `node_ref`, `depth`, and `max_nodes`, and adds an
optional `cursor` of at most 160 bytes. A continuation repeats the same
arguments.

The existing deterministic traversal materializes the root, the neighbourhood
admitted by `depth` and `max_nodes`, static gaps, and imported evidence. The
result is then paged at logical record boundaries under the existing 4096-byte
budget. Recalculation is bounded by `max_nodes <= 50`; no cache or persisted
pagination table is added.

Every page reports emitted, prior, remaining, and total record and byte counts,
plus:

- `page_complete`, which describes pagination of the materialized view; and
- `neighborhood_complete`, which describes whether traversal found everything
  permitted by the graph before hitting `max_nodes`.

These facts remain independent. Exhausting `view_next_cursor` cannot turn a
node-capped traversal into a completeness claim. Existing static and dynamic
evidence status remains separate as well.

When rendered records remain, the response ends with
`view_next_cursor=<token>`. Records are not split. Oversized display fields use
the same explicit shortening rule as search. Any non-node record that still
cannot fit alone is replaced by a bounded omission record naming its category
and original byte count, so every valid cursor makes progress. The generic
`[truncated]` marker and its helper are deleted once no caller remains.

## Cursor Integrity and Errors

Search and view use separate versioned token prefixes. Both reuse Graphr's
existing length-prefixed BLAKE3 binding pattern rather than introducing a
general pagination framework.

A search cursor binds:

- tool and token version;
- snapshot ID;
- query, kind, and limit;
- selected-result checksum; and
- next record offset.

A view cursor binds:

- tool and token version;
- snapshot ID;
- node reference, depth, and max nodes;
- materialized-view checksum; and
- next record offset.

Tokens contain hashes rather than repository-controlled query or symbol text.
Offsets use canonical unsigned decimal encoding, must be within the selected
record count, and must identify a record boundary.

Malformed, cross-tool, non-canonical, or checksum-invalid tokens return an
`invalid_parameters` tool error naming the relevant cursor. A token for another
snapshot returns `cursor_snapshot_mismatch`. Repeating different query
parameters returns `cursor_parameters_mismatch`. No parser accepts an older or
foreign token as a fallback.

## Frozen Search Evaluation

The evaluator is test-only Rust code using the standard library, the existing
graph builder, and SQLite. It adds no published binary, workspace member,
runtime mode, or dependency.

A repository-local fixture contains representative source and labelled search
cases for every supported language. It is frozen before candidate ranking SQL
is written. Each case records a query, optional kind, and one uniquely expected
node. Query families are:

- exact symbol name;
- exact qualified name;
- identifier prefix;
- path plus identifier;
- signature terms; and
- same-name cross-language collisions.

Semantic task prompts and synonym matching are excluded because neither BM25
nor the current FTS fields can satisfy them without changing the search
product.

The evaluator reports mean reciprocal rank, recall at 1, and recall at 5. An
error is reported as an error and excluded from aggregates; it never receives a
zero rank that could hide a broken candidate. Cases and expected nodes are
reviewed and committed before the ranking candidates run.

The current row-order query is the recorded baseline. Only these production
candidates may be evaluated:

1. SQLite `bm25(nodes_fts)` with name and qualified name weighted above path
   and signature; and
2. exact case-insensitive name, then exact qualified name, then the same BM25
   score, with node ID as the final tie-break.

No custom ranker, rank fusion, PageRank, stemming layer, query router, or new
index is added.

## Ranking Gate

A candidate may replace row ordering only if the frozen final corpus shows:

- 100% recall at 1 for exact-name cases;
- overall MRR at least 0.10 above baseline;
- overall recall at 1 at least 10 percentage points above baseline; and
- no decrease in overall recall at 5.

The evaluator also generates a deterministic broad-query graph large enough to
exercise the FTS sort. In the same release-mode run, the candidate must have:

- median latency below 25 ms or no more than five times the baseline median;
- p95 latency below 100 ms; and
- an `EXPLAIN QUERY PLAN` containing the FTS virtual-table search and no full
  scan of `nodes`.

The timing gate compares candidates within one process and reports the sample
count, medians, p95 values, and ratio. It is a deliberate release check rather
than a normal debug-test timing assertion.

If no candidate passes every gate, production keeps row ordering, unused
candidate code is removed, and 0.8.0 ships the evaluator and recorded decision.
Passing the gate authorizes only the winning SQL ordering; it does not authorize
neighbour ranking or any other scoring change.

## Conservative Go Support

### Files and package identity

Tracked or captured `.go` files are detected as `Language::Go`, including
`_test.go`. Package identity uses the repository-relative directory plus the
declared package name so packages with the same name in different directories
cannot collide.

Graphr recognizes at most one repository-root `go.mod` and only its literal
`module` directive. The file must come from the same immutable commit, index,
or worktree capture as the Go sources; live filesystem fallback is forbidden.
The module path maps repository-local imports to package directories.

If the root module declaration is absent or malformed, same-directory analysis
still proceeds. Calls requiring module-path resolution remain unresolved and
produce a relation gap. Nested modules, `go.work`, `replace`, vendor selection,
and build-list computation are out of scope.

### Definitions and tests

The Go parser emits:

- one file node;
- package functions;
- named structs, interfaces, and named aliases/types;
- methods owned by their declared receiver type; and
- interface method requirements as members of the interface type.

A top-level function in `_test.go` becomes a test node when its name follows a
Go testing convention: `Test`, `Benchmark`, `Fuzz`, or `Example` followed by a
non-lowercase suffix. Graphr does not type-check the function signature or
execute the test package.

### Calls and imports

The parser records:

- bare calls resolved within the same package;
- package-qualified calls through an explicit or default repository-local
  import alias;
- calls on the enclosing method receiver; and
- calls on parameters and local variables whose named or pointer receiver type
  is written syntactically.

Blank imports, dot imports, external packages, interface dispatch, function
values, embedded-field promotion, and receivers requiring expression inference
produce no guessed edge. In 0.8.0, imports are resolution bindings only and do
not produce `IMPORTS` edges: the existing graph has no package node, and
choosing an arbitrary file from a multi-file package would fabricate evidence.

When a call target contains an index or slice expression, including syntax that
could be an explicit generic instantiation, Graphr emits no call edge because
the supported syntax evidence cannot distinguish every generic call from an
index-then-call form. The site contributes an explicit
`dynamic-or-unsupported-dispatch` relation gap. This deliberately sacrifices a
true generic edge to prevent a false one.

All keys retain the `go:` language prefix. A Go definition and a definition in
another language with the same displayed name cannot resolve to one another.

### Parse and resolution failures

Tree-sitter error ranges become the existing bounded parser gaps while valid
surrounding definitions remain indexed. Failure to create the parser or compile
the checked-in query is an index error because it is a product invariant.
Repository syntax and unsupported resolution forms are graph gaps, not process
errors.

## Cache and Format Versions

Go changes source detection and graph construction, so
`CACHE_FORMAT_VERSION` increases from 10 to 11. The corrected language
assurance text changes cached review output, so `REVIEW_FORMAT_VERSION`
increases from 6 to 7.

`SCHEMA_VERSION` remains 8 because existing textual language fields and graph
tables represent Go without a schema change. The cache remains under
`graphr/v6`; old incompatible manifests follow existing validation and
quarantine behavior. There is no migration or compatibility reader.

## Public Documentation

The release updates:

- `Cargo.toml` and `Cargo.lock` to version 0.8.0 and to include Go metadata;
- `AGENTS.md` so Go is supported rather than merely following;
- `README.md` language lists, extensions, conservative Go semantics, search and
  view continuation examples, result ceilings, and cache format statements;
- MCP tool schemas and server guidance for both new cursors; and
- `.agents/skills/graphr-review/SKILL.md` only where its workflow or supported
  language statement changes.

No benchmark claim is published unless its fixture, metric, baseline, and
candidate result are committed and reproducible.

## Acceptance Tests

### Language family

1. Every `Language::ALL` member round-trips through its stable name and has a
   representative detected extension.
2. Every member reaches parser dispatch and can be stored and reloaded.
3. Public assurance text is derived from the full family and includes C++ and
   Go exactly once.
4. Identically named functions in every language remain distinct and never
   cross-resolve.

### Search and view

5. A byte-limited search emits whole selected records across cursors exactly
   once and in the same order as the unpaged selection.
6. A search with more matches than `limit` reports `capped=true` and cannot use
   its cursor to bypass the limit.
7. Search zero results, an oversized display field, and a final full page have
   accurate accounting and no `[truncated]` marker.
8. A multi-page view emits every materialized root, neighbour, gap, and evidence
   record exactly once.
9. View page completion and neighbourhood completion remain independent when
   `max_nodes` is reached.
10. Repeating any cursor is byte-identical. Tampered, cross-tool,
    wrong-snapshot, stale-node, and parameter-mismatched cursors return the
    specified structured errors.
11. Search and view text, including status and cursor lines, remains within the
    existing 1536-byte and 4096-byte budgets and on UTF-8 boundaries.

### Retrieval

12. The frozen evaluator reproduces the committed row-order baseline.
13. Each candidate produces complete metric and query-plan output; an execution
    error cannot score as a result.
14. The production ordering equals row order unless one candidate passes every
    relevance and performance gate.

### Go

15. Package functions, named types, methods, interface methods, and conventional
    tests have stable nodes and ownership.
16. Same-package, repository-package, method-receiver, parameter-receiver, and
    explicitly typed local-receiver calls resolve to the intended node.
17. External imports, interface dispatch, inferred receivers, blank/dot imports,
    ambiguous package targets, and generic/index-call forms create no false
    edge and report the appropriate gap.
18. A malformed region reports a parse gap while valid surrounding definitions
    remain searchable.
19. Commit, index, and dirty-worktree snapshots use the matching captured root
    `go.mod`, never live or cross-worktree content.
20. Incremental reindexing, rename detection, snapshot reuse, and cache
    invalidation behave for Go as they do for every existing source language.

## Contract Audit and Release Verification

Before setting the crate version, run every public MCP operation against one
fixed temporary repository: `inspect_root`, `index`, `index_status`,
`cancel_index`, `search`, `view`, and `changes`. Inspect the actual compact
output as an unfamiliar caller and convert every discovered defect into a
focused regression assertion. Do not commit a large showcase transcript that
would immediately become a second contract source.

The release gate is:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
cargo package --locked
```

The search evaluator's release-mode relevance, performance, and query-plan gate
also runs and its keep-or-reject decision is recorded. The release is not
complete while the worktree differs unexpectedly, generated package contents
omit required queries or fixtures, or public documentation describes a
different contract from the MCP schemas.
