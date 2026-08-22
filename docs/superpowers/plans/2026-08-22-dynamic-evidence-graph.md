# Dynamic Evidence Graph Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn Graphr's static change graph into a trustworthy evidence graph
that exposes static-analysis gaps and proves one end-to-end generated-Rust path
from schema input through generated encode/decode code to execution by a named
test.

**Architecture:** Keep the current immutable SQLite graph and `index` workflow.
Add closed, constrained records for gaps, modeled syntax sites, generated
provenance, and coverage observations. An optional manifest binds evidence to a
previous source-only snapshot; the evidence build reuses that exact graph image,
adds verified generated Rust with the existing parser, imports external coverage,
and publishes a new immutable snapshot. `changes` and `view` join static,
provenance, execution, and explicit confidence limits without inventing runtime
call edges.

**Tech Stack:** Rust 2024, existing Tree-sitter parsers, SQLite through
`rusqlite`, existing `blake3`, existing `rmcp::serde_json`, Git CLI, MCP stdio,
LLVM `llvm-cov export` JSON, and Coverage.py JSON. Add no dependency.

**Spec:**
`docs/superpowers/specs/2026-08-22-dynamic-evidence-graph-design.md`.

## Global Constraints

- Execute from a dedicated feature worktree created with
  `superpowers:using-git-worktrees`; do not implement in the planning checkout.
- Use `superpowers:test-driven-development` for every behavior change and
  `superpowers:verification-before-completion` before any success claim.
- Keep one Rust MCP stdio binary. Add no MCP tool, HTTP endpoint, UI, editor
  integration, plugin protocol, migration, compatibility alias, or fallback.
- Keep Rust, Python, JavaScript/JSX, and TypeScript/TSX static support. This
  milestone imports runtime coverage only for Rust and Python.
- Do not execute generators, builds, tests, coverage tools, or repository code.
  Graphr consumes files produced outside the process.
- Treat an explicit evidence manifest as a trust-boundary input: unsafe paths,
  invalid versions, oversized files, symlinks, tracked evidence outputs, digest
  mismatches, invalid spans, duplicate run identities, and source-snapshot
  mismatches are fatal.
- Keep all output deterministic, line-safe, compact, bounded, and independently
  paged. Never echo unsafe path bytes or report-internal external absolute paths.
- Preserve immutable publication, exact cache identity, transactional rollback,
  cancellation, descriptor-relative capture, SQLite foreign keys, and sealed
  image validation.
- Do not claim that coverage proves caller/callee ordering, assertion intent,
  reachability, or universal non-execution. A positive count proves execution in
  one declared run; zero means not observed in that run; absent mapping is
  unknown.
- Keep static `TEST_CALLS` heuristic evidence separate from named-test execution.
- No backwards compatibility: remove `analysis_complete`, `review_complete`,
  `review_complete_when_pages_exhausted`, and broad
  `coverage status=complete` output when their replacements land.
- Add no generic artifact framework, evidence registry, trait with one
  implementation, configuration system, mutation importer, runtime-trace
  importer, shell CFG, trust-role inference, requirement mapping, or before/after
  query in this milestone.
- Commit only after each vertical slice builds and its focused tests pass. Do not
  push.
- Before completion run, in order:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
```

## Final Public Contract

The existing CLI and MCP `index` inputs gain one optional safe relative path:

```text
evidence_manifest: optional relative path beneath worktree_root
```

The v1 JSON manifest is closed with `deny_unknown_fields` and has this shape:

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

Bounds are fixed in v1:

```text
manifest                         64 KiB
each input/generated artifact    2 MiB
each coverage report            64 MiB
generated mappings              64
coverage reports                8
all unique evidence bytes       128 MiB
run_label/test_name              200 bytes, line-safe
path                             existing repository path bound
```

The final `changes` preamble uses these status words only:

```text
complete | partial | not-applicable
```

It emits these independent dimensions and claims:

```text
completeness content_capture=... source_capture=... syntax_parse=...
  site_classification=... static_model=... evidence_capture=...
  provenance_model=... execution_mapping=... traversal=...
claim kind=affected-callers status=... basis=resolved-static-call-graph
claim kind=affected-flows status=... basis=resolved-static-call-graph
claim kind=static-test-paths status=... basis=resolved-static-call-graph
claim kind=generated-provenance ... result=linked|unknown
  basis=verified-generated-manifest
claim kind=changed-execution ... result=observed|not-observed|unknown
  basis=llvm-coverage-json|coverage-py-json
```

Every initial and continuation page repeats:

```text
content_complete_when_pages_exhausted=true|false
static_evidence_status=complete|partial|not-applicable
dynamic_evidence_status=complete|partial|not-applicable
```

The cursor set is `files`, `diff`, `artifacts`, `graph`, and `evidence`. Static
edges, provenance links, observations, branches, and gaps retain distinct output
tokens.

## File Responsibilities

- `src/store.rs`: own final SQLite schema, closed graph/evidence enums and input
  rows, resolution state, relation-site invariant, evidence replacement,
  completeness calculation, `changes` joins, and `view` evidence.
- `src/parse.rs` and `queries/rust.scm`: own Rust syntax capture, macro/include
  classification, outermost parse-error ranges, and generated Rust parsing
  context.
- `src/python.rs` and `queries/python.scm`: own Python call/import site
  classification and parse-error ranges.
- `src/javascript.rs`, `queries/ecmascript.scm`, `queries/jsx.scm`, and
  `queries/typescript.scm`: own JavaScript/TypeScript call, constructor, JSX,
  module, export, and test-registration classification.
- `src/git.rs`: own source omission inventory, requested input-artifact capture,
  evidence-only exclusion from worktree comparison, and tracked-path rejection.
- `src/evidence.rs`: own the one v1 manifest type, safe manifest/evidence capture,
  canonical ordering, digest/span/text validation, and evidence graph identity
  input. It must not execute producers.
- `src/coverage.rs`: own direct parsing of LLVM and Coverage.py JSON into one
  small internal observation representation. It must not resolve graph nodes or
  open paths named by a report.
- `src/index.rs`: orchestrate source/evidence snapshot builds, map generated
  inclusion context, reuse the existing Rust parser, build review sections, and
  page evidence.
- `src/workspace.rs`: carry optional manifest selection through resolved requests,
  snapshot manifests, cache keys, publication, and exposed provenance.
- `src/mcp.rs` and `src/main.rs`: expose the optional input and update deterministic
  workflow guidance without adding a tool.
- `tests/e2e.rs`: prove public MCP/CLI behavior, rollback, pagination, cache
  identity, and the generated-code acceptance chain.
- `README.md`: document the external-producer workflow, evidence limits, and
  exact claim semantics after all behavior is green.

---

### Task 1: Ship trustworthy static accounting as a working slice

**Files:**

- Modify: `src/store.rs`
- Modify: `src/git.rs`
- Modify: `src/index.rs`
- Modify: `src/workspace.rs`
- Modify: `src/parse.rs`
- Modify: `src/python.rs`
- Modify: `src/javascript.rs`
- Modify: `src/mcp.rs`
- Modify: `queries/rust.scm`
- Modify: `queries/python.scm`
- Modify: `queries/ecmascript.scm`
- Modify: `queries/jsx.scm`
- Modify: `queries/typescript.scm`
- Modify: `tests/e2e.rs`

**Interfaces:**

Add closed store types with stable lowercase database/output tokens:

```rust
pub enum ResolutionState { Pending, Resolved, Missing, Ambiguous }

