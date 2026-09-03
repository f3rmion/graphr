# DOT Change-Impact View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in, deterministic Graphviz DOT view of changed symbols and their affected caller paths to the existing `changes` MCP tool.

**Architecture:** `Store::changes` already owns the changed roots, risk scores, structured affected flows, and the traversal that visits direct caller/test edges. It will retain those visited call edges without another query and render DOT beside the existing text while the values are available. `Engine::changes` caches both strings under the existing snapshot/depth/max-nodes key and selects either the unchanged paged review or one complete bounded DOT document from a new `format` parameter.

**Tech Stack:** Rust 2024, standard library collections/string handling, SQLite-backed existing graph analysis, rmcp/serde/schemars, existing in-module and MCP end-to-end tests.

**Spec:** `docs/superpowers/specs/2026-09-03-dot-change-impact-design.md`

## Global Constraints

- Keep one Rust binary serving MCP over stdio; add no renderer, process execution, file-writing API, UI, or dependency.
- `format` defaults to `review`; `format: "dot"` is explicit and rejects `cursor`.
- DOT is one valid `digraph graphr_changes`, never a paged fragment, and is at most 8 KiB.
- DOT `depth` is 0 through 6 and limits visible caller edges; `max_nodes` is 1 through 50 and limits the whole DOT graph.
- Emit only call paths, with actual caller-to-callee edge direction.
- Prefer directly changed roots over context; identify derived callable roots as `affected`, not `changed`.
- Escape all repository-controlled label content and expose omissions without inventing nodes or edges.
- Keep current review text, pagination, cache format, trust-boundary failures, and structured provenance unchanged.
- Add no compatibility layer, migration, speculative abstraction, or new crate.
- Required final checks: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build --locked --release`.

## File map

- `src/store.rs`: build and render the bounded DOT document from existing structured change analysis; store it on `ChangeReview`.
- `src/index.rs`: define the format enum, cache DOT with the review, select the requested representation, and reject DOT cursors.
- `src/mcp.rs`: expose the optional `format` argument and pass it to the engine.
- `tests/e2e.rs`: prove the stdio MCP interface returns renderable DOT with snapshot provenance and keeps review mode as the default.
- `README.md`: document the opt-in call, bounds, completeness semantics, and external Graphviz rendering.

---

### Task 1: Render bounded DOT from structured change analysis

**Files:**

- Modify: `src/store.rs:527-532,959-1330,2039-2087,4293-4380`
- Modify mechanically for the new field: `src/index.rs:1300-1345,3919-3935,4360-4375,5920-6020`
- Test: `src/store.rs` in the existing `tests` module

**Interfaces:**

- Consumes: risk-sorted `&[RowNode]`, `&ChangeAnalysis`, call/test edges retained by the existing neighborhood traversal, `DependencyMode`, `depth`, `max_nodes`, and final analysis accounting already present in `Store::changes`.
- Produces: `ChangeReview { dot: String, .. }` and `pub(crate) fn no_change_dot(snapshot_id: &str, reason: &str) -> String` for the engine's metadata-only no-change fast path.

- [ ] **Step 1: Write the failing layered-graph test**

Add a focused fixture beside the existing `flow_line` tests. Model the ordinary `CALLS` path as an affected flow and the test edge as a relation retained by neighborhood traversal. Include the caller-to-changed edge in both inputs so deduplication is observable.

```rust
#[test]
fn dot_change_impact_is_layered_and_deduplicated() {
    let changed = RowNode {
        id: 3,
        kind: "function".into(),
        name: "changed".into(),
        path: "src/lib.rs".into(),
        line: 30,
    };
    let test = FlowNode {
        id: 1,
        kind: "test".into(),
        name: "covers_changed".into(),
        qualified_name: "covers_changed".into(),
        path: "tests/change.rs".into(),
        line: 10,
    };
    let caller = FlowNode {
        id: 2,
        kind: "function".into(),
        name: "caller".into(),
        qualified_name: "caller".into(),
        path: "src/lib.rs".into(),
        line: 20,
    };
    let target = FlowNode {
        id: 3,
        kind: "function".into(),
        name: "changed".into(),
        qualified_name: "changed".into(),
        path: "src/lib.rs".into(),
        line: 30,
    };
    let analysis = ChangeAnalysis {
        risks: HashMap::from([(
            3,
            NodeRisk {
                score: 4_200,
                flow_component: 4_200,
                test_component: 0,
                security_component: 0,
                caller_component: 0,
                test_node: false,
                test_gap: false,
                indirect_test_covered: false,
            },
        )]),
        flows: vec![AffectedFlow {
            entry: caller.clone(),
            nodes: vec![caller.clone(), target.clone()],
            parents: HashMap::from([(3, 2)]),
            changed: vec![3],
            depth: 1,
            file_count: 1,
            criticality: 4_200,
        }],
        flow_omitted: false,
        test_mapping_omitted: false,
    };
    let row = |node: &FlowNode| RowNode {
        id: node.id,
        kind: node.kind.clone(),
        name: node.name.clone(),
        path: node.path.clone(),
        line: node.line,
    };
    let calls = ChangeCalls {
        nodes: HashMap::from([(1, row(&test)), (2, row(&caller)), (3, row(&target))]),
        edges: BTreeSet::from([(1, 2, true), (2, 3, false)]),
    };
    let accounting = DotAccounting {
        changed_total: 1,
        analysis_roots_omitted: 0,
        deleted_paths_unanalyzed: 0,
        unmapped_ranges: 0,
        file_mapped_ranges: 0,
        traversal_complete: true,
    };

    let dot = change_dot(
        SNAPSHOT,
        &[changed],
        &analysis,
        &calls,
        (6, 50),
        DependencyMode::Boundary,
        accounting,
    )
    .unwrap();

    assert!(dot.starts_with("digraph graphr_changes {\n"));
    assert!(dot.ends_with("}\n"));
    assert_eq!(dot.matches("n1 [").count(), 1);
    assert_eq!(dot.matches("n2 [").count(), 1);
    assert_eq!(dot.matches("n3 [").count(), 1);
    assert!(dot.contains("n1 -> n2 [style=dashed];"));
    assert!(dot.contains("n2 -> n3;"));
    assert!(dot.contains("changed risk=0.4200"));
    assert!(
        dot.lines()
            .find(|line| line.trim_start().starts_with("n1 ["))
            .unwrap()
            .contains("shape=ellipse")
    );
    assert!(
        dot.lines()
            .find(|line| line.trim_start().starts_with("n3 ["))
            .unwrap()
            .contains("fillcolor=\"#fed7aa\"")
    );
    assert!(dot.contains("rankdir=LR"));
}
```

- [ ] **Step 2: Run the focused test and verify the missing renderer fails**

Run:

```bash
cargo test store::tests::dot_change_impact_is_layered_and_deduplicated -- --exact
```

Expected: compilation fails because `DotAccounting` and `change_dot` do not exist.

- [ ] **Step 3: Add the concrete rendering data and path reconstruction**

Add the DOT budget, the minimal retained-call record, and the accounting record near the existing change-analysis types. Import `std::borrow::Cow` and derive `Clone` for `RowNode`. `ChangeCalls` is request-local output from the traversal, not a second graph model.

```rust
const DOT_BUDGET: usize = 8 * 1024;
const DOT_LABEL_PART_LIMIT: usize = 160;

