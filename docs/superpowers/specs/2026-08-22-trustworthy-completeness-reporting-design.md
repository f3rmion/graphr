# Trustworthy Completeness Reporting Design

**Status:** Draft for written review

**Date:** 2026-08-22

## Context

Graphr currently provides immutable Rust and Python source graphs, exact changed
text coverage, bounded affected-flow analysis, and explicit pagination and
artifact omissions. Its completion fields accurately describe those mechanisms,
but their names are broader than the predicates they enforce.

In particular, native graph completion currently means that changed ranges were
mapped and bounded graph traversal finished. It does not mean that every call,
macro expansion, generated source, dynamic dispatch, or language boundary was
modeled. Some unsupported syntax is never captured as a reference, unresolved
references do not retain whether resolution was missing or ambiguous, and
skipped source inventory is returned only in transient index statistics.

The next architectural direction is an evidence graph for change review. The
first vertical slice must make the existing static graph trustworthy before
adding generated-code provenance, runtime coverage, mutation results, trust
boundaries, or requirement mappings.

## Decision

Add a durable gap ledger beside the existing graph and replace broad completion
claims with a compact completeness vector and claim-specific status.

Every relation-bearing syntax site that Graphr observes must produce exactly one
of:

- a resolved static reference and derived edge;
- an unresolved reference with an exact resolution state; or
- an explicit gap describing why no reference can be modeled.

Source capture and parse failures are gaps too. Query traversal remains a
separate concern. A completed traversal over a partial graph must be reported as
completed traversal over partial evidence, never as a complete graph.

This slice extends the working graph. It does not replace it with a generic
claim/evidence framework.

## Goals

- Make source, parser, resolution, macro, generated-code, dynamic-dispatch, and
  language-boundary omissions explicit.
- Distinguish missing and ambiguous reference resolution.
- Prove that every observed call/import site was classified exactly once.
- Separate changed-content capture, source capture, syntax parsing, site
  classification, static modeling, and bounded traversal.
- State whether affected-caller, affected-flow, and static-test-path claims are
  complete or partial and name their evidence basis.
- Preserve immutable snapshots, deterministic compact output, incremental
  indexing, rollback safety, and existing trust-boundary validation.
- Use the existing `changes` graph page and `view` output; add no MCP tool.
- Add no dependency, trait framework, registry, configuration system, or
  compatibility layer.

## Non-goals

This slice does not:

- ingest generated files or prove generator provenance;
- execute builds, tests, coverage tools, mutation tools, or repository code;
- ingest runtime traces, coverage, or mutation reports;
- add artifact nodes or new evidence edge kinds;
- infer source, validator, materializer, or sink roles;
- compare trust-boundary paths between base and head graphs;
- link requirements or normative citations to symbols and tests;
- add shell, JavaScript, TypeScript, TSX, Go, or other language parsing;
- promise that a static graph can prove runtime behavior;
- assign numeric confidence percentages.

Each deferred evidence producer will later resolve or refine gaps without
changing the meaning of existing static evidence.

## Considered approaches

### Output-only counters

Count existing unresolved references and rename completion fields. This is the
smallest patch, but it cannot report syntax that current queries never capture,
parse-error regions, generated boundaries, or skipped source paths. It would
rename the blind spot instead of removing it.

### Gap ledger beside the current graph

Retain files, nodes, references, and edges. Add resolution state to references
and store explicit gaps for evidence the existing relation model cannot hold.
This reuses the current incremental resolver and SQLite image while making
omissions queryable and snapshot-bound. This is the selected approach.

### Universal evidence-graph rewrite

Replace the schema with generic artifacts, claims, evidence records, and typed
relations immediately. That could eventually represent every requested
capability, but there are no generated, runtime, mutation, or policy producers
yet to validate the abstraction. It would trade a working product for
speculative infrastructure and is deferred.

## Terminology

**Captured source** is a supported-language repository source whose exact bytes
were safely captured for the selected immutable target.

**Syntax site** is a parser-observed construct that can affect relationships:
currently calls, imports, macro invocations, and language-specific generated or
dynamic boundaries.

**Resolved reference** has one unique repository-local target.

**Unresolved reference** has static target keys but resolution is either
`missing` or `ambiguous`.

**Gap** is explicit evidence that Graphr could not capture, parse, classify, or
model part of the program.

**Boundary** is an intentional declared limit, such as collapsed dependency
internals. It is still visible as a confidence limit even when it is allowed by
the selected scope.

**Complete claim** means the inputs, syntax, relations, and traversal relevant
to that exact claim contain no unresolved gap. It never means complete runtime
behavior.

## Source inventory accounting

`TargetInventory` will retain source omissions instead of reducing them to one
undifferentiated `skipped` count. A safe UTF-8 path is retained with an omission
reason. Unsafe path bytes remain an aggregate count and are never echoed.

Initial source reasons are:

- `unsafe-path`;
- `non-regular`;
- `unmerged`;
- `oversized`;
- `invalid-utf8`;
- `missing-during-read`.