pub enum GapCategory {
    Source, Parse, Relation, Macro, Generated, Coverage, Language, Boundary,
}

pub enum GapReason {
    UnsafePath,
    NonRegular,
    Unmerged,
    Oversized,
    InvalidUtf8,
    MissingDuringRead,
    ParserError,
    ParserNoTree,
    DynamicOrUnsupportedDispatch,
    MacroExpansionUnavailable,
    GeneratedOutputUnobserved,
    GeneratedOutputAmbiguous,
    ExternalDependency,
    DependencyCollapsed,
    LanguageNotIndexed,
    CoverageUnmappedFile,
    CoverageUnmappedRegion,
    MissingTestContext,
    AmbiguousTestContext,
}

pub enum ModeledSiteKind {
    GeneratedInclusion,
    TestRegistration,
    StaticExport,
}
```

Extend graph inputs without adding a generic property map:

```rust
pub struct FileInput {
    pub path: String,
    pub language: Language,
    pub git_oid: Option<String>,
    pub content_hash: [u8; 32],
    pub parse_context: String,
    pub byte_size: u64,
    pub replace: bool,
    pub observed_relation_sites: u32,
}

pub struct RefInput {
    pub source_key: String,
    pub kind: RefKind,
    pub line: u32,
    pub keys: Vec<String>,
    pub alias_key: Option<String>,
    pub resolved_target_key: Option<String>,
    pub resolution: ResolutionState,
}

pub struct ModeledSiteInput {
    pub file_key: String,
    pub source_key: Option<String>,
    pub kind: ModeledSiteKind,
    pub line_start: u32,
    pub line_end: u32,
    pub target_hint: Option<String>,
    pub parse_context: Option<String>,
}

pub struct GapInput {
    pub file_key: Option<String>,
    pub source_key: Option<String>,
    pub run_key: Option<String>,
    pub path: Option<String>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub category: GapCategory,
    pub reason: GapReason,
    pub target_hint: Option<String>,
    pub occurrences: u32,
    pub relation_site: bool,
}

pub struct Graph {
    pub files: Vec<FileInput>,
    pub nodes: Vec<NodeInput>,
    pub refs: Vec<RefInput>,
    pub trait_implementations: Vec<TraitImplementationInput>,
    pub edges: Vec<EdgeInput>,
    pub modeled_sites: Vec<ModeledSiteInput>,
    pub gaps: Vec<GapInput>,
}
```

Replace nullable-only reference resolution with the invariant:

```text
resolved  <=> resolved_target_id IS NOT NULL
pending   => allowed only inside the indexing transaction
missing   => no direct or alias candidate
ambiguous => more than one candidate or conflicting direct/alias candidate
```

Persist final milestone tables now so schema/cache versions change once:

```text
files.observed_relation_sites
refs.resolution_state
modeled_sites
imported_artifacts
provenance_links
coverage_runs
coverage_regions
coverage_branches
graph_gaps
```

Use foreign keys and closed `CHECK` constraints. `coverage_regions` and
`coverage_branches` include nullable `test_id` because Coverage.py can map
different contexts in one report. `graph_gaps.run_id` owns coverage-import gaps.
Do not add JSON columns.

**Steps:**

- [ ] **Step 1: Establish the clean baseline**

From the feature worktree, record the merge base and run the existing gate before
editing:

```bash
git status --short
git rev-parse HEAD
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

Stop and diagnose any baseline failure before attributing it to this work.

- [ ] **Step 2: Write RED store tests for resolution and accounting**

In `src/store.rs`, add focused tests that create small in-memory `Graph` values
and assert:

1. one candidate seals as `resolved` and owns one edge;
2. no candidate seals as `missing` and owns no edge;
3. two candidates seal as `ambiguous` and own no edge;
4. adding/removing a candidate through incremental indexing changes
   `missing -> resolved -> ambiguous -> resolved` deterministically;
5. a sealed `pending` row is rejected;
6. a `resolved` row with no target and a non-resolved row with a target are
   rejected;
7. identical gaps fold by incrementing `occurrences`;
8. file replacement cascades file-owned modeled sites and gaps;
9. publication rolls back when
   `observed_relation_sites != refs + modeled_sites + relation_site_gaps` for a
   file.

Run only the new tests and confirm RED:

```bash
cargo test store::tests::resolution_state_
cargo test store::tests::relation_site_accounting_
cargo test store::tests::gap_ownership_
```

- [ ] **Step 3: Implement the constrained store model**

In `src/store.rs`:

1. add the enums and input structs above with explicit `db()`/`parse()` methods;
2. extend `Graph::default`, merge logic, full insert, and incremental insert;
3. change in-memory `reference_target` and SQLite candidate resolution to return
   `Resolved(id)`, `Missing`, or `Ambiguous` rather than `Option<id>`;
4. insert new refs as `pending`, update target and state together, and derive
   edges only for `resolved` rows;