#[derive(Default)]
struct ChangeCalls {
    nodes: HashMap<i64, RowNode>,
    // (caller, callee, is_test_call)
    edges: BTreeSet<(i64, i64, bool)>,
}

#[derive(Clone, Copy)]
struct DotAccounting {
    changed_total: usize,
    analysis_roots_omitted: usize,
    deleted_paths_unanalyzed: usize,
    unmapped_ranges: usize,
    file_mapped_ranges: usize,
    traversal_complete: bool,
}

fn flow_path(flow: &AffectedFlow, target: i64, depth: u32) -> Result<Vec<i64>> {
    let mut path = vec![target];
    while path.last().copied() != Some(flow.entry.id) {
        let current = *path.last().expect("path starts at its target");
        let parent = flow
            .parents
            .get(&current)
            .copied()
            .ok_or_else(|| "affected flow path is incomplete".to_owned())?;
        if path.contains(&parent) || path.len() > FLOW_DEPTH as usize {
            return Err("affected flow path is cyclic".into());
        }
        path.push(parent);
    }
    path.reverse();
    let keep = depth as usize + 1;
    if path.len() > keep {
        path.drain(..path.len() - keep);
    }
    Ok(path)
}
```

Initialize `ChangeCalls` with the changed roots before `traverse_changes`. Pass it into `traverse_changes` and, after the dependency-collapse check but before the visited-node check, retain nodes plus these two incoming relationships:

```rust
match relation {
    "test <-" => {
        calls.nodes.entry(node.id).or_insert_with(|| node.clone());
        calls.edges.insert((node.id, source, true));
    }
    "caller <-" => {
        calls.nodes.entry(node.id).or_insert_with(|| node.clone());
        calls.edges.insert((node.id, source, false));
    }
    _ => {}
}
```

The existing traversal queries, text lines, visit order, limits, and omission result must not change.

Implement `change_dot` with this exact selection order:

1. Copy direct changed roots from `roots` in their existing risk order.
2. Build one ID-to-node catalog from roots, flow nodes, and `ChangeCalls`; direct-root metadata wins on duplicate IDs.
3. Build candidate paths by iterating the already criticality-sorted flows, then each flow's sorted `changed` IDs, and calling `flow_path`.
4. Admit direct changed roots until `max_nodes`, then admit affected-flow paths that fit.
5. Walk `ChangeCalls.edges` in reverse breadth-first order from every admitted changed/impact root, admitting connected callers and tests that fit. This attaches `TEST_CALLS` without changing flow/risk semantics.
6. Treat a flow target absent from `analysis.risks` as a derived `affected` root.
7. Deduplicate edges as `(caller_id, callee_id)` pairs; preserve the retained `is_test_call` flag and render those edges dashed.
8. Render direct roots first, then context in first-admission order; render deduplicated edges in numeric pair order.

Use the following signature so `Store::changes` can pass values it already owns:

```rust
fn change_dot(
    snapshot_id: &str,
    roots: &[RowNode],
    analysis: &ChangeAnalysis,
    calls: &ChangeCalls,
    limits: (u32, u32),
    dependency_mode: DependencyMode,
    accounting: DotAccounting,
) -> Result<String>
```

Render these non-color cues as well as colors: append `changed risk=<score>` or `affected` to the label, and keep test nodes elliptical. Style precedence is changed, affected, dependency boundary, unchanged test, then ordinary context.

```rust
let attributes = if changed_ids.contains(&node.id) {
    format!(
        "fillcolor=\"#fed7aa\", color=\"#c2410c\", penwidth=2, label=\"{}\\n{}:{}\\nchanged risk={}\"",
        dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
        dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
        node.line,
        score_text(analysis.risks[&node.id].score),
    )
} else if impact_ids.contains(&node.id) {
    format!(
        "fillcolor=\"#fef3c7\", color=\"#a16207\", label=\"{}\\n{}:{}\\naffected\"",
        dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
        dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
        node.line,
    )
} else if dependency_mode == DependencyMode::Boundary
    && dependency_package(&node.path).is_some()
{
    format!(
        "fillcolor=\"#e5e7eb\", color=\"#4b5563\", label=\"{}\\n{}:{}\"",
        dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
        dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
        node.line,
    )
} else {
    format!(
        "label=\"{}\\n{}:{}\"",
        dot_escape(&shorten(&node.name, DOT_LABEL_PART_LIMIT)),
        dot_escape(&shorten(&node.path, DOT_LABEL_PART_LIMIT)),
        node.line,
    )
};
```

Add `shape=ellipse, fillcolor="#dbeafe", color="#2563eb"` for unchanged test nodes and at least `shape=ellipse` for changed/affected tests.

- [ ] **Step 4: Add failing trust-boundary and budget tests**

Add one test that puts quotes, a backslash, newlines, carriage returns, a tab, and `é` in names/paths. Add another with 50 long nodes and paths. Assert no raw label can terminate its quoted DOT string, UTF-8 remains valid, output never exceeds `DOT_BUDGET`, low-priority paths are omitted as a whole, and the closing brace remains.

```rust
#[test]
fn dot_change_impact_escapes_labels_and_preserves_framing() {
    assert_eq!(
        dot_escape("quote\" slash\\ line\nreturn\rtab\té"),
        "quote\\\" slash\\\\ line\\nreturn\\ntab\\té"
    );
    let shortened = shorten(&"é".repeat(200), DOT_LABEL_PART_LIMIT);
    assert!(shortened.len() <= DOT_LABEL_PART_LIMIT);
    assert!(shortened.ends_with('…'));
}

