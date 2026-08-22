# Dynamic evidence graph final fix report

Date: 2026-08-22

Base: `5ed7c36`

Branch: `feat/dynamic-evidence-graph`

Scope: the ten Important findings from the final review. The two noted Minor
findings (exact-hit `files_total` and static-gap sort priority) remain
intentionally unchanged.

## 1. Aggregate LLVM runs without a test name

Root cause: execution-claim completeness incorrectly required either a manifest
`test_name` or a Coverage.py context. That attribution requirement overrode an
otherwise complete mapped run-level observation, so both positive and zero LLVM
counts became partial/unknown.

Focused RED:

```text
cargo test store::tests::changed_execution_claim_renders_scoped_counts_and_keeps_static_test_calls -- --exact --nocapture
```

Exit 101. The zero-count aggregate assertion expected a complete
`not-observed` run claim, but the renderer emitted a partial/unknown claim.

Minimal GREEN change: determine claim completeness solely from exact file,
region, and context mapping gaps. Retain `test=None` for aggregate LLVM runs;
positive and zero counts now answer `observed` and `not-observed` respectively.
No runtime edge is created and the existing static `TEST_CALLS` edge remains
unchanged.

Focused GREEN: the same exact command exited 0 with 1 passed, 0 failed. The test
asserts positive and zero aggregate claims, absence of `test=`, and an unchanged
single static `TEST_CALLS` edge.

Affected files: `src/store.rs`.

Contract/doc implication: implements the progress-ledger ruling that aggregate
LLVM evidence is a complete run-level claim and is not a synthetic named-test
claim. No design-spec change was required.

## 2. Coverage.py empty contexts and signed arcs

Root cause: the decoder rejected the valid empty context string, and the typed
decoder/store/schema boundary represented arc endpoints as unsigned lines. That
made entry and exit arcs such as `[-1,8]` and `[8,-1]` unrepresentable and also
made seal recomputation assume every branch endpoint was a positive range.

Focused RED:

```text
cargo test coverage::tests::coverage_py_accepts_empty_run_context_and_signed_entry_exit_arcs -- --exact --nocapture
cargo test store::tests::coverage_py_signed_arcs_round_trip_through_store_render_and_seal -- --exact --nocapture
```

Both commands exited 101 before production changes. The decoder returned
`Coverage.py context is invalid`; the store round-trip failed evidence
relationship validation for signed endpoints.

Minimal GREEN change: normalize `""` to a run-level `None` context while still
rejecting duplicate empty contexts; use signed `i64` branch start/end/target
fields; validate arcs as two non-zero signed endpoints with at least one
positive repository line; map using positive endpoints while storing,
querying, sealing, and rendering the original signed values. Non-arc LLVM
branches retain positive-range validation. Schema and cache identities were
bumped because stored evidence semantics changed.

Focused GREEN: both exact commands exited 0 with 1 passed, 0 failed. The signed
store test also sealed the image and passed `validate_image`. The broader
`cargo test coverage::tests` run passed all 9 decoder tests, including malformed
duplicate and contradictory observations.

Affected files: `src/coverage.rs`, `src/store.rs`, `src/index.rs`,
`src/workspace.rs`, and
`docs/superpowers/plans/2026-08-22-dynamic-evidence-graph.md`.

Contract/doc implication: the implementation plan's `BranchObservation` and
`CoverageBranchInput` endpoint types are now signed, as ruled in the progress
ledger. The approved design spec was not amended. This is a new schema/cache
image, not a migration or compatibility path.

## 3. LLVM region discriminators

Root cause: the LLVM importer parsed the tuple discriminator but ignored it,
turning expansion, skipped, gap, and branch/MC/DC region tuples into executable
observations. It likewise accepted arbitrary file-level branch kinds.

Focused RED:

```text
cargo test coverage::tests::llvm_imports_only_code_regions_and_valid_branch_region_kinds -- --exact --nocapture
```

Exit 101. The decoder returned observation start lines for every region kind
instead of only the executable code region.

Minimal GREEN change: import only region kind `0`, explicitly ignore documented
non-code kinds `1..=6`, reject unknown kinds, and accept only documented
file-level branch-region kinds `4` and `6`.

Focused GREEN: the exact command exited 0 with 1 passed, 0 failed; the broader
coverage decoder suite passed 9/9.

Affected files: `src/coverage.rs`.

Contract/doc implication: narrows the decoder to the documented LLVM tuple
shapes already required by the design and plan; unsupported shapes fail closed.

## 4. Anonymous external coverage boundaries

Root cause: pathless `coverage-unmapped-file` gaps were considered relevant only
after another observation from the same run became relevant. All-external runs
therefore appeared not-applicable, and mixed runs could hide the aggregate
boundary.

Focused RED:

```text
cargo test store::tests::external_coverage_gap_is_anonymous_partial_in_changes_and_view -- --exact --nocapture
```

Exit 101. An all-external run returned complete/not-applicable instead of a
partial/unknown execution claim.