5. add `require_graph_invariants` and call it before transaction commit, from
   `seal`, and from read-only image validation;
6. fold gaps in deterministic path/range/category/reason/hint order;
7. keep global gaps when a source file is absent and use `ON DELETE CASCADE` for
   file/run-owned rows;
8. create the final constrained evidence tables listed in **Interfaces**, even
   though later tasks are their first producers;
9. bump `SCHEMA_VERSION`, `CACHE_FORMAT_VERSION`,
   `GRAPH_ANALYZER_VERSION`, and `REVIEW_FORMAT_VERSION` once; reject prior
   images with the existing version check and add no migration.

Set `observed_relation_sites` in each language adapter at the point it emits a
reference, modeled site, or relation-site gap. Do not derive it later from
database rows or weaken the store invariant while the parser queries expand.

Run:

```bash
cargo test store::tests::resolution_state_
cargo test store::tests::relation_site_accounting_
cargo test store::tests::gap_ownership_
cargo test store::tests::incremental_
```

- [ ] **Step 4: Write RED source-inventory tests**

In `src/git.rs`, add tests for a target containing:

- an oversized supported source;
- invalid UTF-8 source bytes;
- an unmerged supported source;
- a path that disappears safely during inventory capture;
- one unsafe path byte sequence;
- one `.go` file;
- one ordinary non-source text file.

Assert that safe omissions retain path/reason, unsafe bytes only increment an
aggregate occurrence, `.go` becomes `language/not-indexed`, ordinary text does
not become a language gap, and omission changes alter the graph image key. Add
an index-level rollback test for concurrent mutation/digest mismatch; that case
must remain fatal rather than becoming a gap.

Run and confirm RED:

```bash
cargo test git::tests::source_omission_
cargo test workspace::tests::graph_image_key_
```

- [ ] **Step 5: Retain source omissions through graph publication**

In `src/git.rs`, replace `TargetInventory.skipped` and
`SourceSnapshot.skipped` as the source of truth with:

```rust
pub enum SourceOmissionReason {
    UnsafePath,
    NonRegular,
    Unmerged,
    Oversized,
    InvalidUtf8,
    MissingDuringRead,
    LanguageNotIndexed,
}

pub struct SourceOmission {
    pub path: Option<String>,
    pub reason: SourceOmissionReason,
    pub occurrences: u32,
}

pub struct SourceSnapshot {
    pub capture_root: PathBuf,
    pub files: Vec<CapturedSource>,
    pub omissions: Vec<SourceOmission>,
}
```

Keep `IndexStats.files_skipped` as a derived sum for compact operational stats.
Classify safe expected omissions, sort/fold them, and preserve fatal capture
failures. In `src/workspace.rs`, hash the full ordered omission inventory in
`graph_image_key`. In `src/index.rs`, convert every source omission and every
invalid-UTF-8 parser input into a global `GapInput`; a missing Git blob or digest
change remains fatal.

Run:

```bash
cargo test git::tests::source_omission_
cargo test workspace::tests::graph_image_key_
cargo test index::tests::source_gap_
```

- [ ] **Step 6: Write RED Rust and Python site-classification tests**

Change the Rust and Python parser tests before changing queries. Cover:

```text
Rust: direct identifier, scoped call, self/identifier method, computed callee,
      closure call, macro invocation, static OUT_DIR include!, malformed syntax
Python: bare call, attribute call, subscript/computed call, import, malformed syntax
```

Assert one outcome per observed relation slot:

```text
reference | modeled_site | relation_site_gap
```

Assert outermost Tree-sitter `ERROR` ranges and uncovered `MISSING` nodes are
sorted/deduplicated parse gaps. A parser with no tree yields one whole-file
`parser-no-tree` gap. Match-limit exhaustion remains fatal.

Run and confirm RED:

```bash
cargo test parse::tests::classifies_
cargo test python::tests::classifies_
cargo test parse::tests::reports_parse_
cargo test python::tests::reports_parse_
```

- [ ] **Step 7: Classify every Rust and Python relation slot**

In `queries/rust.scm`, capture the whole `call_expression` and every
`macro_invocation`; in `queries/python.scm`, capture every `call`. In
`src/parse.rs`, add one shared outermost error-range walker and use it from all
three language parsers.

In the Rust index adapter:

- keep current supported call normalization;
- emit `dynamic-or-unsupported-dispatch` for unsupported call shapes instead of
  dropping them;
- emit a ref with exact `missing`/`ambiguous` resolution for supported targets;
- emit `macro-expansion-unavailable` for unexpanded macros;
- recognize only the literal
  `include!(concat!(env!("OUT_DIR"), "/name.rs"))` shape as a
  `generated-inclusion` modeled site;
- store the output basename as `target_hint` and the exact lexical Rust
  `TargetPath::parse_context()` for later generated parsing;
- add a non-site `generated-output-unobserved` gap until evidence links it.

In Python, keep bare identifier refs and turn every other call target into a
`dynamic-or-unsupported-dispatch` gap. Mark imports whose syntax names an
external top-level package as `boundary/external-dependency` without pretending
they resolve locally.

Set `FileInput.observed_relation_sites` from the parser's classified outcome
count, not from post-hoc graph row counts. Run:

```bash
cargo test parse::tests
cargo test python::tests
cargo test index::tests::rust_
cargo test index::tests::python_
```

- [ ] **Step 8: Write RED JavaScript/TypeScript accounting tests**

In `src/javascript.rs`, add one compact table-driven test across `.js`, `.jsx`,
`.ts`, and `.tsx` for:

- direct calls and constructors;
- supported object/member and `this` calls;
- computed/nested call targets;
- supported and unsupported JSX targets;
- ESM imports, side-effect imports, re-exports, `export *`, and CommonJS
  `require`;
- local static exports whose relationship is represented by stable symbol keys;
- recognized and shadowed `test`/`it` registrations;
- malformed syntax.

Assert that every relation slot produces refs, a `test-registration` or
`static-export` modeled site, or a relation gap, and that the file accounting
equation holds. Run and confirm RED:

```bash
cargo test javascript::tests::classifies_all_relation_sites
cargo test javascript::tests::reports_parse_gaps
```