#[test]
fn dot_change_impact_is_one_bounded_document() {
    let (roots, analysis, calls, accounting) = oversized_dot_fixture(50);
    let dot = change_dot(
        SNAPSHOT,
        &roots,
        &analysis,
        &calls,
        (6, 50),
        DependencyMode::Boundary,
        accounting,
    )
    .unwrap();
    assert!(dot.len() <= DOT_BUDGET, "{}", dot.len());
    assert!(dot.starts_with("digraph graphr_changes {\n"));
    assert!(dot.ends_with("}\n"));
    assert!(dot.contains("render_complete=false"));
}
```

Add `dot_change_impact_marks_derived_and_dependency_nodes` with a changed type
root and an `AffectedFlow` whose `changed` function ID is absent from
`analysis.risks`. Put a `.cargo/vendor/example/src/lib.rs` caller before that
function and assert the function line contains `affected` with `#fef3c7`, while
the dependency line contains `#e5e7eb`. This proves the renderer does not call
the derived function directly changed.

Add an exact empty-document check:

```rust
#[test]
fn no_change_dot_is_valid_and_escaped() {
    let dot = no_change_dot(SNAPSHOT, "empty_\"worktree\\delta");
    assert!(dot.starts_with("digraph graphr_changes {\n"));
    assert!(dot.contains("no_changes_reason=empty_\\\"worktree\\\\delta"));
    assert!(!dot.contains("  n"));
    assert!(dot.ends_with("}\n"));
    assert!(dot.len() <= DOT_BUDGET);
}
```

