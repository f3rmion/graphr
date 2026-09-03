# Graphr Review Coverage Design

## Context

Graphr 0.6.0 settled *delivery*: explicit authorized roots, content-addressed
immutable snapshots, asynchronous jobs, typed provenance, and a structured error
model. A reviewer can name exactly which repository, range, and target state a
response describes, and prove the response came from that state.

Graphr has not settled *coverage*: whether the response contains everything a
reviewer needs. This document treats coverage as the limiting factor. That is a
hypothesis, not an established fact — no instrument exists to test it yet, and
the 8 KiB budget is an equally plausible limiter. Milestone 2 builds the
instrument, and its first baseline is what confirms or refutes the premise. If
the baseline shows coverage is already adequate, milestones 4 through 6 are
re-scoped rather than executed on schedule.

This document is a fresh coverage review of the shipped 0.6.0 tree at
`95d2cdb`. It amends one reporting principle, narrows one non-goal, and upholds
the rest; the Supersession section dispositions each line individually. It does
not revisit any 0.6.0 delivery decision; the snapshot architecture is correct
and stays.

Three facts frame the work:

- Every unresolved reference is already persisted. `refs` retains its
  `ref_keys` with `resolved_target_id IS NULL`, and `store.rs:1633-1637`
  already counts those rows as `external_calls` for flow criticality. The graph
  knows what it could not resolve and currently spends that knowledge on a
  scalar — and conflates two distinct populations inside it (see Tiers).