- [ ] **Step 9: Close JavaScript/TypeScript silent drops**

Retain all relevant `@call`, `@jsx`, and module captures in `ParsedFile`, even
when `call()`, `jsx_target()`, `relative_module()`, binding lookup, or export
lookup cannot produce keys. Classify:

- recognized test registration as `test-registration`;
- local export represented by node keys as `static-export`;
- supported call/import/re-export/require as refs;
- computed/nested/member/JSX or unclassifiable module targets as exact relation
  gaps;
- external package imports as explicit boundary gaps;
- parse error ranges with the shared walker.

Remove the current `keys.is_empty() { continue; }` and equivalent silent skips.
Do not add a JavaScript runtime coverage importer. Run:

```bash
cargo test javascript::tests
cargo test index::tests::script_
```

- [ ] **Step 10: Write RED completeness/output tests**

Update unit and E2E assertions first to require:

- `analysis_complete` renamed to `traversal_complete`;
- no `review_complete` field;
- `content_complete_when_pages_exhausted` based only on exact changed content;
- `static_evidence_status` independent of page exhaustion;
- `dynamic_evidence_status=not-applicable` without a manifest;
- all nine completeness dimensions;
- claim-specific status/basis lines;
- `languages=rust,python,javascript,typescript`;
- missing/ambiguous counts and compact ordered gap summaries;
- a completed traversal over a macro/dynamic/parse gap still reporting static
  evidence as partial;
- a direct-call-only repository reporting complete static evidence.

Delete test helpers that parse removed names; do not retain aliases. Run and
confirm RED:

```bash
cargo test store::tests::completeness_
cargo test index::tests::review_status_
cargo test --test e2e completeness
```

- [ ] **Step 11: Implement static completeness and renamed review status**

Add a small value type, not a framework:

```rust
pub enum CompletenessStatus { Complete, Partial, NotApplicable }

pub struct ChangeReview {
    pub graph: String,
    pub evidence: String,
    pub static_status: CompletenessStatus,
    pub dynamic_status: CompletenessStatus,
}
```

Make `Store::changes` return `ChangeReview`. Compute static dimensions from the
complete gap/ref inventory plus the existing mapping/traversal limits. Use the
approved conservative relevance rule: repository-wide source/parse omissions
and broad macro/dynamic gaps make affected static claims partial; keyed missing
or ambiguous refs matter when their keys can name a changed or traversed node.

In `src/index.rs`, stop inferring review completeness from a broad graph string.
Carry typed statuses into `ReviewSnapshot`, rename page metadata exactly, and
repeat the three terminal status fields on every response. Replace generic
`coverage` diagnostic lines with dimension/claim lines. Update MCP guidance and
all E2E parsers to the new names.

Run the slice gate:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
```

- [ ] **Step 12: Commit static assurance slice**

Review the diff for compatibility aliases, unbounded output, and silent parser
`continue` branches, then commit:

```bash
git add src queries tests/e2e.rs
git commit -m "feat: report trustworthy graph completeness"
```

---

### Task 2: Ship hash-verified generated-artifact provenance

**Files:**

- Create: `src/evidence.rs`
- Modify: `src/main.rs`
- Modify: `src/mcp.rs`
- Modify: `src/workspace.rs`
- Modify: `src/git.rs`
- Modify: `src/index.rs`
- Modify: `src/store.rs`
- Modify: `tests/e2e.rs`

**Interfaces:**

`src/evidence.rs` exposes concrete data only:

```rust
pub const MANIFEST_LIMIT: u64 = 64 * 1024;
pub const ARTIFACT_LIMIT: u64 = 2 * 1024 * 1024;
pub const COVERAGE_LIMIT: u64 = 64 * 1024 * 1024;
pub const GENERATED_LIMIT: usize = 64;
pub const COVERAGE_REPORT_LIMIT: usize = 8;
pub const EVIDENCE_TOTAL_LIMIT: u64 = 128 * 1024 * 1024;

pub enum CoverageFormat { Llvm, CoveragePy }

pub struct SourceSpan {
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
}

pub struct CapturedArtifact {
    pub path: String,
    pub content_hash: [u8; 32],
    pub bytes: Vec<u8>,
}

pub struct CapturedArtifactSpan {
    pub artifact: CapturedArtifact,
    pub line_start: u32,
    pub line_end: u32,
}

pub struct CapturedEvidence {
    pub source_snapshot_id: String,
    pub manifest: CapturedArtifact,
    pub generated: Vec<CapturedGenerated>,
    pub coverage: Vec<CapturedCoverage>,
}

pub struct CapturedGenerated {
    pub input: CapturedArtifactSpan,
    pub generator: SourceSpan,
    pub output: CapturedArtifactSpan,
}

pub struct CapturedCoverage {
    pub format: CoverageFormat,
    pub report: CapturedArtifact,
    pub run_label: String,
    pub test_name: Option<String>,
}
```

Use private `serde::Deserialize` manifest structs with
`#[serde(deny_unknown_fields)]`; do not expose a registry or trait.

Extend request and provenance types:

```rust
pub struct IndexRequest {
    pub worktree_root: PathBuf,
    pub base_ref: String,
    pub head_ref: String,
    pub target: SnapshotTarget,
    pub dependency_mode: DependencyMode,
    pub evidence_manifest: Option<PathBuf>,
}

pub struct ResolvedIndexRequest {
    pub root: RootIdentity,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub target: SnapshotTarget,
    pub dependency_mode: DependencyMode,
    pub evidence_manifest: Option<PathBuf>,
}

pub struct Provenance {
    pub repository_id: String,
    pub workspace_id: String,
    pub snapshot_id: String,
    pub common_git_dir: PathBuf,
    pub git_dir: PathBuf,
    pub repository_root: PathBuf,
    pub worktree_root: PathBuf,
    pub branch: Option<String>,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub target_state: SnapshotTarget,
    pub selected_layers: Vec<ChangeLayer>,
    pub dirty_digest: String,
    pub commits_base_to_head: u64,
    pub changed_files: usize,
    pub index_generation: i64,
    pub source_snapshot_id: Option<String>,
    pub evidence_manifest_digest: Option<String>,
}
```