Concurrent mutation, inconsistent Git metadata, timeouts, digest mismatch, and
internal capture failures remain fatal and publish no snapshot. They are not
ordinary gaps because Graphr cannot bind trustworthy evidence to the requested
target.

The complete source-gap inventory participates in the graph image key. Exact
cache reuse is therefore impossible when an omission appears, disappears, or
changes reason.

## Syntax-site classification

Rust and Python queries will capture every grammar-level call expression, not
only the statically supported target shapes. Existing resolution logic remains
authoritative for supported forms. Unsupported forms emit a gap owned by their
closest source node.

Rust additionally captures every macro invocation. A macro invocation is a gap
unless Graphr has direct source-level evidence for the relationship. An
`include!` or equivalent expression that statically references `OUT_DIR` is
classified as `generated-output-unobserved`; other unexpanded macros use
`macro-expansion-unavailable`. This records the boundary without guessing what
the expansion emits.

Python attribute calls, subscript calls, calls through computed expressions, and
other unsupported targets are retained as `dynamic-or-unsupported-dispatch`
gaps rather than discarded.

Each parser records outermost Tree-sitter error ranges and uncovered missing
nodes deterministically. A parser returning no tree records one whole-file
parse gap. Match-limit exhaustion and analyzer invariant failure remain fatal.

For each successfully parsed file, indexing checks:

```text
observed_relation_sites = references + site_gaps
```

Failure of that equality aborts publication. A parse gap makes syntax coverage
partial because constructs hidden inside the erroneous region cannot be
counted, even when all observed sites were classified.

## Reference resolution state

`RefInput` and the SQLite `refs` table gain a resolution state:

- `pending`, permitted only inside an unpublished indexing transaction;
- `resolved`, requiring a non-null resolved target;
- `missing`, with no repository-local candidate;
- `ambiguous`, with multiple or conflicting candidates.

Sealing rejects any `pending` reference and any state/target mismatch.
Incremental resolution updates the state whenever candidate nodes or aliases
change. Derived `CALLS`, `TEST_CALLS`, and `IMPORTS` edges continue to exist only
for `resolved` references.

The first slice does not guess whether a missing target is an external package,
generated output, foreign-language symbol, or genuinely absent unless syntax
provides direct evidence for one of those boundaries.

## Declared language and dependency boundaries

The graph summary declares `languages=rust,python`. A changed path with a known
JavaScript, TypeScript, TSX, or Go source suffix retains exact artifact diff
coverage and adds a `language/not-indexed` review gap. Generic text does not
become a language gap merely because it lacks a parser.

A Rust or Python import whose syntax directly identifies an external package is
classified as `boundary/external-dependency` rather than an unexplained missing
target. Boundary-mode Cargo dependency internals retain their existing package
summary and add a counted `boundary/dependency-collapsed` confidence limit.
Imports that cannot be classified from syntax remain `missing`; Graphr does not
consult a package manager or execute dependency discovery.

## Gap ledger

Add one `graph_gaps` table with constrained fields equivalent to:

```text
id
file_id nullable
source_id nullable
path nullable
line_start nullable
line_end nullable
category
reason
target_hint nullable
occurrences
```

Categories are `source`, `parse`, `relation`, `macro`, `generated`, `language`,
and `boundary`. Reasons are a closed Rust enum serialized to stable lowercase
tokens. `target_hint` is bounded, escaped repository-derived text; it is never
treated as a resolved target.

Per-file gaps reference the indexed file and are removed by the existing
incremental file replacement. Global inventory gaps have no `file_id`; they are
replaced wholesale from the complete source snapshot during each index build.
Reused files retain their parser gaps from the seed graph. The store validates
that a gap has either a file, a safe path, or a positive aggregate occurrence
count.

The gap ledger is deterministic by path, line range, category, reason, and
target hint. Duplicate identical gaps are folded into `occurrences`.

Completeness summaries count unresolved references and `graph_gaps` together.
A reference is not duplicated as a ledger row merely to make it appear in the
summary.

## Completeness vector

The graph summary reports these independent dimensions:

- `content_capture`: whether every changed source/artifact has exact diff and
  required artifact-analysis coverage;
- `source_capture`: whether every supported source was safely captured;
- `syntax_parse`: whether every captured source parsed without error regions;
- `site_classification`: whether every observed relation site became a
  reference or explicit gap;
- `static_model`: whether every relation relevant to the claim was resolved and
  no unsupported boundary can hide another path;
- `traversal`: whether bounded graph queries completed without limits or
  omitted roots.

`complete`, `partial`, and `not-applicable` are status words, not scores.
Accounting can be complete while modeling is partial: Graphr may know exactly
which macro or dynamic call it cannot model.

The overall output includes claim records for:

- `affected-callers`;
- `affected-flows`;
- `static-test-paths`.

Each claim names `status=complete|partial` and
`basis=resolved-static-call-graph`. A complete static claim still does not prove
runtime execution.