Minimal GREEN change: always include the pathless aggregate external-file gap
in scoped public evidence, count it when determining execution applicability,
and render only the run label, reason, and occurrence count. The source name is
never stored in the gap and therefore cannot be echoed.

Focused GREEN: the exact command exited 0 with 1 passed, 0 failed. It checks
`changes` and `view`, partial status, the anonymous aggregate record, and absence
of the fixture's external path.

Affected files: `src/store.rs`.

Contract/doc implication: all-external and mixed reports expose an aggregate
trust boundary without disclosing absolute names or paths.

## 5. One output per inclusion site

Root cause: generated outputs were resolved independently by basename. Two
different output paths could each observe the same single `OUT_DIR/out.rs` site,
be parsed in that context, and acquire verified links even though neither
output-to-site identity was unique globally.

Focused RED:

```text
cargo test index::tests::same_basename_outputs_contending_for_one_include_are_all_ambiguous -- --exact --nocapture
```

Exit 101. The generated graph still contained parsed files/nodes for the two
contending outputs.

Minimal GREEN change: precompute distinct output paths by basename before any
generated parsing. A contended basename records an ambiguity for every output
and skips generator mapping, provenance, parsing, and call resolution. The
transactional insertion boundary and seal validation also reject a modeled site
linked to multiple outputs or an output linked to multiple modeled sites;
multiple declared chains may still share the same exact output/site pair.

Focused GREEN: the exact command exited 0 with 1 passed, 0 failed and asserted
zero generated files, nodes, refs, and provenance links plus two ambiguity
gaps.

Affected files: `src/index.rs`, `src/store.rs`, and `src/workspace.rs` (evidence
semantic/cache version).

Contract/doc implication: verified inclusion is globally one-output to
one-site. Contention is explicit partial evidence rather than an inferred link;
insertion failures remain transactional.

## 6. Provenance completeness per declaration

Root cause: completeness compared the number of distinct complete output paths
with the number of output artifacts. When declarations shared an output, one
successful link could mask another declaration's failed generator/include
mapping.

Focused RED:

```text
cargo test store::tests::provenance_completeness_is_per_declared_chain_for_shared_output -- --exact --nocapture
```

Exit 101. The scoped review did not emit `provenance_model=partial` for the
failed declaration sharing a successfully linked output.

Minimal GREEN change: evaluate every relevant provenance row and every relevant
manifest-declaration generated gap, emit a partial per-chain claim for the
failed output/generator span, and keep its exact gap visible. Node-owned static
generated gaps remain visible but no longer masquerade as failed manifest
chains or lower `provenance_model` by themselves.

Additional self-review RED:

```text
cargo test store::tests::static_generated_gap_does_not_masquerade_as_failed_manifest_chain -- --exact --nocapture
```

Exit 101 at the expected `provenance_model=not-applicable` assertion. After the
ownership distinction, the same command exited 0 with 1 passed, 0 failed.

Focused GREEN: the shared-output exact command exited 0 with 1 passed, 0 failed
and asserted the failed output/generator claim, exact gap, and partial dynamic
status.

Affected files: `src/store.rs`.

Contract/doc implication: provenance status is per declared chain rather than
per distinct output path. Static include discovery and manifest provenance
remain separate evidence classes.

## 7. Repository-local Rust/Python generators only

Root cause: generator-span lookup filtered only node kind, so repository
JavaScript/TypeScript functions could be accepted as complete generators for a
Rust output despite the milestone's Rust/Python producer boundary.

Focused RED:

```text
cargo test store::tests::generator_mapping_accepts_only_repository_rust_and_python_nodes -- --exact --nocapture
```

Exit 101. The JavaScript fixture mapped as `Unique` instead of `Missing`.

Minimal GREEN change: add the same `files.language IN ('rust','python')`
condition to live mapping, transactional insertion, and sealed-image invariant
validation.

Focused GREEN: the exact command exited 0 with 1 passed, 0 failed, proving Rust
and Python unique while JavaScript and TypeScript remain missing.

Affected files: `src/store.rs`.

Contract/doc implication: enforces the design's initial repository-local
Rust/Python generator chain without adding a JS/TS provenance feature.

## 8. Python absolute import classification

Root cause: the Python analyzer guessed any absolute import whose first
component differed from the current file's first module component was external.
For a repository root `app.py`, `import util` was therefore discarded as an
external boundary before the normal key resolver could find sibling `util.py`.

Focused RED:

```text
cargo test index::tests::python_absolute_import_resolves_sibling_and_keeps_unknown_missing -- --exact --nocapture
```

Exit 101. The sibling import reference was absent and the test failed while
locating it.

Minimal GREEN change: remove the top-component external-import heuristic.
Classifiable absolute imports now emit ordinary repository candidate keys;
normal resolution produces a sibling match or an exact `missing` state.

Focused GREEN: the exact command exited 0 with 1 passed, 0 failed. It proves
`import util` resolves to sibling `util.py`, while an absent package remains
missing and is not guessed external.

Affected files: `src/python.rs`, `src/index.rs`, and `src/workspace.rs` (analyzer
and review cache identities).

