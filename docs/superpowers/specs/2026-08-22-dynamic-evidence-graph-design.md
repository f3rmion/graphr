# Dynamic Evidence Graph Design

**Status:** Draft for written review

**Date:** 2026-08-22

## Context

Graphr currently provides immutable Rust and Python source graphs, exact changed
text coverage, bounded affected-flow analysis, and explicit pagination and
artifact omissions. Its completion fields describe those mechanisms, but their
names are broader than the predicates they enforce.

In particular, native graph completion currently means that changed ranges were
mapped and bounded graph traversal finished. It does not mean that every call,
macro expansion, generated source, dynamic dispatch, or language boundary was
modeled. Some unsupported syntax is never captured as a reference, unresolved
references do not retain whether resolution was missing or ambiguous, and
skipped source inventory is returned only in transient index statistics.

The product direction is a dynamic evidence graph for change review, not a more
confident static call-graph viewer. Static structure must connect to generated
artifacts and observations from real executions. Completeness reporting is the
assurance layer that makes those paths reviewable; it is not the product
endpoint.

The first milestone must therefore do both: make the current static model's gaps
explicit and prove one useful dynamic path from source provenance through
generated Rust to execution by a named test. The milestone is not complete if
it only reports that such evidence is missing.

## Decision

Evolve the existing graph in three connected layers:

- **structure**: source files, symbols, calls, schemas, generated files, and
  exact provenance links;
- **observations**: imported, snapshot-bound coverage from real test runs;
- **assurance**: durable gaps, completeness dimensions, and claim-specific
  limits.

Add a durable gap ledger beside the current graph, import an explicit evidence
manifest through the existing indexing workflow, index verified generated Rust
with the existing parser, and map standard Rust and Python coverage reports onto
exact source regions and static symbols.

Every relation-bearing syntax site that Graphr observes must produce exactly one
of:

- a resolved static reference and derived edge;
- a verified syntax-backed provenance link;
- an unresolved reference with an exact resolution state; or
- an explicit gap describing why no relationship can be modeled.

Source capture and parse failures are gaps too. Query traversal remains a
separate concern. A completed traversal over a partial graph must be reported as
completed traversal over partial evidence, never as a complete graph. An
unobserved static edge is not reported as unreachable, and aggregate coverage
is not attributed to a particular test.

The milestone extends the working graph and indexing transaction. It does not
replace them with a generic claim/evidence framework or execute repository code.

## Goals

- Make source, parser, resolution, macro, generated-code, dynamic-dispatch, and
  language-boundary omissions explicit.
- Distinguish missing and ambiguous reference resolution.
- Prove that every observed call, import, and macro site was classified exactly
  once.
- Separate changed-content capture, source capture, syntax parsing, site
  classification, static modeling, and bounded traversal.
- State whether affected-caller, affected-flow, and static-test-path claims are
  complete or partial and name their evidence basis.
- Import one bounded evidence manifest tied to an existing immutable source
  snapshot without adding a new MCP tool.
- Capture hash-verified generated Rust outputs and link their input spans,
  generator branch, generated spans, and `OUT_DIR` inclusion site.
- Ingest LLVM coverage JSON and Coverage.py JSON produced outside Graphr.
- Distinguish run-level execution from exact named-test execution.
- Show whether changed and generated regions were observed, not observed in the
  declared run, or remain unknown.
- Make the first end-to-end acceptance path:
  `schema annotation -> generator branch -> OUT_DIR output -> generated
  encode/decode -> predicate -> named test execution`.
- Preserve immutable snapshots, deterministic compact output, incremental
  indexing, rollback safety, and existing trust-boundary validation.
- Use `index`, `changes`, and `view`; add an evidence continuation cursor but no
  MCP tool.
- Add no dependency, trait framework, registry, configuration system, or
  compatibility layer.

## Non-goals

This milestone does not:

- execute builds, generators, tests, coverage tools, mutation tools, or other
  repository code;
- infer generated provenance without an explicit, hash-verified manifest;
- ingest runtime call stacks, build traces, or mutation reports;
- claim that coverage establishes caller/callee ordering;
- add a universal artifact, observation, or plugin protocol;
- infer source, validator, materializer, or sink roles;
- compare trust-boundary paths between base and head graphs;
- link requirements or normative citations to symbols and tests;
- add shell, JavaScript, TypeScript, TSX, Go, or other language parsing;
- treat code not observed in one run as unreachable or untested by every run;
- assign numeric confidence percentages.