Define `oversized_dot_fixture(count: usize)` in the test module to return
`(Vec<RowNode>, ChangeAnalysis, ChangeCalls, DotAccounting)`. This fixture
guarantees the pre-pruned DOT would exceed 8 KiB without depending on parser or
SQLite behavior:

```rust
fn oversized_dot_fixture(
    count: usize,
) -> (Vec<RowNode>, ChangeAnalysis, ChangeCalls, DotAccounting) {
    let mut roots = Vec::with_capacity(count);
    let mut risks = HashMap::with_capacity(count);
    let mut flows = Vec::with_capacity(count);
    let mut calls = ChangeCalls::default();
    for index in 0..count {
        let caller_id = i64::try_from(index * 2 + 1).unwrap();
        let target_id = caller_id + 1;
        let score = u32::try_from((count - index) * 100).unwrap();
        let target = RowNode {
            id: target_id,
            kind: "function".into(),
            name: format!("changed_{index}_{}", "x".repeat(320)),
            path: format!("src/{}/changed_{index}.rs", "p".repeat(320)),
            line: 2,
        };
        let caller = RowNode {
            id: caller_id,
            kind: "function".into(),
            name: format!("caller_{index}_{}", "y".repeat(320)),
            path: format!("src/{}/caller_{index}.rs", "q".repeat(320)),
            line: 1,
        };
        roots.push(target.clone());
        risks.insert(
            target_id,
            NodeRisk {
                score,
                flow_component: score,
                test_component: 0,
                security_component: 0,
                caller_component: 0,
                test_node: false,
                test_gap: false,
                indirect_test_covered: false,
            },
        );
        flows.push(AffectedFlow {
            entry: FlowNode {
                id: caller.id,
                kind: caller.kind.clone(),
                name: caller.name.clone(),
                qualified_name: caller.name.clone(),
                path: caller.path.clone(),
                line: caller.line,
            },
            nodes: vec![
                FlowNode {
                    id: caller.id,
                    kind: caller.kind.clone(),
                    name: caller.name.clone(),
                    qualified_name: caller.name.clone(),
                    path: caller.path.clone(),
                    line: caller.line,
                },
                FlowNode {
                    id: target.id,
                    kind: target.kind.clone(),
                    name: target.name.clone(),
                    qualified_name: target.name.clone(),
                    path: target.path.clone(),
                    line: target.line,
                },
            ],
            parents: HashMap::from([(target_id, caller_id)]),
            changed: vec![target_id],
            depth: 1,
            file_count: 2,
            criticality: score,
        });
        calls.nodes.insert(caller_id, caller);
        calls.nodes.insert(target_id, target);
        calls.edges.insert((caller_id, target_id, false));
    }
    (
        roots,
        ChangeAnalysis {
            risks,
            flows,
            flow_omitted: false,
            test_mapping_omitted: false,
        },
        calls,
        DotAccounting {
            changed_total: count,
            analysis_roots_omitted: 0,
            deleted_paths_unanalyzed: 0,
            unmapped_ranges: 0,
            file_mapped_ranges: 0,
            traversal_complete: true,
        },
    )
}
```