Add one store transaction for evidence, reusing current incremental insertion:

```rust
pub enum ArtifactRole { Manifest, Input, GeneratedRust, CoverageReport }

pub enum CoverageBranchKind { TrueOutcome, FalseOutcome, Arc }

pub struct EvidenceLineSpan {
    pub start: u32,
    pub end: u32,
}

pub struct ModeledSiteLocator {
    pub path: String,
    pub line: u32,
    pub kind: ModeledSiteKind,
    pub target_hint: Option<String>,
}

pub struct ImportedArtifactInput {
    pub key: String,
    pub path: String,
    pub role: ArtifactRole,
    pub content_hash: [u8; 32],
    pub byte_size: u64,
}

pub struct ProvenanceInput {
    pub input_key: String,
    pub input_lines: EvidenceLineSpan,
    pub generator_path: String,
    pub generator_lines: EvidenceLineSpan,
    pub output_key: String,
    pub output_lines: EvidenceLineSpan,
    pub inclusion_site: Option<ModeledSiteLocator>,
}

pub struct CoverageRunInput {
    pub key: String,
    pub format: CoverageFormat,
    pub report_key: String,
    pub run_label: String,
    pub test_name: Option<String>,
}

pub struct CoverageRegionInput {
    pub run_key: String,
    pub path: Option<String>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub execution_count: u64,
    pub context: Option<String>,
}

pub struct CoverageBranchInput {
    pub run_key: String,
    pub path: Option<String>,
    pub start_line: i64,
    pub start_column: u32,
    pub end_line: i64,
    pub end_column: u32,
    pub target_line: Option<i64>,
    pub kind: CoverageBranchKind,
    pub execution_count: u64,
}

pub struct EvidenceInput {
    pub artifacts: Vec<ImportedArtifactInput>,
    pub provenance: Vec<ProvenanceInput>,
    pub runs: Vec<CoverageRunInput>,
    pub regions: Vec<CoverageRegionInput>,
    pub branches: Vec<CoverageBranchInput>,
    pub gaps: Vec<GapInput>,
}

pub struct EvidenceStats {
    pub generated_files: usize,
    pub artifacts: usize,
    pub provenance_links: usize,
    pub runs: usize,
    pub regions: usize,
    pub branches: usize,
    pub gaps: usize,
}

pub fn replace_evidence(
    &mut self,
    generated_graph: Graph,
    evidence: &EvidenceInput,
    cancelled: &AtomicBool,
) -> Result<EvidenceStats>
```

It clears prior evidence rows, synthesizes unchanged `FileInput` rows from the
source seed, adds generated files as replacements, calls the current incremental
resolver, inserts exact artifact/provenance rows, validates evidence invariants,
increments generation once, and commits once. It never mutates the published
source snapshot.

**Steps:**

- [ ] **Step 1: Write RED public-input and manifest validation tests**

Add CLI, MCP schema, unit, and E2E tests for:

- optional `--evidence-manifest RELATIVE_PATH` and MCP
  `evidence_manifest` input;
- duplicate option, absolute path, `..`, empty component, non-UTF-8 path, and
  overlong path rejection;
- unknown manifest fields and unsupported `format_version` rejection;
- 64 KiB inclusive bound and one-byte-over rejection;
- symlink, directory, missing file, replacement-during-read, and unsafe file
  rejection;
- invalid digest, span, label, test name, duplicate generated link, and duplicate
  run identity rejection;
- generated/report files at the inclusive bounds and one byte over;
- generated/report entry-count and aggregate captured-byte bounds;
- every explicit output/report path being untracked or ignored, never tracked;
- the manifest itself being untracked or ignored, never tracked;
- failure publishing no new snapshot and preserving the source snapshot.

Run and confirm RED:

```bash
cargo test evidence::tests
cargo test main::tests::parses_evidence_manifest
cargo test mcp::tests::index_schema_includes_evidence_manifest
cargo test --test e2e evidence_manifest_validation
```

- [ ] **Step 2: Implement safe manifest and artifact capture**

Create `src/evidence.rs` and add `mod evidence` in `src/main.rs`. Reuse the
existing descriptor-relative `O_NOFOLLOW`/regular-file capture path from
`src/git.rs`; expose a `pub(crate)` bounded helper instead of reimplementing path
opening. Capture bytes, `fstat` before/after, and BLAKE3 before parsing or using
any declaration.

Normalize, sort, and deduplicate manifest entries. Validate spans against line
counts after exact bytes are captured. Bound and line-sanitize labels/test names.
Return a typed `CapturedEvidence`; do not retain raw JSON `Value` objects.
Until Task 3, a non-empty `coverage` array is rejected as unsupported explicit
evidence; it is never ignored or published as partial provenance.

Add `Repository::reject_tracked_evidence_paths` using the current captured Git
index and `git ls-files`. Ignored paths are allowed; tracked paths are fatal.
Pass a sorted evidence-only exclusion set containing the manifest, generated
outputs, and coverage reports into worktree untracked inventory, review capture,
and `target_dirty_digest`. Never exclude manifest input or generator source.

Run:

```bash
cargo test evidence::tests
cargo test git::tests::evidence_exclusion_
cargo test main::tests::parses_evidence_manifest
cargo test mcp::tests::index_schema_includes_evidence_manifest
```

- [ ] **Step 3: Prove exact source-snapshot binding**

Extend `Repository::capture_snapshot` with two explicit inputs:

```rust
requested_artifact_paths: &BTreeSet<String>
evidence_only_paths: &BTreeSet<String>
```

Capture each declared input artifact from the selected commit/index/worktree
state, not blindly from the live filesystem. Store its exact bytes in the
private capture directory and verify its manifest digest. Keep these requested
artifact bytes out of the static source graph.

After exclusions, recompute the ordinary source graph image ID, review ID,
dirty digest, and `snapshot_key` with no evidence fields. Require exact equality
with `source_snapshot_id`, and require matching repository/workspace, base/head
OIDs, target, and dependency mode. Reject evidence-on-evidence layering by
requiring the source entry's `evidence_manifest_digest` to be `None`.