Later evidence producers will add their own exact records and gaps without
changing the meaning of static edges or coverage observations.

## Considered approaches

### Output-only counters

Count existing unresolved references and rename completion fields. This is the
smallest patch, but it cannot report syntax that current queries never capture,
parse-error regions, generated boundaries, or skipped source paths. It would
rename the blind spot instead of removing it.

### Trust-only gap ledger

Retain files, nodes, references, and edges. Add resolution state to references
and store explicit gaps for evidence the existing relation model cannot hold.
This reuses the current incremental resolver and SQLite image while making
omissions queryable and snapshot-bound. It is necessary, but it would leave
Graphr no more capable of answering whether generated code or a changed branch
actually ran. It is selected as the assurance layer, not as the whole milestone.

### Layered extension of the current graph

Keep the current static schema and add only the records needed by the first
end-to-end path: gaps, imported artifacts, provenance links, coverage runs, and
coverage regions. This delivers a real dynamic query while keeping existing
parsers, resolution, traversal, snapshots, and review output. This is the
selected approach.

### Universal evidence-graph rewrite

Replace the schema with generic artifacts, claims, evidence records, and typed
relations immediately. That could eventually represent every requested
capability, but one generated-manifest format and two coverage formats do not
justify the abstraction. It would trade a working product for speculative
infrastructure and is deferred.

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

**Evidence manifest** is a bounded JSON document supplied explicitly to
`index`. It binds generated outputs and coverage reports to a previously
published source snapshot using exact content digests.

**Provenance link** connects exact source spans through a repository-local
generator span to a hash-verified generated output span.

**Coverage run** is one imported coverage report and its declared scope. It is
an observation of that run, not a universal statement about possible behavior.

**Observed region** has a positive execution count in an imported report.
**Not observed** means a mapped executable region has zero count in that exact
run. **Unknown** means no trustworthy observation can answer the question.

**Complete claim** means the inputs, syntax, relations, observations, and
traversal relevant to that exact claim contain no unresolved gap. Static claim
completeness never proves runtime behavior; a dynamic claim proves only the
named observation.

## Layered graph model

The existing files, symbols, references, and derived edges remain the
structural graph. Generated Rust outputs enter that same graph after their bytes
and declared digests are verified, so existing parsing and resolution can
connect hand-written callers, generated encode/decode methods, and shared
predicates without a second call-graph implementation.

Provenance and coverage remain typed evidence records rather than being forced
into ordinary `CALLS` edges:

```text
static:       source symbol --CALLS--> generated symbol --CALLS--> predicate
provenance:   input span --GENERATED_BY--> generator span --PRODUCES--> output span
inclusion:    OUT_DIR include site --INCLUDES--> generated file
observation:  named test/run --EXECUTED--> generated/output region
assurance:    claim --LIMITED_BY--> exact gap
```

These are logical review paths. SQLite may store provenance and observations in
dedicated constrained tables because coverage counts and run identity do not
belong on globally deduplicated static edges.

`changes` joins the layers for changed roots. `view` shows evidence owned by the
displayed node or file. Static and observed relationships use different output
tokens and are never silently merged.

## Evidence indexing workflow

The existing `index` request gains one optional `evidence_manifest` path. No
separate ingestion tool or mutable attachment is added.

The workflow is:

1. Index the selected source state and retain its immutable `snapshot_id`.
2. Run the repository's generator and tests outside Graphr.
3. Produce the evidence manifest and standard coverage reports under the
   authorized worktree.
4. Call `index` again with the same source selection and the manifest path.
5. Graphr verifies that the selected source still matches the manifest's source
   snapshot, captures every declared evidence file, and publishes a new
   immutable evidence-bearing snapshot.

The second index reuses unchanged static files through the current seed-image
path. Its snapshot and graph image identity include the source snapshot ID,
manifest digest, every imported artifact digest, and evidence semantics version.
Changing any of them creates a different immutable snapshot.