- Several deferred improvements carry a `ponytail:` marker gating them on
  measurement — `store.rs:32` ("stable rowid order stops at the output budget;
  add BM25 only if measured relevance warrants ranking every match"),
  `store.rs:1790` ("the index does not retain decorators; add decorator
  metadata when framework-wired handlers become a measured flow-coverage gap"),
  `python.rs:385` ("bare calls cover the measured Python corpus"). No
  measurement apparatus exists: no `benches/`, no `eval/`, no `[[bench]]`, no
  dev-dependencies. That convention gates the milestones carrying those
  markers. It does not gate defect repair on the shipped surface.
- Every accuracy and token claim in `README.md` is unreproducible from this
  tree and describes call patterns that 0.6.0 made impossible.

## Supersession

Less is retired than a first reading suggests. Each line is quoted exactly,
then dispositioned.

**`2026-08-11-magnus-feedback-release-0.5.0-design.md:35`** — "Do not implement
unrestricted or ambiguous Rust glob resolution."

Upheld, not retired. Nothing in this document resolves an ambiguous glob scope.
Evidence rule 2 below explicitly refuses global uniqueness as evidence, which is
the same principle stated positively.

**`2026-08-11-magnus-feedback-release-0.5.0-design.md:108-109`** — "Ambiguous
glob scopes remain unresolved, which favors missing evidence over false edges."

Amended in scope, not reversed. The resolution behaviour it describes is
unchanged: an ambiguous reference still produces no edge, and Graphr still never
asserts an unproven target. What changes is that an unresolved reference stops
being *silent*. Favouring missing evidence over false edges is right; making
missing evidence indistinguishable from absent evidence is not, because a
reviewer cannot act on silence. The Evidence Tiers model reports the gap without
filling it.

**`2026-08-11-complete-artifact-coverage-design.md:313`** — "Confidence or
provenance changes for test-gap heuristics."

Retired as a blanket bar. Test-path confidence remains a constant string; this
document adds an evidence tier to *call references*, which is a different
subject that the line was never written to cover.

**`2026-08-11-complete-artifact-coverage-design.md:309`** — "Generic-method
resolution improvements."

Narrowed, not retired. Generic instantiation stays out of scope. Trait
implementation expansion is in scope: `trait_implementations` already stores
`resolved_implementor_id` and `resolved_trait_id` behind two partial indexes,
and 0.6.0 already reads both directions for adjacency.

**`2026-08-11-magnus-feedback-release-0.5.0-design.md:36-37`** — "Do not change
risk weights, security-name matching, or the meaning of static affected flows."

Upheld. Risk weight tuning is gated on measurement, and the instrument does not
exist yet. This constrains the tier work: splitting the `external_calls`
population (below) must not change the value `flow_criticality` consumes.
`README.md:131`'s claim that churn factors are not used also stands, and churn
is out of scope here.

**`2026-08-11-magnus-feedback-release-0.5.0-design.md:76-77`** — deferred
inferred receiver typing and macro token-tree extraction to 0.6.0.

Reaffirmed, not retired. Both shipped unmet. Receiver typing is scheduled here;
macro token-tree extraction is deferred again with an explicit reason below.

## Goals

- Distinguish "no caller exists" from "a caller may exist and could not be
  proven", on each impact record and in aggregate per analysed root.
- Resolve the Python and Rust call classes that dominate real code and are
  currently invisible.
- Spend every byte budget on the most relevant records rather than on insertion
  order.
- Make coverage measurable, and gate the milestones whose own markers demand it.
- Fix three coverage defects in the shipped 0.6.0 surface.

## Product Boundaries

Carried unchanged from 0.6.0:

- Keep one Rust binary using MCP over stdio.
- Keep Rust and Python as the only indexed source languages.
- Keep Markdown, TSV, and bounded generic text as reviewed artifacts.
- Do not add HTTP, UI, editor integration, embeddings, plugins, migrations, or
  compatibility layers.
- Keep tool output deterministic and compact.

Added by this document:

- Do not emit an individually named ambiguous call target on the wire. An
  unproven target is reported as a count, never as a record a model can quote as
  fact.
- Do not add a fifth cursor stream, and do not add a new `coverage` category.
- Do not add a scoring model that cannot be recomputed deterministically from a
  published snapshot.
- Do not tune risk weights, security-name matching, or churn in this slice, and
  do not change the value `flow_criticality` consumes.

The JavaScript/TypeScript/TSX and Go expansion named in `AGENTS.md` sits after
these milestones. Depth on Rust and Python comes first because breadth
multiplies whatever per-language coverage exists, and the baseline that would
say what that is does not exist yet.

## Architectural Decision

Adopt evidence tiers for call references, and make relevance the ordering key
for every budgeted result set.

A reference has exactly one tier, derived from state the graph already holds:

- `resolved` — `refs.resolved_target_id IS NOT NULL`. A unique key match.
  Becomes an `edges` row. Behaviour is unchanged from 0.6.0.
- `inferred` — one candidate, or a set the graph has already enumerated as
  complete *within the indexed snapshot*, selected under one of the named
  evidence rules below. Each member becomes an `edges` row carrying its tier.
- `ambiguous` — two or more surviving candidates that the graph has not
  enumerated as complete, or suppression by a poison key
  (`rust:shadowed-value:*`, `rust:ambiguous-import:*`). Never becomes an edge.
  Counted and reported.
- `unresolved` — `resolved_target_id IS NULL` with no surviving candidate: a
  call into `std`, a third-party crate, or any target outside the graph. Never
  becomes an edge, and is **not** counted as ambiguous.

The `ambiguous`/`unresolved` split matters because `store.rs:1633-1637` counts
both together today. A call into `std` is not a coverage gap — it is correctly
outside the graph — and folding it into an ambiguity signal would make that
signal useless on any real repository. The split is presentational only: the
combined count that `flow_criticality` consumes as `external_calls` is
unchanged, per the upheld bar above.

"Enumerated as complete" means complete **within the indexed snapshot**, never
within the world. That distinction is load-bearing for rule 4 below.

The tier is a property of *how the target was selected*, not a probability. No
float score is introduced. This is deliberate: `code-review-graph` ships a
`confidence REAL` column that is never computed and an `AMBIGUOUS` tier that
does not exist in its codebase, and the lesson is that an uncomputed confidence
number is worse than none.

Milestones 2 through 4 require no schema change. `resolved`, `ambiguous`, and
`unresolved` are all derivable from existing `refs` and `ref_keys` rows. Only
`inferred` needs persistence, and only once the first inference rule lands.

## Evidence Rules

An `inferred` edge may be created only by a rule on this list. A rule must yield
either exactly one candidate, or a set the graph has enumerated as complete
within the snapshot. It must never choose among unknowns.

1. **Same-file uniqueness.** A bare call whose name has exactly one definition
   in the calling file resolves to it.
2. **Single-import uniqueness.** A bare call whose name is exported by exactly
   one imported module resolves to it. Global uniqueness across the repository
   is *not* evidence and must not resolve.
3. **Local receiver type.** A Python attribute call whose receiver is `self`,
   or a name bound in the enclosing scope to a resolvable constructor call,
   resolves to the corresponding method.
4. **Trait implementation expansion.** A Rust call to a trait method expands to
   every recorded implementor in `trait_implementations` with a non-null
   `resolved_implementor_id`. Every implementor edge is `inferred`.

   The implementor set is snapshot-complete, not world-complete: `cfg`-gated,
   feature-selected, and downstream implementations are invisible to the index.
   Expansion therefore **suppresses ambiguity only for a trait that is not
   publicly exported from its crate**. For a publicly exported trait the
   expansion still emits its edges, and the call site still contributes to
   `ambiguous_callers`, because an unindexed implementor may exist. Claiming a
   closed world here would manufacture exactly the false confidence this design
   exists to prevent.

Rules 1 and 2 are adopted from `code-review-graph`'s `resolve_bare_call_targets`,
whose refusal to treat global uniqueness as evidence is the correct instinct and
is preserved verbatim in rule 2.

## Relevance Ordering

Every budgeted result set is currently insertion-ordered. `SEARCH_SQL`
(`store.rs:34`) is `ORDER BY nodes_fts.rowid`; the neighbour and trait queries
(`store.rs:1018`, `:1036`, `:3182`, `:3202`) are
`ORDER BY f.path GLOB '.cargo/vendor/*/*', n.id`. The 8 KiB review budget and
the 1536/4096-byte search and view budgets are therefore filled with an
arbitrary subset of candidates.

- `search` orders by `bm25(nodes_fts, …)` with `name` and `qualified_name`
  weighted above `path` and `signature`, node id as final tiebreak.
- Neighbour and trait result sets order by relevance score, then by the existing
  vendored-last predicate, then node id.
- Ordering is computed from snapshot-pinned data only. `snapshot_id` already
  binds the graph image and review generation, so a given snapshot yields a
  byte-identical page for a given cursor. The existing cursor idempotence tests
  remain valid and must continue to pass unmodified.

Two costs, both real and neither optional:

- **`search_query_uses_the_bounded_fts_plan` (`store.rs:3566`) must change.**
  It asserts the plan contains no `TEMP B-TREE`; `ORDER BY bm25(…)` introduces
  one by construction. The test is updated to assert the bounded virtual-table
  scan and the absence of `SCAN n`, dropping only the `TEMP B-TREE` clause.
- **bm25 replaces early termination with a full sort.** Today rowid order
  streams and stops at `LIMIT ?3`. bm25 must score every match, and
  `literal_fts` (`store.rs:3275`) expands every term to `"term"*`, so match sets
  are large by construction. This is the cost `store.rs:32` names as "stops at
  the output budget", and it is why that marker gates the change on measured
  relevance. Milestone 3 carries the search-relevance benchmark that opens its
  own gate.

Impact-set ordering is **not** part of this milestone; the score it would sort
on is defined in milestone 6 and does not exist before then.

## Impact Traversal

The caller closure traverses `edges.kind='CALLS'` only, unweighted, truncating
by `ORDER BY changed_id, node_id LIMIT`. A module that imports a changed type
without calling it is invisible, and truncation drops arbitrary callers rather
than the least relevant.

Traversal becomes weighted and multi-kind. A path's score is the product of the
traversed edge weights and a depth decay of `0.6`:

| Edge kind | Weight | Direction | Rationale |
| --- | ---: | --- | --- |
| `CALLS` (`resolved`) | 1.00 | incoming | Caller is the edge source |
| `CALLS` (`inferred`) | 0.75 | incoming | Same, discounted for selection method |
| `TEST_CALLS` | 0.70 | incoming | **Test is the edge source** (`store.rs:3162`, query `test <-`) |
| `IMPORTS` | 0.50 | incoming | Importer is the edge source |

Every kind traverses incoming, because in every case the dependent — caller,
importer, or exercising test — is stored as the edge source. `TEST_CALLS` is
written test-as-source and every existing query finds tests via `target_id`;
traversing outgoing would return what a changed symbol calls and find no tests
at all.

**A node's score is the maximum score over all eligible paths reaching it.** A
node appears at most once in the impact set, at its best-scoring path. Ties
break on `node_id`. Without this rule two implementations could rank or
duplicate the same node differently and the result would not be deterministic.

**There is no score floor.** `depth` is API-capped at 6 and `max_nodes` bounds
output, so those two are the only traversal bounds and score is purely a ranking
and truncation key. A floor would silently make the advertised depth
unreachable: under a compounding product, a six-hop `IMPORTS` path scores
`0.5^6 × 0.6^6 ≈ 0.0007`, so any floor large enough to prune usefully also
prunes paths at depths the surface tells clients to request.

Truncation drops the lowest-scoring node, and the existing `callers_omitted`
counter continues to report the drop.

This changes which nodes appear in the impact set and their order. It does not
change `node_risk` weights, `flow_criticality` weights, security-name matching,
or the meaning of a static affected flow, all of which remain as shipped.

## Entry Points

The structural rule is already shipped and must not be rebuilt. `store.rs:1364`
reads `if current.kind == "function" && (callers.is_empty() ||
conventional_entry(&current.name))`, so a caller-less function is already an
entry point, and `load_flow_neighbors` already restricts incoming callers to
`n.kind='function'`.

The real gap is `Test` nodes. `store.rs:1329` filters them out of flow seeding
(`roots.iter().filter(|node| node.kind != "test")`) and `store.rs:1364` excludes
them from entry classification. A test is an entry point by definition — nothing
calls it — and excluding tests from flow tracing means no flow ever begins at
the one place coverage is provable.

The change is narrow: remove the seed filter and widen the entry predicate to
admit `Test`. Flow criticality weights are untouched.

Attribute and decorator retention — `#[tokio::main]`, `#[test]`, route macros,
`@app.route` — is deferred. It requires the macro token-tree extraction that
`0.5.0-design:76-77` deferred once already. Milestone 6 carries the flow-recall
benchmark that measures what the `Test`-node change leaves missing, which is the
evidence `store.rs:1790` demands before decorator metadata is added.

## Coverage Defects in 0.6.0

Three defects in the shipped surface, each independent of everything else here.

**Default depth contradicts the server's own instructions.** `ChangesParams`
defaults `depth` to `1` (`mcp.rs:123-125`, `default_changes_depth` at
`mcp.rs:439-441`) while `get_info` (`mcp.rs:254`) instructs the agent to call
with depth 6. A client omitting the parameter silently receives a
six-times-shallower blast radius with no warning. `depth` and `max_nodes` become
required parameters with no defaults, matching `index`, which already requires
all five of its parameters. **This is a breaking change** for any client that is
not the bundled skill, and it sets this milestone's release boundary.

**Two-dot diff where review needs merge-base.** Commit-target capture pushes
`base_oid` and `head_oid` as separate positional arguments to `git diff`
(`git.rs:1431-1434`), which is two-dot semantics. When the base branch has
advanced, commits the author never wrote are reported as changes. The bundled
skill instructs the agent to select merge-base as the review base for a branch
or pull request, which is precisely the case this gets wrong. A worktree
reviewed against an advanced base exhibits the identical defect, so the rule is
uniform rather than commit-only: `resolve_request` (`workspace.rs:1721`)
resolves `base_oid` to the merge-base of `base` and the resolved head, at the
single point where refs become OIDs. Every downstream consumer — capture,
`snapshot_id`, provenance — is then correct without change, and `git.rs` is
untouched. Where no merge-base exists, the request fails with a structured
error rather than silently degrading to two-dot.

**Snapshots are invisible until attached.** `SnapshotCatalog::attach`
(`workspace.rs:502`) runs only from `build_snapshot` (`index.rs:98`) and
`inspect_root` (`index.rs:368`). After a server restart, a valid on-disk
`snapshot_id` returns `SnapshotNotFound` until something re-attaches that root,
so a resumed review sees a false "snapshot gone".

`attach` takes a `RootIdentity`, and nothing maps a content-addressed
`snapshot_id` back to its owning root, so "attach the owning root" is not
derivable at lookup time. On a lookup miss the catalog inspects and attaches
every allowed root **once**, then retries the lookup **once**. The retry is
bounded because `attach` is expensive and destructive: it enumerates and
revalidates every manifest under the root, hashes graph images, and calls
`reconcile` (`workspace.rs:558`), which evicts loaded snapshots whose manifest
has disappeared. An unknown id must not trigger a repeated full catalog rescan
on the synchronous query path.

## Measurement

Coverage claims require an instrument. Add `graphr-eval` as a workspace member,
excluded from the published crate, so the binary's dependency set is untouched.

**Co-change recall is necessary but not sufficient.** Files an author happened
to touch in one commit are not the same set as the files a reviewer needed to
read: a commit can touch unrelated files while a required definition or test
stays unchanged. Milestone 2 therefore ships two benchmarks, and the premise
decision requires both:

1. **Co-change recall** — milestone 2. Seed impact analysis with one changed
   file from a pinned commit; grade against the *other* files the author touched
   in that commit. Ground truth is git history, not the graph, so it is not
   circular. `code-review-graph`'s equivalent returns zero predictions on every
   graded commit, so no comparable published figure exists.
2. **Review-context recall** — milestone 2. A hand-labelled set, pre-registered
   before any milestone runs, naming the files a competent reviewer needed for
   each fixture change — explicitly including required-but-unchanged files that
   co-change can never capture.
3. **Search relevance** — milestone 3. MRR over curated query/expected pairs.
   Opens the gate `store.rs:32` sets on BM25.
4. **Edge-tier precision** — milestones 4 and 5. On the hand-labelled fixtures,
   the share of `inferred` edges that are correct, and the share of true callers
   that land in `ambiguous` rather than being found. Milestone 4 records the
   baseline; milestone 5 must improve caller discovery without reducing
   `inferred` precision.
5. **Flow recall** — milestone 6. Detected entry points against a hand-labelled
   set. Opens the gate `store.rs:1790` sets on decorator metadata. Milestone 6
   additionally requires **no regression in co-change and review-context
   recall**, because the weight table decides which nodes survive `max_nodes`
   and flow recall would not detect that harm.

**Gates are pre-registered and falsifiable.** Before milestone 2 runs, record
for each of milestones 3 through 6 a minimum effect size on its own metric and a
non-regression guardrail on the others. A milestone whose metric moves less than
its recorded effect size, or that regresses a guardrail metric, is re-scoped or
reverted — not retained on the strength of the argument that motivated it. A
directional movement alone is not a pass.

Rules, adopted from `code-review-graph`'s harness because both were learned from
real measurement bugs:

- A benchmark whose tool call raises is recorded as `status=error` and excluded
  from every aggregate. A crash must never score as a win.
- Every fixture pins an upstream SHA. Fixture repositories are cloned into a
  scratch directory and never operated on in place.
- Any metric whose ground truth derives from the graph under test is labelled
  circular in its output and may not be quoted as an accuracy result.

Fixtures: two Rust and two Python repositories, pinned, with a documented
selection rationale, including at least one where coverage is expected to be
poor so the baseline can discriminate.

Every benchmark number in `README.md` is deleted when milestone 2 lands, because
the 0.6.0 surface makes the measured call patterns impossible. The README
benchmark section stays absent until the harness produces a reviewer-visible
figure — review-context token cost per fixture change, alongside the internal
graph-quality metrics — and that figure is a milestone 2 deliverable.

## Wire Format

Additive only. The four cursor names, the section order, and the 8 KiB budget
are unchanged, so the bundled skill continues to parse correctly.

- Graph records carry `tier=resolved` or `tier=inferred`.
- Each impact record carries `ambiguous_callers=N`, omitted when zero.
- Each analysed root carries `ambiguous_callers=N` as the aggregate, omitted
  when zero.
- `unresolved` references are never counted in either field.

**Attribution is deterministic.** An ambiguity is attributed to the impact
record containing the *call site*, not to any candidate target. Multiple
ambiguous references from one source to one changed root count once per
reference, not once per surviving candidate. Poison-key suppressions count
exactly like multi-candidate ambiguities. The root aggregate is the sum over its
records, so per-record and root totals always reconcile.

**Completeness gains a third state.** `ambiguous_callers` does not emit a
`coverage category=…` line and does not name a remediation: every shipped
remediation (`call-index-then-restart-changes`,
`call-view-on-each-emitted-changed-node-ref`,
`narrow-review-base-and-restart-changes`, `review-corresponding-diff-pages`) is
an action that resolves its gap, and an unprovable target is irreducible.
Routing it through `coverage` would make every real review report permanent
incompleteness and train reviewers to ignore the banner.

But excluding it entirely lets a client stop while a caller list is known short.
So the terminal signal distinguishes **complete** from
**complete-with-uncertainty**: pages are exhausted and no remediation is
outstanding, and a non-zero `ambiguous_callers` is present. The bundled skill
must read the changed symbol's call sites directly before concluding, and must
report the residual uncertainty in its findings. This is a new terminal *value*,
not a new cursor stream or `coverage` category, so the closed four-cursor set is
untouched.

New fields consume budget, and budget is the scarcest resource on the surface.
`tier` is a short token on records that already exist, and `ambiguous_callers`
is one integer per record and per root, not per reference.

## Acceptance Tests

All tests use real temporary Git repositories, following the existing suite.

1. A Rust call whose name is claimed by two definitions produces no edge; the
   impact record and its root both report `ambiguous_callers=1`.
2. A call into a third-party crate produces no edge and does **not** increment
   `ambiguous_callers`.
3. A root with non-zero `ambiguous_callers` reports
   complete-with-uncertainty, not incomplete, and names no remediation.
4. Two ambiguous references from one source to one root count as 2; a
   poison-key suppression counts as 1; root totals equal the sum of records.
5. A Rust call resolvable only through same-file uniqueness produces one edge
   with `tier=inferred`.
6. A Python `self.method()` call produces one edge with `tier=inferred`; a call
   on an unresolvable receiver produces no edge and increments
   `ambiguous_callers`.
7. A crate-private trait with two implementors expands to two `inferred` edges
   and suppresses ambiguity; a publicly exported trait expands to its edges and
   still increments `ambiguous_callers`.
8. A test exercising a changed symbol appears in that symbol's impact set.
9. Every edge kind in the weight table is reachable at `depth=6`.
10. A node reachable by two paths appears once, scored by the better path.
11. A module importing a changed type without calling it appears in the impact
    set, ranked below a direct caller.
12. Impact truncation at the node limit drops the lowest-scored node.
13. `search` returns an exact `qualified_name` match first where one exists.
14. Two identical cursor requests against one snapshot return byte-identical
    pages, with ranking enabled.
15. On a fixed fixture, every mandatory section and outstanding remediation is
    still present in the 8 KiB response after `tier` and `ambiguous_callers`
    are added.
16. `changes` called without `depth` fails with a structured error.
17. A commit-target request whose base branch has advanced reports only
    head-side changes.
18. A snapshot id valid on disk resolves after a server restart without an
    intervening `inspect_root`, and an unknown id triggers at most one rescan.
19. A test with no incoming `CALLS` edge seeds a flow and is classified as an
    entry point.
20. `flow_criticality` output is unchanged for a fixture whose unresolved
    references split across `ambiguous` and `unresolved`.

## Delivery Sequence

Independently reviewable milestones. Each lands with its own plan.

1. **Coverage defects.** Required parameters, merge-base, bounded
   attach-on-miss. Correctness fixes on a published surface, carrying no
   `ponytail:` marker, so nothing gates them. Ships first.
2. **Measurement.** `graphr-eval`, co-change recall, review-context recall,
   pre-registered gates, fixtures, README benchmark removal.
3. **Relevance ordering.** BM25, neighbour and trait score ordering, plan-test
   update, search-relevance benchmark. No schema change.
4. **Evidence tiers.** Four derived tiers, attribution rules, per-record and
   per-root `ambiguous_callers`, complete-with-uncertainty terminal state. No
   schema change.
5. **Inference rules.** Same-file and single-import uniqueness, Python local
   receiver typing, trait implementation expansion. Introduces the `inferred`
   tier's persistence and the only schema bump in this document.
6. **Weighted impact, `Test` entry points, flow-recall benchmark.**

Milestone 2 gates milestones 3 through 6 by convention: 3, 5, and 6 carry
`ponytail:` markers demanding measurement, and 4 is included because the Context
premise decision governs whether it runs at all. Its re-scope decision is
recorded before milestone 4 starts. Milestone 1 carries no marker and is not
gated. Milestones 1 and 2 are mutually independent. **Milestone 5 precedes
6**, because milestone 6 assigns a distinct weight to `CALLS (inferred)` and
those edges do not exist until 5 lands; milestone 6 verification must exercise
persisted `inferred` edges.

Two breaking changes reach installed users, and release notes must name both:

| Milestone | Break | Release |
| --- | --- | --- |
| 1 | Required `depth`/`max_nodes` rejects previously valid `changes` calls | **0.7.0** (minor — a breaking wire change cannot ship as a patch) |
| 2-4 | none, additive | 0.7.x |
| 5 | `SCHEMA_VERSION` bump hard-fails `serve` until `index --rebuild` | **0.8.0** |
| 6 | none | 0.8.x |

## Release Verification

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

Additionally, per milestone from 3 onward: the milestone's own metric and every
guardrail metric, before and after, with the fixture set and pinned SHAs
recorded alongside them, and an explicit keep/re-scope decision against the
effect size pre-registered in milestone 2.