Initially, any repository-wide source/parse omission or broad-target gap such as
an unexpanded macro conservatively makes affected-caller completeness partial.
Missing or ambiguous references with target keys are relevant when those keys
can name a changed or traversed node. This favors an explicit limitation over a
false completeness claim. Relevance can be narrowed later only with measured
evidence that the conservative rule makes reviews unusably noisy.

## Review output

Remove or narrow fields whose current names imply more than they prove:

- `analysis_complete` becomes `traversal_complete`;
- `coverage status=complete` becomes dimension-specific mapping and traversal
  status;
- `review_complete` is removed;
- `review_complete_when_pages_exhausted` becomes
  `content_complete_when_pages_exhausted`;
- `static_evidence_status=complete|partial|not-applicable` reports the graph
  evidence limit independently of pagination.

No compatibility aliases are retained.

A compact graph preamble resembles:

```text
completeness content_capture=complete source_capture=complete syntax_parse=complete site_classification=complete static_model=partial traversal=complete
gaps total=2 relevant=1 by_reason=ambiguous-target:1,macro-expansion-unavailable:1
claim kind=affected-callers status=partial basis=resolved-static-call-graph
claim kind=affected-flows status=partial basis=resolved-static-call-graph
claim kind=static-test-paths status=partial basis=resolved-static-call-graph
gap category=macro reason=macro-expansion-unavailable path="src/lib.rs" line=12 occurrences=1
```

The existing files, source diff, artifacts, and graph cursors remain. The graph
section includes exact gap records owned by changed roots and emitted affected
flows. Repository-wide gaps that affect a claim but are outside that review
neighborhood are summarized by category and reason rather than flooding the
review page.

`view` includes gaps owned by the displayed node and keeps its existing bounded
omission marker. `search` remains node-oriented in this slice.

Changed-content pagination and static evidence are separate terminal facts. A
client can finish reading every immutable page even when static evidence is
partial, then report the exact gaps. Content omissions remain a review-coverage
failure. Every initial and continuation response repeats
`content_complete_when_pages_exhausted` and `static_evidence_status` so a client
does not infer either value from cursor presence.

## Incremental indexing and cache behavior

The schema, analyzer, review, and cache format versions bump together. Old graph
images and review snapshots are rejected and rebuilt; no migration or fallback
is added.

Incremental behavior remains:

- unchanged parsed files reuse nodes, references, gaps, and edges;
- replaced files delete their owned gaps through SQLite foreign keys;
- source-inventory gaps are replaced from the complete new inventory;
- changes to candidate nodes re-resolve affected references and their exact
  resolution state;
- graph image identity covers source omissions and gap-producing analyzer
  semantics.

A cancelled or failed update leaves the prior published image unchanged.

## Safety and failures

Repository-derived gap values use the existing line-safe escaping and bounded
path/target limits. Unsafe paths remain counts. Graphr never reads generated
outputs outside the authorized worktree merely because source contains an
`OUT_DIR` expression.

Unsupported syntax produces partial evidence. Corrupt metadata, inconsistent
site accounting, impossible resolution state, invalid gap ownership, output
overflow, or snapshot mismatch remains fatal. No partial database image is
published after an internal invariant failure.

## Verification

Implementation follows test-first red/green cycles. Focused tests cover:

- Rust direct calls, unsupported call shapes, macro invocations, generated
  `OUT_DIR` includes, and parse-error ranges;
- Python bare calls, attribute/dynamic calls, and parse-error ranges;
- missing versus ambiguous references through full and incremental resolution;
- safe per-path source omissions and aggregate unsafe-path omissions;
- the exact relation-site accounting invariant;
- foreign-language changed paths reported as graph limitations while retaining
  artifact diff coverage;
- deterministic gap folding, escaping, ordering, and ownership;
- graph-page and `view` rendering with compact gap summaries;
- complete traversal over a partial graph remaining explicitly partial;
- a direct-call-only fixture that can still produce a complete static claim;
- cache invalidation and rollback when gap semantics or inventory changes.

One end-to-end fixture combines a resolved call, ambiguous call, dynamic
dispatch, macro-generated boundary, parse gap, skipped source, and exercising
test. It must prove that pagination can finish while affected-caller and
static-test-path claims remain partial for exact named reasons.

The final verification commands are:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

## Follow-up evidence producers

Once completeness reporting is trustworthy, later slices can close specific
gaps:

1. Generated-artifact provenance adds artifact nodes and hash-bound producer,
   input, output, and source-map evidence. Graphr ingests evidence but does not
   run generators.
2. Coverage, runtime, and mutation ingestion binds observations to exact source
   and binary digests. Coverage proves execution; mutation proves which test
   killed a mutant. Intended-assertion claims require assertion-level evidence.
3. Trust-boundary modeling adds explicit source, validator, materializer, and
   sink roles, then compares path claims across immutable base and head graphs.
4. Requirement mapping promotes existing Markdown requirement/citation records
   into explicit requirement-to-symbol and requirement-to-test evidence.
5. Observed build traces provide execution ordering before a general shell CFG
   is justified. Static shell analysis remains a separate language slice.

Each producer must add evidence provenance, declare its own gaps, and leave a
claim partial when its input is absent, stale, or mismatched.