- [ ] **Step 5: Implement safe label shortening, escaping, and whole-path pruning**

Use character-boundary truncation and explicit DOT escapes. Replace remaining control characters with U+FFFD so they cannot affect DOT parsing.

```rust
fn shorten(value: &str, limit: usize) -> Cow<'_, str> {
    if value.len() <= limit {
        return Cow::Borrowed(value);
    }
    let mut end = limit.saturating_sub('…'.len_utf8()).min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}…", &value[..end]))
}

fn dot_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' | '\r' => output.push_str("\\n"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => output.push('�'),
            value => output.push(value),
        }
    }
    output
}
```

Render the selected roots and paths, and if the result exceeds `DOT_BUDGET`, pop the last selected path and re-render. Once no paths remain, pop the lowest-priority changed root and re-render. The graph label must include these exact machine-readable fields:

```text
snapshot=<id> changed_emitted=<n> changed_total=<n> paths_emitted=<n> paths_discovered=<n> flow_discovery=complete|partial render_complete=true|false analysis_roots_omitted=<n> deleted_paths_unanalyzed=<n> unmapped_ranges=<n> file_mapped_ranges=<n> traversal_complete=true|false
```

The maximum is 50 nodes, so the simple re-render loop is intentionally bounded. Leave this ceiling explicit:

```rust
// ponytail: re-rendering is bounded to 50 nodes; stream with reserved bytes only if that cap grows.
```

- [ ] **Step 6: Attach DOT to every `ChangeReview` without changing review text**

Add the field and compute it after final completeness accounting while `roots` and `analysis` are still available:

```rust
pub struct ChangeReview {
    pub graph: String,
    pub dot: String,
    pub evidence: String,
    pub static_status: CompletenessStatus,
    pub dynamic_status: CompletenessStatus,
}
```

For the two early returns in `Store::changes`, call the same renderer with empty roots/analysis and accurate deleted/unmapped accounting. Add:

```rust
pub(crate) fn no_change_dot(snapshot_id: &str, reason: &str) -> String
```

It must produce a complete empty graph whose label is `snapshot=<id> no_changes_reason=<escaped reason>`.

Update every test-only `ChangeReview` literal in `src/index.rs` with `dot: String::new()` so the repository remains buildable after this task. Do not change the existing `graph` strings or review assertions.

- [ ] **Step 7: Run focused and regression tests**

Run:

```bash
cargo test store::tests::dot_change_impact
cargo test index::tests::review_context_pages_diff_and_graph_without_losing_utf8 -- --exact
cargo test
```

Expected: all commands exit 0; existing paged review assertions remain unchanged.

- [ ] **Step 8: Commit the renderer slice**

```bash
git add src/store.rs src/index.rs
git commit -m "feat: render bounded DOT change impact"
```

---

### Task 2: Expose DOT through `changes` and document the workflow

**Files:**

- Modify: `src/index.rs:1-45,613-704,1270-1360`
- Modify: `src/mcp.rs:1-35,123-135,229-247,464-523`
- Modify: `tests/e2e.rs:4327-4430,5625-5670,7166-7350`
- Modify: `README.md:69-90,196-225`
- Test: `src/mcp.rs`, `src/index.rs`, and `tests/e2e.rs`

**Interfaces:**

- Consumes: `ChangeReview::dot` and `no_change_dot` from Task 1.
- Produces: optional MCP input `format: "review" | "dot"`, defaulting to `review`; DOT text content with unchanged `QueryOutput.provenance` structured content.

- [ ] **Step 1: Write failing schema and default-format tests**

Define the expected enum before implementing it by extending `tool_schemas_require_explicit_root_or_snapshot_context`:

```rust
let changes = by_name("changes");
let format = &changes.input_schema["properties"]["format"];
let definition = format["$ref"].as_str().unwrap().rsplit('/').next().unwrap();
assert_eq!(
    changes.input_schema["$defs"][definition]["enum"],
    rmcp::serde_json::json!(["review", "dot"]),
);
assert!(
    !changes.input_schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "format")
);
```