The manifest, generated outputs, and coverage reports are evidence-only files.
They must be untracked or ignored and are excluded from the second call's
source-state comparison only after the manifest names them. This permits a test
run to create `target` or coverage output without making its own source snapshot
stale. A tracked evidence output, or any change to a non-evidence source path,
is a fatal source-snapshot mismatch.

The manifest and every referenced file must use a safe relative path beneath
the authorized worktree, be a bounded regular file, and remain unchanged during
capture. Symlinks, unsafe names, missing files, digest mismatches, source
snapshot mismatches, duplicate run identities, invalid spans, and unsupported
manifest versions are fatal. Explicitly requested evidence is never quietly
downgraded to a gap.

Reuse current bounds: 64 KiB for the manifest, 2 MiB for each generated or
source artifact, and 64 MiB for each coverage report. Larger real reports can
justify streaming or a raised measured bound later; v1 does neither.

The v1 manifest contains only fields needed by this milestone:

```text
format_version
source_snapshot_id
generated[]: input path/digest/span, generator path/span,
             output path/digest/span
coverage[]: format, path/digest, run_label, optional test_name
```

Repeated entries are sorted and deduplicated. Labels and test names are bounded
line-safe text. The manifest is evidence input, not repository configuration;
there is no discovery, search path, environment expansion, or format registry.
Digests use the existing BLAKE3 content-hash representation.

The manifest is a producer attestation. Graphr proves that its paths, spans,
digests, source snapshot, generated syntax, and coverage contents agree; it does
not independently prove that the declared process caused the output. Review
output therefore names `verified-generated-manifest` as the provenance basis.
An observed build trace can add causal process evidence later.

## Generated-artifact provenance

Each `generated` record must name one exact input span, generator span, and
output span. Graphr verifies input bytes against the selected source snapshot,
maps the generator span to one Rust or Python node, captures the generated Rust
file, and verifies its digest before parsing it with the existing Rust parser.

The initial supported chain is repository-local Rust/Python generator source to
generated Rust output. `.proto`, descriptor, TSV, fixture, and other inputs are
retained as typed artifact spans without adding language parsers. Their declared
bytes and hashes are still exact evidence. Generated Python provenance waits for
a concrete producer; Python execution evidence does not.

Rust `include!` expressions with a statically recoverable `OUT_DIR` filename are
matched to one imported output. A unique match creates an `INCLUDES` provenance
link. No match remains `generated-output-unobserved`; multiple matches become an
ambiguous generated-output gap. Other macro expansions remain explicit gaps.

A generated Rust file enters the symbol graph only when it has one unique
inclusion site. It inherits that site's module and owner context before the
existing parser and resolver run. Zero or multiple inclusion sites retain the
artifact and provenance records but produce a generated-inclusion gap rather
than context-free symbols. Multiple semantic inclusion contexts can be added
when a real repository requires them.

Only verified generated source participates in static parsing and call
resolution. A manifest link proves the declared input/generator/output
provenance; the parsed generated call graph independently proves whether both
encode and decode call the intended predicate. Graphr does not infer either
fact from matching names or nearby text.

## Coverage observations

The first importers accept:

- LLVM `llvm-cov export` JSON with region data for Rust;
- Coverage.py JSON for Python, including contexts when the report contains
  them.