Contract/doc implication: unknown absolute imports stay explicit missing
references. No search path, import execution, or external-package inference was
added.

## 9. Missing versus ambiguous aliases

Root cause: both the in-memory resolver and sealed/store recomputation inserted
a missing alias exporter as an ambiguous alias candidate. SQL alias aggregation
also treated any unresolved exporter as ambiguity, conflating zero candidates
with multiple/conflicting candidates.

Focused RED:

```text
cargo test index::tests::imported_aliases_preserve_missing_and_ambiguous_candidates -- --exact --nocapture
cargo test store::tests::seal_recomputes_missing_and_ambiguous_aliases_without_conflating_them -- --exact --nocapture
```

Both commands exited 101 before the fix: the live missing consumer was
ambiguous, and seal recomputation reported a candidate mismatch.

Minimal GREEN change: omit missing exporters from alias candidate maps, retain
ambiguous exporters, and aggregate SQL aliases by explicit ambiguous state plus
the number of distinct resolved targets. Apply the same rule to live
resolution, incremental resolution, script export methods, trait aliases, and
seal recomputation.

Focused GREEN: both exact commands exited 0 with 1 passed, 0 failed; the seal
test also passed image validation. Existing re-export and trait-resolution
expectations were updated to the exact missing/unique semantics.

Affected files: `src/index.rs`, `src/store.rs`, and `tests/e2e.rs`.

Contract/doc implication: `missing` means zero candidates and `ambiguous` means
multiple or conflicting candidates at every live and sealed boundary.

## 10. Public evidence scope

Root cause: static gap rendering queried every repository gap, and coverage
scope eagerly inserted every generated output. `changes` and `view` could
therefore spend bounded output on unrelated exact gaps, provenance chains, and
observations. `view(depth>0)` also inherited dynamic evidence from displayed
neighbors rather than keeping evidence owned by the requested node.

Focused RED:

```text
cargo test store::tests::changes_and_view_emit_only_relevant_exact_static_and_dynamic_records -- --exact --nocapture
```

Initial exit 101: the changed-root response contained the unrelated static exact
gap. A self-review extension that inserted a traversed view neighbor also exited
101 and printed the unrelated provenance chain and coverage observation.

Minimal GREEN change: select exact static gaps only when owned by changed roots
or emitted affected-flow nodes; keep repository-wide summary counts. Build a
coverage/provenance scope from changed spans, changed nodes, and affected flows,
then expand only through relevant provenance chains. `view` renders root-owned
static gaps and root-owned dynamic evidence plus that root's provenance closure;
displayed graph neighbors do not import their evidence.

Focused GREEN: the exact command exited 0 with 1 passed, 0 failed. It retains the
changed/root-owned static gap, chain, and observation; retains the aggregate
`gaps total=2` summary; displays a neighbor node; and excludes that neighbor's
exact gap, provenance, and observation.

Affected files: `src/store.rs` and `tests/e2e.rs`.

Contract/doc implication: `changes` exact records are limited to changed roots
and affected flow, while out-of-neighborhood impact remains summarized. `view`
evidence is owned by the requested node, as specified. Search remains
node-oriented.

## Full gate

The required commands ran serially after the final production and test edits:

1. `cargo fmt --check` — exit 0, no output.
2. `cargo clippy --all-targets -- -D warnings` — exit 0;
   `Finished dev profile [unoptimized + debuginfo] target(s) in 2.52s`.
3. `cargo test` — exit 0:
   - unit binary: 280 passed, 0 failed, 0 ignored, finished in 2.33s;
   - CLI integration: 5 passed, 0 failed, 0 ignored, finished in 0.21s;
   - end-to-end integration: 48 passed, 0 failed, 0 ignored, finished in
     9.91s.
4. `cargo build --locked --release` — exit 0;
   `Finished release profile [optimized] target(s) in 12.78s`.
5. `git diff --check` — exit 0, no output.
6. `git status --short` — exit 0 with the expected seven tracked paths before
   staging/commit:

```text
 M docs/superpowers/plans/2026-08-22-dynamic-evidence-graph.md
 M src/coverage.rs
 M src/index.rs
 M src/python.rs
 M src/store.rs
 M src/workspace.rs
 M tests/e2e.rs
```

This report lives beneath the intentionally ignored SDD report directory and is
force-added to the final commit.

## Self-review

- The design spec was left unchanged; only the implementation plan's defective
  unsigned arc interface was corrected.
- No dependency, migration, compatibility layer, plugin, HTTP/UI surface, or
  speculative language feature was added.
- Decoder and database inputs continue to reject malformed, duplicate, and
  contradictory evidence.
- Provenance cardinality and generator language are checked during insertion
  and again during seal/image validation. Evidence replacement remains one
  transaction, so a trust-boundary failure rolls back rather than publishing a
  partial image.
- External coverage paths remain anonymous aggregate counts and are never
  opened or rendered.
- Output ordering remains deterministic and bounded. The two nonblocking Minor
  findings were not expanded into this fix wave.
