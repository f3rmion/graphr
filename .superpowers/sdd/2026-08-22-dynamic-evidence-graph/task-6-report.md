# Task 6 final fix report

Date: 2026-08-22

Base: `fe3db9fdd1b30540f7aecdfee7fedb389633b827`

Scope: preserve one public provenance claim per manifest declaration and restrict exact evidence to changed roots/emitted affected flows or the selected view node plus their provenance closure. No migration, compatibility path, dependency, protocol, UI, or transport change was added.

## Root-cause trace

### 1. Failed declaration identity and duplicate claims

The manifest decoder retained the full input/generator/output declaration, but `build_generated_evidence` diverted a failed generator or inclusion lookup into `EvidenceInput.gaps`. The `graph_gaps` identity did not contain the declared input, so two declarations differing only by input collapsed. A unique generator combined with a failed inclusion could also leave a provenance row and produce a generated gap. The renderer independently synthesized claims from provenance rows and generated graph gaps, so one declaration could become two public claims while a collapsed pair could become one.

The durable fix is one representation, not a second declaration table: every manifest declaration is now stored once in `provenance_links`. The row contains the exact input artifact/span, declared generator path/span, nullable uniquely resolved generator identity, exact output artifact/span, nullable uniquely resolved inclusion identity, and a constrained `linked`, `unobserved`, or `ambiguous` state. The evidence transaction derives the nullable identities and state from the indexed graph. Rendering emits exactly one claim, and its optional limit line, from that row. `graph_gaps` remains exclusively static/parser/coverage evidence and rejects evidence-imported generated gaps.

### 2. Exact evidence scope

`changes` previously put every `traverse_changes` neighbor into the exact evidence scope. That confused graph display breadth with evidence ownership. Provenance closure also started from generator/output/inclusion fields but omitted the declared input. Finally, generated gaps were considered relevant by path, so selecting one function could reveal an unrelated function's same-file gap.

The fix keeps `traversed_ids` only for graph display/accounting and builds exact evidence scope from changed roots plus nodes in emitted `AffectedFlow` records. `view` starts from only the selected node. Provenance closure is bidirectional over input, generator, output, and inclusion spans and resolved owners. Static/generated gaps require an exact owner, overlapping range, or an explicitly whole-file scope; same-file membership alone is not sufficient. Global gap totals and confidence remain global, so omitted outside-neighborhood evidence still affects the summary without consuming bounded exact rows.

## Focused RED and GREEN evidence

### Failed declarations retain input identity and render once

Production mutation caught: routing failed declarations back through generic generated gaps, omitting input identity, or rendering both a provenance row and a generated gap.