The importers follow the documented
[LLVM JSON export](https://llvm.org/docs/CommandGuide/llvm-cov.html#export-command)
and [Coverage.py JSON context](https://coverage.readthedocs.io/en/latest/commands/cmd_json.html)
formats. Unsupported major versions are rejected rather than guessed.

Graphr parses reports directly with existing JSON support. It does not add a
coverage library or invoke either tool. Summary-only reports cannot establish
region execution and are rejected as invalid evidence for this milestone.

Coverage paths map only to captured hand-written or verified generated files.
Every executable region and reported branch outcome becomes a run-scoped
observation with its exact range and count. Overlap with a symbol or changed
span is computed from source ranges; coverage function names are hints, not
resolution authority.

Report-internal absolute filenames are accepted only when lexical
normalization places them beneath the authorized worktree, then stored as safe
relative paths. Other absolute or unsafe filenames become aggregate external
coverage boundaries and are never echoed or opened. Graphr reads source bytes
from its captured snapshot, never from a path named only by a report.

An LLVM report normally identifies a run, not individual tests. It gains exact
test attribution only when its manifest record supplies one `test_name` and
that name resolves uniquely to a static test node. Aggregate reports remain
run-level. Coverage.py contexts may produce per-test observations when a context
resolves uniquely; empty, missing, or ambiguous contexts stay run-level or
become explicit mapping gaps as applicable.

A positive count proves that a region executed during that run. A zero count
proves only that the executable region was not observed in that run. Line or
region coverage does not create dynamic caller/callee edges and does not prove
which assertion defended the behavior.

Exact named-test observations are rendered before heuristic static test paths.
Existing `TEST_CALLS` evidence remains a possible source-level path with its
current static basis; it is never relabeled as runtime coverage or deleted
merely because one imported run did not observe it.

LLVM branch counts may be attributed to the manifest's single named test.
Coverage.py contexts establish per-test line execution, but a report that does
not associate branch arcs with contexts keeps Python branch evidence at run
scope. Graphr reports that limit instead of copying a line context onto a branch.

## First milestone acceptance path

The milestone ships only when one end-to-end Rust fixture demonstrates:

```text
.proto annotation
  -> manifest input span
  -> Rust generator branch
  -> verified generated OUT_DIR Rust file
  -> generated encode method -> shared predicate
  -> generated decode method -> shared predicate
  -> positive coverage for both call sites and the relevant predicate branch
  -> one uniquely resolved named test
```

The review must show both encode and decode paths. Removing either generated
predicate call, changing an input/output digest, using aggregate-only coverage,
or omitting one required observation must change the exact claim result or
publish no snapshot. This is the capability proof that prevents completeness
work from shipping as the endpoint.

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
linked only when one imported generated output matches; otherwise it is
classified as `generated-output-unobserved` or
`generated-output-ambiguous`. Other unexpanded macros use
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
observed_relation_sites = references + syntax_backed_provenance_links + site_gaps
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

The milestone does not guess whether a missing target is an external package,
generated output, foreign-language symbol, or genuinely absent unless syntax or
an imported provenance record provides direct evidence for that boundary.

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

## Evidence records

Extend the SQLite image with the minimum constrained records required by the
accepted import formats:

```text
imported_artifacts: path, role, content_hash, byte_size
provenance_links: input artifact/span, generator file/node/span,
                  output artifact/span, kind
coverage_runs: format, report artifact, run_label, optional test node
coverage_regions: run, file, start/end positions, execution_count
coverage_branches: run, file, source/target positions, execution_count
```

Generated Rust artifacts with one inclusion context additionally have ordinary
`files`, `nodes`, `refs`, and resolved `edges` rows. Artifact, provenance, and
observation rows are not a generic property bag: roles, link kinds, and coverage
formats are closed enums for the v1 manifest.

Foreign keys enforce ownership and deletion. Unique constraints reject repeated
run identities and duplicate provenance links. Sealing verifies that every
manifest entry produced its required artifact and link rows and that every
coverage region or branch belongs to one imported report and one captured file.
Derived static edges still come only from resolved source references.

## Gap ledger

Add one `graph_gaps` table with constrained fields equivalent to:

```text
id
file_id nullable
source_id nullable
run_id nullable
path nullable
line_start nullable
line_end nullable
category
reason
target_hint nullable
occurrences
```

Categories are `source`, `parse`, `relation`, `macro`, `generated`, `coverage`,
`language`, and `boundary`. Reasons are a closed Rust enum serialized to stable
lowercase tokens. Coverage reasons include unmapped file/region and missing or
ambiguous test context. `target_hint` is bounded, escaped repository-derived
text; it is never treated as a resolved target.

Per-file gaps reference the indexed file and are removed by the existing
incremental file replacement. Run-owned coverage gaps are removed with their
immutable run. Global inventory gaps have no `file_id`; they are replaced
wholesale from the complete source snapshot during each index build. Reused
files retain their parser gaps from the seed graph. The store validates that a
gap has either a file, a safe path, a coverage run, or a positive aggregate
occurrence count.

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
- `evidence_capture`: whether the supplied manifest and every declared
  generated/coverage artifact were hash-verified and captured;
- `provenance_model`: whether each relevant generated input, generator, output,
  and inclusion site was uniquely linked;
- `execution_mapping`: whether each relevant coverage region and declared test
  context mapped to the selected source graph;
- `traversal`: whether bounded graph queries completed without limits or
  omitted roots.

`complete`, `partial`, and `not-applicable` are status words, not scores.
Accounting can be complete while modeling is partial: Graphr may know exactly
which macro, generated link, dynamic call, or coverage context it cannot model.
Evidence dimensions are `not-applicable` when no manifest was supplied. Invalid
explicit evidence is fatal and never appears as `partial`.

The overall output includes claim records for:

- `affected-callers`;
- `affected-flows`;
- `static-test-paths`;
- `generated-provenance`;
- `changed-execution`.

Each claim separates its answer from its evidence limit:

```text
status=complete|partial|not-applicable
result=linked|observed|not-observed|unknown
basis=resolved-static-call-graph|verified-generated-manifest|llvm-coverage-json|coverage-py-json
```

Static claims omit `result` and retain
`basis=resolved-static-call-graph`. Generated provenance is `linked` only when
the exact chain is present. Changed execution is `observed` for a positive
mapped count, `not-observed` only for a mapped executable region with zero count
in the named run, and `unknown` when evidence is absent or cannot answer the
question. A complete static claim still does not prove runtime execution.

Generated-provenance and changed-execution claims are emitted per relevant
changed span or generated chain, not as one repository-wide Boolean. When
evidence applies, the dynamic evidence summary is partial if any relevant claim
is partial or unknown, even when another changed region was observed.

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
  evidence limit independently of pagination;
- `dynamic_evidence_status=complete|partial|not-applicable` reports imported
  provenance and execution mapping independently of static completeness.

No compatibility aliases are retained.

A compact graph preamble resembles:

```text
completeness content_capture=complete source_capture=complete syntax_parse=complete site_classification=complete static_model=partial evidence_capture=complete provenance_model=complete execution_mapping=complete traversal=complete
gaps total=2 relevant=1 by_reason=ambiguous-target:1,macro-expansion-unavailable:1
claim kind=affected-callers status=partial basis=resolved-static-call-graph
claim kind=affected-flows status=partial basis=resolved-static-call-graph
claim kind=static-test-paths status=partial basis=resolved-static-call-graph
claim kind=generated-provenance path="proto/message.proto" line=18 status=complete result=linked basis=verified-generated-manifest
claim kind=changed-execution path="target/.../out/message.rs" lines=42-48 status=complete result=observed basis=llvm-coverage-json run="strict-roundtrip" test="strict_roundtrip"
provenance input="proto/message.proto:18" generator="src/generator.rs:74" output="target/.../out/message.rs:42"
observed run="strict-roundtrip" test="strict_roundtrip" path="target/.../out/message.rs" lines=42-48 count=1
observed-branch run="strict-roundtrip" test="strict_roundtrip" path="src/predicate.rs" line=31 arm=0 count=1
gap category=macro reason=macro-expansion-unavailable path="src/lib.rs" line=12 occurrences=1
```

The existing files, source diff, artifacts, and graph cursors remain. An
independently paged `evidence_next_cursor` contains relevant provenance chains
and observations. The graph section includes exact gap records owned by changed
roots and emitted affected flows. Repository-wide gaps that affect a claim but
are outside that review neighborhood are summarized by category and reason
rather than flooding the review page.

`view` includes gaps, provenance links, and run observations owned by the
displayed node and keeps its existing bounded omission marker. `search` remains
node-oriented in this milestone; verified generated symbols are searchable
because they use the existing symbol tables.

Changed-content pagination and static evidence are separate terminal facts. A
client can finish reading every immutable page even when static evidence is
partial, then report the exact gaps. Content omissions remain a review-coverage
failure. Terminal page exhaustion includes the evidence cursor when a manifest
was supplied. Every initial and continuation response repeats
`content_complete_when_pages_exhausted`, `static_evidence_status`, and
`dynamic_evidence_status` so a client does not infer any value from cursor
presence.

## Incremental indexing and cache behavior

The schema, analyzer, review, and cache format versions bump together. Old graph
images and review snapshots are rejected and rebuilt; no migration or fallback
is added.

Incremental behavior remains:

- unchanged parsed files reuse nodes, references, gaps, and edges;
- unchanged verified generated files reuse the same parser and replacement path;
- replaced files delete their owned gaps through SQLite foreign keys;
- source-inventory gaps are replaced from the complete new inventory;
- changes to candidate nodes re-resolve affected references and their exact
  resolution state;
- evidence rows are rebuilt from the one captured manifest rather than merged
  with an older run;
- graph image identity covers source omissions, gap-producing analyzer
  semantics, the source snapshot ID, and every imported evidence digest.

Exact repeated evidence input reuses the immutable image. A cancelled or failed
update leaves every prior source or evidence-bearing snapshot unchanged.

## Safety and failures

Repository-derived gap, manifest, context, and run values use the existing
line-safe escaping and bounded path/target limits. Unsafe paths remain counts.
Graphr never reads generated outputs or coverage reports outside the authorized
worktree merely because source or a report names them. Only explicit manifest
paths that pass descriptor-relative validation are captured.

Unsupported syntax produces partial evidence. Corrupt metadata, inconsistent
site accounting, impossible resolution state, invalid gap ownership, invalid
evidence structure, digest or snapshot mismatch, output overflow, or evidence
sealing failure remains fatal. No partial database image is published after an
internal invariant failure.

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
- manifest path, size, symlink, digest, span, source-snapshot, and
  capture-mutation validation;
- generated Rust parsing, `OUT_DIR` inclusion, ambiguous output matching, and
  cross-file call resolution;
- LLVM region/branch count import and rejection of summary-only reports;
- Coverage.py line/branch/context import with missing and ambiguous test
  mappings;
- run-level coverage never becoming named-test evidence without an exact
  mapping;
- evidence cursor ordering, escaping, accounting, and terminal status;
- cache invalidation and rollback when gap, inventory, provenance, or coverage
  semantics change.

One end-to-end fixture combines a resolved call, ambiguous call, dynamic
dispatch, macro-generated boundary, parse gap, skipped source, and exercising
test. It must prove that pagination can finish while affected-caller and
static-test-path claims remain partial for exact named reasons.

The milestone acceptance fixture is the generated Rust path specified above.
It must prove from one `changes` review that both generated encode and decode
call the predicate and that the named test executed both call sites and the
required branch. Negative variants independently remove the decode call,
corrupt a digest, replace per-test coverage with aggregate coverage, and set one
required branch count to zero.

The final verification commands are:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

## Delivery slices

Implementation grows through four working slices:

1. Durable gaps, exact resolution states, completeness dimensions, and renamed
   static output.
2. Evidence-manifest capture, generated-file parsing, provenance links, and
   `OUT_DIR` inclusion.
3. LLVM and Coverage.py observations with exact run/test scope.
4. Evidence pagination, joined review claims, and the end-to-end generated Rust
   acceptance fixture.

Each slice leaves the binary buildable and its existing behavior internally
consistent. The product milestone is all four slices; slice one is not released
or described as the dynamic evidence graph by itself.

## Later evidence producers

After the first dynamic path works, later slices add evidence only when an exact
producer and review query justify it:

1. Before/after evidence comparison consumes two explicit immutable snapshot
   IDs and reports added, removed, observed, and newly unobserved paths.
2. Mutation ingestion records the exact mutant, changed span, outcome, and
   killing test. Intended-assertion claims wait for assertion-level evidence.
3. Runtime call traces add observed caller/callee ordering without changing
   static `CALLS` semantics.
4. Trust-boundary modeling adds explicit source, validator, materializer, and
   sink roles, then compares path claims across immutable base and head graphs.
5. Requirement mapping promotes existing Markdown requirement/citation records
   into explicit requirement-to-symbol and requirement-to-test evidence.
6. Observed build traces provide execution ordering before a general shell CFG
   is justified. Static shell analysis remains a separate language slice.
7. JavaScript, TypeScript, TSX, and Go producers follow the existing product
   order and must expose the same completeness limits.

Each producer must add evidence provenance, declare its own gaps, and leave a
claim partial when its input is absent, stale, or mismatched.