Add the explicit default test:

```rust
#[test]
fn changes_format_defaults_to_review() {
    assert_eq!(ChangesFormat::default(), ChangesFormat::Review);
}
```

- [ ] **Step 2: Run the schema test and verify it fails**

Run:

```bash
cargo test mcp::tests::tool_schemas_require_explicit_root_or_snapshot_context -- --exact
```

Expected: FAIL because `changes` has no `format` property.

- [ ] **Step 3: Add the format enum and MCP parameter**

Define the API enum in `src/index.rs` so both MCP deserialization and engine selection use one type:

```rust
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    serde::Deserialize,
    rmcp::schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum ChangesFormat {
    #[default]
    Review,
    Dot,
}
```

Import it in `src/mcp.rs` and extend `ChangesParams`:

```rust
#[serde(default)]
format: ChangesFormat,
```

Change the `changes` tool description to
`"Return a paged review or one bounded DOT change-impact graph for an explicit snapshot_id"`
so DOT mode is discoverable without making it the normal review workflow.

Add a temporary `_format: ChangesFormat` argument to `Engine::changes`, pass
`params.format` immediately before the cursor argument, and otherwise leave the
method's output selection unchanged in this step. Update the two direct engine
calls in `src/index.rs` tests to pass `ChangesFormat::Review`. This keeps the
schema slice compiling while the next red test proves the format is not yet
implemented.

- [ ] **Step 4: Write the failing engine-selection and MCP end-to-end tests**

Add a focused MCP fixture with a test calling a caller which calls a modified function:

```rust
#[test]
fn changes_dot_returns_bounded_affected_callgraph() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path.join("src")).unwrap();
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn changed() -> u32 { 1 }\n\
         pub fn caller() -> u32 { changed() }\n\
         #[test]\nfn covers_changed() { let _ = caller(); }\n",
    )
    .unwrap();
    init_git(&fixture.path);
    git(&fixture.path, &["add", "--", "src/lib.rs"]);
    git_commit(&fixture.path, "baseline");
    fs::write(
        fixture.path.join("src/lib.rs"),
        "pub fn changed() -> u32 { 2 }\n\
         pub fn caller() -> u32 { changed() }\n\
         #[test]\nfn covers_changed() { let _ = caller(); }\n",
    )
    .unwrap();
    let mut client = Client::start(&fixture.path);

    let review = response_text(&client.changes(6, 50, None));
    assert!(review.starts_with("files\n"));

    let response = client.changes_dot(6, 50);
    let value = response_json(&response);
    let dot = response_text(&response);
    let node_id = |needle: &str| {
        dot.lines()
            .find(|line| line.contains(needle))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    };
    let test = node_id("label=\"covers_changed\\n");
    let caller = node_id("label=\"caller\\n");
    let changed = node_id("label=\"changed\\n");
    assert!(dot.contains(&format!("{test} -> {caller} [style=dashed];")));
    assert!(dot.contains(&format!("{caller} -> {changed};")));
    assert!(dot.contains("changed risk="));
    assert!(dot.len() <= 8 * 1024);
    assert_eq!(
        value["result"]["structuredContent"]["provenance"]["snapshot_id"],
        client.snapshot_id(),
    );

    let cursor = client.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": client.snapshot_id(),
            "depth": 6,
            "max_nodes": 50,
            "format": "dot",
            "cursor": "not-a-review-cursor",
        }),
    );
    assert_tool_error_code(&cursor, "invalid_parameters");
    assert_eq!(
        response_json(&cursor)["result"]["structuredContent"]["message"],
        "DOT changes do not accept a cursor",
    );
    let invalid = client.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": client.snapshot_id(),
            "format": "svg",
        }),
    );
    let invalid = response_json(&invalid);
    assert!(invalid["error"].is_object() || invalid["result"]["isError"] == true);
    client.close();
}
```

Add this helper without changing the existing `Client::changes` signature:

```rust
fn changes_dot(&mut self, depth: u32, max_nodes: u32) -> String {
    self.call(
        "changes",
        rmcp::serde_json::json!({
            "snapshot_id": self.snapshot_id(),
            "depth": depth,
            "max_nodes": max_nodes,
            "format": "dot",
        }),
    )
}
```