Add tests that independently change a source file, tracked input artifact,
base/head, target, dependency mode, or non-evidence untracked source after the
source snapshot. Each must fail as `source snapshot mismatch`; creating only the
declared evidence files must pass.

Run:

```bash
cargo test index::tests::evidence_source_snapshot_
cargo test git::tests::requested_artifact_
cargo test --test e2e evidence_source_snapshot
```

- [ ] **Step 4: Add evidence image identity and exact source seeding**

Add one `evidence_graph_image_key` beside `graph_image_key`. Hash, in tagged
length-delimited order:

```text
domain/version
source graph image ID
source snapshot ID
manifest digest
each captured artifact role/path/digest/size
evidence semantics version
schema/analyzer/cache versions
```

When no manifest is supplied, preserve the ordinary build path. With evidence:

1. try exact evidence image reuse;
2. otherwise validate and copy only the manifest's source snapshot graph as the
   seed;
3. never select an arbitrary newer cache seed;
4. build all evidence in the private copy;
5. publish the evidence image/review/snapshot through the current no-replace
   catalog path.

Include `source_snapshot_id` and manifest digest in snapshot identity and
published provenance. Add exact-repeat reuse, one-digest-change invalidation,
cancelled-build rollback, and corrupt-source-seed quarantine tests.

Run:

```bash
cargo test workspace::tests::evidence_graph_image_key_
cargo test index::tests::evidence_cache_
cargo test --test e2e evidence_cache
```

- [ ] **Step 5: Write RED provenance and generated-include tests**

Build focused fixtures for:

- `.proto` and TSV input spans retained as imported artifacts;
- generator span mapping to exactly one Rust function and one Python function;
- missing/ambiguous generator node;
- one unique static `OUT_DIR` include candidate;
- no include candidate;
- two include candidates for the same output basename;
- generated Rust whose output path collides with a captured repository file;
- generated Rust containing direct calls that resolve to a hand-written
  predicate;
- generated Rust parse gaps;
- corrupt input/output digest and invalid output span.

Assert unique provenance rows, no context-free generated nodes for zero/multiple
includes, and complete rollback on fatal manifest errors. Run and confirm RED:

```bash
cargo test store::tests::provenance_
cargo test index::tests::generated_rust_
cargo test --test e2e generated_provenance
```

- [ ] **Step 6: Parse uniquely included generated Rust in the existing graph**

Add a bounded read query on the private source seed that returns modeled
`generated-inclusion` candidates by exact output basename with:

```text
site id, containing file/node, line, target hint, Rust parse_context
```

For one candidate, construct a normal `Source` from the already captured output
bytes and call the existing `add_rust_file` with
`TargetPath::from_parse_context`. Set `git_oid=None`; retain the manifest path and
digest as the generated file identity. Do not create a second parser or a
generated-code resolver.

For zero candidates, retain artifact/provenance attestation and
`generated-output-unobserved`; for multiple candidates, replace it with
`generated-output-ambiguous`. In both cases skip static generated nodes. For one
candidate, remove the unobserved gap, link the inclusion site, insert the
generated graph plus imported artifacts/provenance in one `replace_evidence`
transaction, and re-resolve both new and previously missing keys.

Map each generator span to exactly one Rust/Python node. Insert no provenance
link when mapping is missing or ambiguous; record a generated gap and make the
claim partial. A malformed explicit manifest or digest remains fatal.

- [ ] **Step 7: Render the provenance vertical slice**

Teach `Store::changes` and `Store::view` to emit bounded, sorted records:

```text
claim kind=generated-provenance ... status=complete|partial result=linked|unknown basis=verified-generated-manifest
provenance input="path:start-end" generator="path:start-end" output="path:start-end"
includes source="path:line" output="path"
gap category=generated reason=... path="..." line=... occurrences=...
```

Set `evidence_capture`, `provenance_model`, and
`dynamic_evidence_status` from stored evidence; leave `execution_mapping` and
changed-execution `not-applicable` while `coverage=[]`. Keep static calls from
generated encode/decode visible as ordinary resolved graph edges.

Run the slice gate:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
```

- [ ] **Step 8: Commit generated provenance slice**

```bash
git add src tests/e2e.rs
git commit -m "feat: trace generated Rust provenance"
```

---

### Task 3: Import real Rust and Python execution evidence

**Files:**

- Create: `src/coverage.rs`
- Modify: `src/main.rs`
- Modify: `src/evidence.rs`
- Modify: `src/index.rs`
- Modify: `src/store.rs`
- Modify: `tests/e2e.rs`

**Interfaces:**

`src/coverage.rs` reuses `evidence::CoverageFormat` and produces concrete
normalized observations:

```rust
pub struct CoverageObservation {
    pub path: Option<String>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub execution_count: u64,
    pub context: Option<String>,
}

pub enum BranchObservationKind { TrueOutcome, FalseOutcome, Arc }

pub struct BranchObservation {
    pub path: Option<String>,
    pub start_line: i64,
    pub start_column: u32,
    pub end_line: i64,
    pub end_column: u32,
    pub target_line: Option<i64>,
    pub kind: BranchObservationKind,
    pub execution_count: u64,
    pub context: Option<String>,
}