RED command (the first run intentionally used Cargo's substring filter; a fully qualified `--exact` run was used for GREEN):

```text
cargo test failed_generated_declarations_keep_distinct_input_identity_and_one_claim_each -- --nocapture
```

RED result: exit 101; 1 failed, 280 filtered. The assertion failed with `manifest declarations must not escape into generic graph gaps`.

Minimal GREEN change: `build_generated_evidence` now forwards every declaration as `ProvenanceInput`; the store resolves it and the renderer produces one claim from the stored declaration row.

```text
cargo test index::tests::failed_generated_declarations_keep_distinct_input_identity_and_one_claim_each -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 286 filtered out
```

The test supplies two failed declarations differing only by input and requires two claims naming both input spans, with no evidence-generated graph gap.

### Transactional declaration uniqueness and seal recomputation

Production mutations caught: allowing duplicate exact declaration identities, trusting caller-provided mapping state, or validating only SQL nullability rather than recomputing graph resolution.

These hardening tests were added with the unified row implementation; they are GREEN invariant proofs rather than retroactively claimed RED runs:

```text
cargo test store::tests::duplicate_provenance_declaration_rolls_back_the_evidence_transaction -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 282 filtered out

cargo test store::tests::seal_and_image_validation_recompute_provenance_declaration_state -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 282 filtered out
```

The duplicate test proves the prior generation remains committed with zero leaked artifacts/declarations. The seal test changes a linked row to a constraint-valid but graph-inconsistent unobserved row; both sealing and image validation reject `database provenance declaration mapping is inconsistent`.

### Unsafe stored declaration identity

Production mutation caught: checking artifact paths only during manifest decoding and then trusting a constraint-valid stored declaration during seal/image validation.

```text
cargo test store::tests::seal_rejects_unsafe_provenance_declaration_identity -- --exact --nocapture
```

RED result: exit 101; 1 failed, 286 filtered. A row mutated to `generator_path='../escape.rs'` sealed successfully.

Minimal GREEN change: expose and reuse the exact manifest path predicate in the evidence transaction and provenance invariant recomputation.

```text
cargo test store::tests::seal_rejects_unsafe_provenance_declaration_identity -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 286 filtered out
```

### Traversal-only neighbors do not own exact evidence

Production mutation caught: adding all displayed traversal neighbors to the exact evidence node set.

```text
cargo test store::tests::traverse_only_neighbor_does_not_enter_exact_evidence_scope -- --exact --nocapture
```

RED result: exit 101; 1 failed, 285 filtered. The review reported `flows_total=0` but emitted the traversal-only neighbor's exact `neighbor-gap`.

Minimal GREEN change: retain separate `traversed_ids` and `evidence_node_ids`; only changed roots and affected-flow nodes seed exact changes evidence.

```text
cargo test store::tests::traverse_only_neighbor_does_not_enter_exact_evidence_scope -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 285 filtered out
```

### Changed input spans pull their declaration chain

Production mutation caught: provenance closure that includes generator/output/inclusion but omits the declared input artifact/span.

```text
cargo test store::tests::changed_input_span_pulls_its_exact_provenance_chain -- --exact --nocapture
```

RED result: exit 101; 1 failed, 285 filtered. The provenance claim was present after an initial partial edit, but the generated-output LLVM observation remained absent and `execution_mapping=not-applicable`, exposing the incomplete closure.

Minimal GREEN change: include exact input paths/spans in declaration relevance and closure. The final test uses a real unsupported `.proto` changed path to exercise whole-file root ownership.

```text
cargo test store::tests::changed_input_span_pulls_its_exact_provenance_chain -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 285 filtered out
```

### Same-file generated gaps require exact ownership

Production mutation caught: treating any gap whose path is in scope as relevant to a span-scoped function.

```text
cargo test store::tests::same_file_generated_gap_is_scoped_by_owner_not_path -- --exact --nocapture
```

RED result: exit 101; 1 failed, 285 filtered. A selected line 1-2 function leaked a line 10 `other-generated.rs` gap from another owner.

Minimal GREEN change: range/owner-aware gap relevance for both `changes` and `view`, with path-only matching reserved for explicit whole-file scope.

```text
cargo test store::tests::same_file_generated_gap_is_scoped_by_owner_not_path -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 285 filtered out
```

The same test also requires the unrelated exact row to remain absent while `gaps total=1` and static completeness stays partial.

### Basename contention and shared output hardening

Production mutations caught: choosing one of two distinct outputs with the same basename for one inclusion site, or counting completeness per output instead of per declaration.

```text
cargo test index::tests::same_basename_outputs_contending_for_one_include_are_all_ambiguous -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 285 filtered out
```

The strengthened test persists both declarations, requires zero verified links and zero generated parsing, and renders two partial ambiguous claims. The existing shared-output test was rewritten to model the failed chain as its own declaration and now requires the failed input identity in the public claim.

## Broad regression evidence during development

The first broad unit run after the row redesign reported `280 passed; 6 failed`. All six were stale contract assertions: two builder tests expected failed declarations as generic gaps, one per-chain test constructed an obsolete generated evidence gap, two store tests expected the old generic invariant error, and one renderer test expected a corrupt generated file to remain partial despite recomputation. Updating those tests to the approved one-row contract produced `286 passed; 0 failed` before the final unsafe-path regression increased the unit total to 287.

The first integration rerun produced CLI `5 passed` and E2E `47 passed; 1 failed`; the sole failure expected the old provenance text without the now-required input span. The focused E2E rerun passed:

```text
cargo test --test e2e generated_evidence_chain_joins_provenance_static_calls_and_named_coverage -- --exact --nocapture
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 47 filtered out
```

An early clippy preflight rejected a complex tuple return type. Replacing the tuple with the local `ProvenanceResolution` data structure removed the warning without introducing a trait, factory, or dependency.

## Minimal production changes and affected files

- `src/evidence.rs`: share the existing exact repository-relative artifact path predicate with store validation.
- `src/index.rs`: emit one provenance input for every manifest declaration; parse generated Rust only for one unambiguous inclusion; retain basename contention globally; update focused builder tests.
- `src/store.rs`: schema v8 declaration row, transactional resolution/identity validation, seal/image recomputation, single-source rendering, input-inclusive provenance closure, and owner/range-scoped exact evidence; add focused unit regressions.
- `src/workspace.rs`: invalidate persisted identities whose storage meaning changed.
- `tests/e2e.rs`: assert the public provenance claim's declared input/generator/output spans.
- `docs/superpowers/plans/2026-08-22-dynamic-evidence-graph.md`: retain the approved Task 6 plan and remove the obsolete caller-supplied inclusion locator from the implementation interface.

## Schema, cache, and contract implications

- SQLite schema identity is now 8. `provenance_links` is the sole declaration representation and uses exact declared identity plus store-derived nullable resolution/state. The project intentionally has no migration or compatibility path.
- Evidence image semantics changed from 2 to 3; cache format changed 9 to 10; graph analyzer identity changed 5 to 6; review format changed 5 to 6. Existing incompatible images are rebuilt rather than interpreted under the new meaning.
- Public provenance claims now always include input, generator, and output spans, including failed chains. One declaration yields one claim. Static generated/parser gaps remain separate evidence.
- No design-spec amendment was needed; this implements its layered graph and evidence-scope requirements. The implementation plan now matches the store-derived inclusion-site contract.

## Self-review

- Confirmed failed and complete declarations use the same constrained row and uniqueness identity; there is no parallel declaration table or renderer-only fallback.
- Confirmed mapping identity/state is recomputed inside replacement, sealing, and immutable-image validation, including exact path and artifact-role/span checks.
- Confirmed failed replacement rolls back all new artifacts/declarations and does not alter the prior generation.
- Confirmed graph traversal breadth does not seed exact evidence and that provenance closure includes declared input, generator, output, and inclusion.
- Confirmed generated/static exact gaps require owner/range relevance while global accounting still affects completeness.
- Confirmed deterministic ordering, five independent evidence cursors, cancellation checks, coverage/static separation, and basename ambiguity behavior remain covered.
- Confirmed no dependency, migration, compatibility layer, fallback, HTTP/UI/plugin work, or unrelated minor-review cleanup was introduced.

## Final serial gate

Run from the isolated worktree in the required order:

```text
cargo fmt --check
exit 0; no output

cargo clippy --all-targets -- -D warnings
exit 0; Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.38s

cargo test
exit 0; unit: 287 passed, 0 failed; CLI: 5 passed, 0 failed; E2E: 48 passed, 0 failed (340 passed total)

cargo build --locked --release
exit 0; Finished `release` profile [optimized] target(s) in 11.26s

git diff --check
exit 0; no output

git status --short
exit 0; six expected modified tracked files:
 M docs/superpowers/plans/2026-08-22-dynamic-evidence-graph.md
 M src/evidence.rs
 M src/index.rs
 M src/store.rs
 M src/workspace.rs
 M tests/e2e.rs
```

The report is force-added because `.superpowers/sdd` reports are intentionally ignored. A post-commit `git status --short` is also recorded in the handoff to prove the tracked worktree is clean.

## Post-Task-6 final-review correction

The final reviewer identified two remaining public-evidence defects at clean HEAD `67914e781ed48222f06e78a5237e1c0071bacf45`.

### Additive owner/range gap relevance

Root cause: `CoverageScope::gap_relevant` short-circuited on any non-null `source_id`. A generated node outside `scope.nodes` therefore returned false even when changing a declared input had expanded the exact provenance scope to an overlapping output range.

Mutation caught: restoring owner precedence instead of owner OR range relevance.

```text
cargo test store::tests::provenance_expanded_range_keeps_an_owned_gap_relevant -- --exact --nocapture
RED: exit 101; 0 passed, 1 failed, 288 filtered. The output contained the complete schema.proto -> target/out.rs:1-5 provenance chain but omitted its owned line-3 generated gap.
GREEN: exit 0; 1 passed, 0 failed, 288 filtered.
```

Minimal change: preserve the explicit whole-file fast path, then accept either an exact scoped owner or an overlapping scoped range. No owner is inserted merely to make rendering pass.

### Coverage mapping-gap identity

Root cause: the coverage-gap query sorted by `gap.target_hint` but did not select it. Consequently the renderer could not distinguish unresolved contexts or echo a manifest test name on either the synthetic partial claim or its gap record.

Mutations caught: removing `target_hint` from the SELECT/tuple, omitting `test=` from the partial mapping claim, omitting `target=` from the gap, or failing to escape quotes/backslashes through bounded pagination.

```text
cargo test store::tests::coverage_gap_rendering_preserves_distinct_escaped_test_contexts -- --exact --nocapture
RED: exit 101; 0 passed, 1 failed, 288 filtered. test_a and test_\"b produced indistinguishable partial claims and gaps.
GREEN: exit 0; 1 passed, 0 failed, 288 filtered.

cargo test --test e2e evidence_pagination_is_independent_bounded_and_exhaustive -- --exact --nocapture
RED: exit 101; 0 passed, 1 failed, 47 filtered. The paginated LLVM missing-test claim/gap omitted the escaped manifest test identity.
GREEN: exit 0; 1 passed, 0 failed, 47 filtered.

cargo test store::tests::coverage_mapping_renders_relevant_pathless_manifest_test_gap_reasons -- --exact --nocapture
GREEN contract update: exit 0; 1 passed, 0 failed, 288 filtered.
```

Minimal change: select the already validated stored hint, render it with Rust's escaped debug form as `test=` only for missing/ambiguous test-context partial claims, and render it as `target=` on the corresponding coverage gap. Aggregate run-level observations remain run-level and are not relabeled as verified named-test evidence.

Affected files: `src/store.rs`, `tests/e2e.rs`, this report, and the progress ledger. Storage meaning and cache identity are unchanged; no schema bump, migration, dependency, compatibility path, or design-spec amendment is required.

### Follow-up serial gate

```text
cargo fmt --check
exit 0; no output
cargo clippy --all-targets -- -D warnings
exit 0; Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.41s
cargo test
exit 0; unit: 289 passed, 0 failed; CLI: 5 passed, 0 failed; E2E: 48 passed, 0 failed (342 passed total)
cargo build --locked --release
exit 0; Finished `release` profile [optimized] target(s) in 11.67s
git diff --check
exit 0; no output
git status --short
exit 0; three expected modified tracked files:
 M .superpowers/sdd/2026-08-22-dynamic-evidence-graph/task-6-report.md
 M src/store.rs
 M tests/e2e.rs
```