Extend the existing `identical_commit_oids` test to call `changes_dot(1, 50)` and assert a valid empty graph containing `no_changes_reason=identical_commit_oids` while structured `no_change_reason` remains unchanged.

Also add this cheap unit loop in `src/index.rs` so all four structured reasons
remain renderable without four repository fixtures:

```rust
#[test]
fn every_no_change_reason_renders_empty_dot() {
    for reason in [
        NoChangeReason::IdenticalCommitOids,
        NoChangeReason::IdenticalTrees,
        NoChangeReason::EmptyIndexDelta,
        NoChangeReason::EmptyWorktreeDelta,
    ] {
        let dot = no_change_dot(REVIEW_SNAPSHOT_ID, reason.as_str());
        assert!(dot.starts_with("digraph graphr_changes {\n"));
        assert!(dot.contains(&format!("no_changes_reason={}", reason.as_str())));
        assert!(dot.ends_with("}\n"));
    }
}
```

Add `NoChangeReason` to the existing `crate::workspace` imports in the index
test module.

- [ ] **Step 5: Run the new tests and verify output selection still fails**

Run:

```bash
cargo test --test e2e changes_dot_returns_bounded_affected_callgraph -- --exact
```

Expected: FAIL because `Engine::changes` still returns review text for DOT format.

- [ ] **Step 6: Implement engine selection and no-change DOT**

Destructure and retain `dot` in `ReviewSnapshot::new`. Insert the field between the existing graph and evidence strings:

```rust
graph: String,
dot: String,
evidence: String,
```

Rename the temporary `_format` argument to `format`. Validate the combination before the no-change fast path so cursors are rejected consistently for every snapshot:

```rust
if format == ChangesFormat::Dot && cursor.is_some() {
    return Err(OperationError::new(
        ErrorCode::InvalidParameters,
        "DOT changes do not accept a cursor",
    ));
}
```

For `snapshot.no_change_reason`, select the existing text for review mode and `no_change_dot(snapshot_id, reason.as_str())` for DOT mode. For cached reviews, use:

```rust
let text = match (format, cursor) {
    (ChangesFormat::Review, Some(cursor)) => render_section(
        &review,
        &parse_review_cursor(cursor, snapshot_id, depth, max_nodes)
            .map_err(query_operation_error)?,
    )
    .map_err(query_operation_error)?,
    (ChangesFormat::Review, None) => review_context(&review).map_err(query_operation_error)?,
    (ChangesFormat::Dot, None) => review.dot.clone(),
    (ChangesFormat::Dot, Some(_)) => unreachable!("DOT cursors rejected before rendering"),
};
```

Do not include `format` in the review cache key or cursor checksum: the cached object owns both renderings, and DOT accepts no cursor.

- [ ] **Step 7: Document the opt-in export and external renderer**

Add this example beside the existing `changes` call:

```text
changes({
  "snapshot_id": "<digest>",
  "depth": 6,
  "max_nodes": 50,
  "format": "dot"
})
```

State that review is the default, DOT mode returns one complete document with no cursor, its bounds apply to the whole visual graph, and omissions/completeness remain explicit. Tell users to save the returned text and render outside Graphr, for example:

```bash
dot -Tsvg impact.dot -o impact.svg
```

Clarify that Graphr neither invokes nor depends on Graphviz and that the graph is static resolved-call evidence, not runtime execution.

- [ ] **Step 8: Run targeted tests and formatting**

Run:

```bash
cargo fmt --check
cargo test mcp::tests::tool_schemas_require_explicit_root_or_snapshot_context -- --exact
cargo test index::tests::changes_format_defaults_to_review -- --exact
cargo test index::tests::every_no_change_reason_renders_empty_dot -- --exact
cargo test --test e2e changes_dot_returns_bounded_affected_callgraph -- --exact
cargo test --test e2e identical_clean_oids_return_explained_no_changes -- --exact
```

Expected: every command exits 0.

- [ ] **Step 9: Run the complete repository gate**

Run each command separately and inspect its exit status:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
```

Expected: every command exits 0 and `git diff --check` prints nothing.

- [ ] **Step 10: Commit the MCP and documentation slice**

```bash
git add src/index.rs src/mcp.rs tests/e2e.rs README.md
git commit -m "feat: export DOT change impact over MCP"
```