pub struct ParsedCoverage {
    pub format: CoverageFormat,
    pub regions: Vec<CoverageObservation>,
    pub branches: Vec<BranchObservation>,
    pub external_paths: u32,
}
```

Report parsers receive captured bytes plus the authorized canonical worktree
path only for lexical path normalization. They never open report filenames.

**Steps:**

- [ ] **Step 1: Write RED LLVM importer tests from documented JSON shapes**

Use inline minimal JSON fixtures in `src/coverage.rs` for:

- exact `type=llvm.coverage.json.export` with version major 2 or 3;
- positive and zero-count `data[].functions[].regions` tuples;
- positive and zero-count branch outcomes;
- multiple `data[].files[]` blocks with deterministic folding;
- relative filename, authorized absolute filename, and external absolute
  filename;
- malformed tuple lengths/types/counts/ranges;
- unsupported major version;
- summary-only JSON with no regions.

Assert external absolute paths increment an aggregate boundary and are not
retained verbatim. Run and confirm RED:

```bash
cargo test coverage::tests::llvm_
```

- [ ] **Step 2: Implement the minimal LLVM JSON decoder**

Use `rmcp::serde_json::Value` only at the documented tuple boundary; immediately
validate and convert each tuple to the typed structs above. Consume function
filenames plus eight-field region tuples
`[line_start,column_start,line_end,column_end,count,file_id,expanded_file_id,kind]`
and file-level nine-field branch tuples with separate true/false counts. Emit
one `TrueOutcome` and one `FalseOutcome` per branch tuple. Accept only the exact
top-level type and version majors 2 or 3, require region-level data, use checked
integer conversions, normalize paths lexically, sort/deduplicate exact records,
and sum duplicate counts with `checked_add`.

Do not use coverage function names as symbol identity. Do not create call edges.
Run:

```bash
cargo test coverage::tests::llvm_
```

- [ ] **Step 3: Write RED Coverage.py importer tests**

Use inline JSON for:

- `meta.format=3` with arbitrary bounded `meta.version` package text;
- `executed_lines` and `missing_lines`;
- `executed_branches` and `missing_branches`;
- `contexts` mapping line strings to one, multiple, empty, and missing context;
- reports without contexts;
- authorized relative/absolute and external paths;
- malformed line/arc/context values and summary-only files.

Assert branch arcs remain run-scoped because Coverage.py does not bind branch
arcs to contexts. Run and confirm RED:

```bash
cargo test coverage::tests::coverage_py_
```

- [ ] **Step 4: Implement Coverage.py decoding**

Require Coverage.py JSON `meta.format=3`. Turn executed/missing lines into
one-line regions with counts `1`/`0`; turn executed/missing arcs into `Arc`
branch observations with counts `1`/`0`, column zero, and `target_line=Some(...)`.
Attach contexts only to line regions. Preserve run-level observations when
contexts are absent. Sort/deduplicate exactly as for LLVM and reject other
format values or reports without executable line/region data.

Run:

```bash
cargo test coverage::tests
```

- [ ] **Step 5: Write RED graph-mapping and claim tests**

In `src/store.rs` and `tests/e2e.rs`, assert:

1. coverage maps only to a captured hand-written or uniquely included generated
   file;
2. an unmapped path/region produces a run-owned coverage gap;
3. LLVM with one manifest `test_name` maps every observation to one unique static
   test;
4. missing/ambiguous manifest test names produce exact mapping gaps and remain
   run-level;
5. Coverage.py line contexts map independently to unique tests;
6. Coverage.py branches remain run-level even when line contexts map;
7. aggregate coverage never becomes named-test evidence;
8. positive count renders `observed`, zero executable count renders
   `not-observed`, and missing/unmapped evidence renders `unknown`;
9. one run's zero count never removes or relabels static `TEST_CALLS` evidence;
10. duplicate `(format, run_label, report digest)` identities abort and roll
    back.

Run and confirm RED:

```bash
cargo test store::tests::coverage_mapping_
cargo test store::tests::changed_execution_claim_
cargo test --test e2e coverage_evidence
```

- [ ] **Step 6: Store and map coverage in the evidence transaction**

Parse every captured report before opening the SQLite transaction. In
`replace_evidence`, insert the report artifact and run, then map each normalized
path to one `files` row. Map regions to overlapping nodes by source range, but
retain the file/range observation even when no symbol overlaps. Resolve test
names/contexts against `kind='test'` nodes with exact unique matching; create
coverage gaps for missing/ambiguous mappings.

Use `u64` in Rust and checked conversion to SQLite signed integers. Enforce
unique run/region/branch rows and evidence ownership in
`require_graph_invariants`. A parse or insertion failure rolls back the generated
graph and all evidence together.

- [ ] **Step 7: Render scoped observations and dynamic claims**

For relevant changed/generated ranges, render in deterministic run/path/range
order:

```text
claim kind=changed-execution ... status=complete|partial result=observed|not-observed|unknown basis=... run="..." test="..."
observed run="..." test="..." path="..." lines=... count=...
not-observed run="..." path="..." lines=... count=0
observed-branch run="..." test="..." path="..." line=... arm=... count=...
```

Exact named-test observations precede run-level observations, which precede
heuristic static test paths. Set `execution_mapping` and
`dynamic_evidence_status` from all relevant claims, not from whether any one
region executed.

Extend `view` to show observations owned by the selected node/file within its
existing 4096-byte bound and omission marker.

- [ ] **Step 8: Run and commit the coverage slice**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
git add src tests/e2e.rs
git commit -m "feat: import execution evidence"
```

---

### Task 4: Join and page review-ready evidence end to end

**Files:**

- Modify: `src/index.rs`
- Modify: `src/store.rs`
- Modify: `src/mcp.rs`
- Modify: `tests/e2e.rs`

**Interfaces:**

Extend `ReviewSection` exactly:

```rust
enum ReviewSection {
    Files,
    Diff,
    Artifacts,
    Graph,
    Evidence,
}
```

Use cursor code `e`, label `evidence_next_cursor`, and header `evidence`. Include
the evidence text in `ReviewSnapshot` checksum. A continuation cursor remains
bound to snapshot ID, depth, max-nodes, section, offset, and checksum.

**Steps:**

- [ ] **Step 1: Write RED evidence-pagination tests**

Extend current cursor helpers and tests to prove:

- initial output independently budgets all five sections;
- evidence cursor ordering and terminal exhaustion;
- no split UTF-8 code point and line-safe framing;
- exact emitted/prior/remaining record and byte accounting;
- cursor tampering, cross-snapshot reuse, depth/max-node changes, and stale
  checksum rejection;
- every initial/continuation page repeats content/static/dynamic terminal facts;
- pagination can finish while evidence status remains partial;
- no manifest yields no evidence records and
  `dynamic_evidence_status=not-applicable`.

Run and confirm RED:

```bash
cargo test index::tests::evidence_page_
cargo test --test e2e evidence_pagination
```

- [ ] **Step 2: Add the evidence section without changing graph limits**

Add `INITIAL_EVIDENCE_BUDGET` by redistributing the existing 8192-byte initial
review budget; do not raise it. Add evidence record ranges, page headers, cursor
encoding/decoding, checksum input, and completion metadata. Apply the current
`max_nodes` record limit only to graph records; evidence uses its byte/record
accounting and continuation cursor.

Update MCP instructions so clients exhaust `files`, `diff`, `artifacts`,
`graph`, and `evidence` cursors verbatim and report typed completeness rather
than treating partial evidence as a transport failure.

- [ ] **Step 3: Build the generated-code acceptance fixture**

In `tests/e2e.rs`, construct one temporary repository containing:

```text
proto/message.proto             changed strict annotation
src/generator.rs                one branch that emits strict code
src/lib.rs                      unique OUT_DIR include
src/predicate.rs                shared strict predicate with a branch
src/tests.rs                    unique strict_roundtrip test
target/.../out/message.rs       untracked/ignored generated encode + decode
target/graphr/strict.json       untracked LLVM coverage JSON
target/graphr/evidence.json     untracked v1 manifest
```

Create the source-only worktree snapshot first, then create evidence files and
index again with the same selection plus manifest. From one completely paged
`changes` review assert this exact chain:

```text
.proto annotation span
  -> mapped Rust generator branch
  -> verified generated output and unique include site
  -> generated encode CALLS predicate
  -> generated decode CALLS predicate
  -> positive named-test coverage for both call sites
  -> positive named-test coverage for the required predicate branch
```

Also assert the generated encode/decode symbols are searchable and their `view`
output includes provenance and observations.

- [ ] **Step 4: Add the four acceptance negatives**

Use separate fixture instances so cache state cannot hide a failure:

1. remove the decode-to-predicate call and assert the decode static path is
   absent and the end-to-end evidence output changes;
2. corrupt one declared digest and assert no evidence snapshot publishes;
3. omit `test_name` and assert execution remains run-level and the named-test
   claim becomes unknown/partial;
4. set the required predicate branch count to zero and assert
   `not-observed`, never `observed`.

Keep each failure assertion tied to the intended record, not a broad substring
such as `partial`.

Run:

```bash
cargo test --test e2e generated_evidence_chain -- --nocapture
cargo test --test e2e generated_evidence_negative -- --nocapture
```

- [ ] **Step 5: Add a mixed-gap review fixture**

Create one compact E2E repository containing a resolved call, ambiguous call,
dynamic dispatch, macro boundary, parse error, skipped source, JS test
registration, and exercising test. Assert that all cursors exhaust successfully
while affected callers/flows/static-test paths remain partial for the exact
ordered reasons. This proves content completion and evidence completeness are
independent.

Run:

```bash
cargo test --test e2e mixed_evidence_gaps -- --nocapture
```

- [ ] **Step 6: Run and commit the end-to-end slice**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
git add src tests/e2e.rs
git commit -m "feat: review dynamic evidence paths"
```

---

### Task 5: Document, audit, and finish the milestone

**Files:**

- Modify: `README.md`
- Modify if behavior changed: `docs/superpowers/specs/2026-08-22-dynamic-evidence-graph-design.md`
- Modify: `docs/superpowers/plans/2026-08-22-dynamic-evidence-graph.md` only to check completed boxes or record an approved deviation

**Steps:**

- [x] **Step 1: Document the producer/consumer workflow**

Update `README.md` with:

1. source-only `index` -> external generator/tests -> manifest -> evidence
   `index`;
2. the exact v1 manifest example from **Final Public Contract**;
3. accepted report formats and fixed bounds;
4. generated Rust's unique `OUT_DIR` include requirement;
5. the meanings of observed, not-observed, unknown, static heuristic paths, and
   manifest attestation;
6. all five continuation cursors and three repeated terminal facts;
7. explicit limits: no process execution, causal build trace, runtime call
   ordering, mutation proof, JS runtime coverage, before/after trust query, or
   normative citation mapping yet.

Do not describe the manifest as proof that a process caused an output.

- [x] **Step 2: Audit the implementation against the approved design**

Run these searches and resolve every hit intentionally:

```bash
rg -n "analysis_complete|review_complete|review_complete_when_pages_exhausted|coverage status=complete" src tests README.md
rg -n "TODO|TBD|FIXME|not implemented|temporary|compat|fallback" src tests README.md docs/superpowers/specs/2026-08-22-dynamic-evidence-graph-design.md
rg -n "continue;" src/parse.rs src/python.rs src/javascript.rs src/index.rs
rg -n "std::process|Command::new|tokio::process" src/evidence.rs src/coverage.rs
```

Expected results:

- removed output names have no production or public-test hits;
- no planning placeholder or compatibility path remains;
- every parser `continue` is for a proven non-relation capture or follows an
  emitted reference/modeled-site/gap;
- evidence/coverage modules execute no process.

Compare each spec verification bullet to a named unit/E2E test. If behavior
deviated, either fix it or amend the spec only after explicit user approval.

- [x] **Step 3: Run the complete serial gate with fresh evidence**

Run every command from the repository root and retain exit status/output:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
git status --short
```

If a command fails, use `superpowers:systematic-debugging`, repair the root
cause, and restart the gate from `cargo fmt --check`.

Controller-approved process ownership: the mandatory final whole-branch review
will satisfy Step 4's review requirement once, with this documentation
included. This task commits the documentation after the serial gate without a
duplicate review. The controller also owns Step 5 and branch handoff; this task
does not invoke the branch-finishing workflow or integrate the branch.

- [ ] **Step 4: Request review and commit documentation**

Use `superpowers:requesting-code-review` over the complete feature range. Resolve
all correctness, trust-boundary, rollback, determinism, and evidence-semantics
findings. Then commit documentation:

```bash
git add README.md docs/superpowers/specs/2026-08-22-dynamic-evidence-graph-design.md docs/superpowers/plans/2026-08-22-dynamic-evidence-graph.md
git commit -m "docs: explain dynamic evidence review"
```

- [ ] **Step 5: Finish without integrating automatically**

Use `superpowers:finishing-a-development-branch`. Report:

```text
Branch and exact HEAD
Four slice commits plus documentation commit
Required checks and exit status
Generated acceptance chain result
Known explicit evidence limits
Next human integration choices
```

Do not merge, push, delete the worktree, or remove the branch without the user's
explicit selection.
