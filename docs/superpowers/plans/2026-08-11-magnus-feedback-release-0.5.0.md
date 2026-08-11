# Magnus Feedback and Graphr 0.5.0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve Magnus's four open Graphr issues, verify the exact audit-range reproduction, and prepare a local 0.5.0 release without publishing it.

**Architecture:** Preserve wildcard Rust imports in the existing parsed-import stream, encode one unambiguous module-level glob in the existing binding map, and add its exact call keys only after ordinary lexical candidates. Keep the immutable stateless review protocol; close the remaining cursor, completeness, and repository-binding gaps through precise README, MCP, and bundled-skill guidance.

**Tech Stack:** Rust 2024, Tree-sitter Rust, SQLite/rusqlite, rmcp stdio, Cargo, Git.

## Global Constraints

- Build one Rust binary for Codex and Claude over MCP stdio.
- Rust and Python remain the only indexed languages.
- Do not add Java, editor code, HTTP, UI, embeddings, plugins, migrations, or dependencies.
- Do not preserve obsolete behavior through compatibility paths or fallbacks.
- Keep output deterministic, compact, bounded, and conservative at trust boundaries.
- Keep explicit imports authoritative; never guess across multiple or block-local glob imports.
- Do not change risk weights, security-name matching, or static-flow semantics.
- Do not tag, push, publish, post to GitHub, or close issues during local preparation.
- Use test-driven development for parser and resolver behavior.

---

### Task 1: Preserve bounded Rust wildcard imports

**Files:**
- Modify: `src/parse.rs:480-545`
- Test: `src/parse.rs:615-850`

**Interfaces:**
- Consumes: Tree-sitter `use_wildcard` nodes already captured through `use_declaration`.
- Produces: existing `Import.path: String` values such as `super::*` and `crate::support::*`; no new type.

- [ ] **Step 1: Add the failing parser test**

Add this test beside `bounds_grouped_import_expansion`:

```rust
#[test]
fn retains_module_and_block_wildcard_import_paths() {
    let parsed = RustParser::new()
        .unwrap()
        .parse(
            r#"
mod tests {
    use super::*;
    fn check() {
        use crate::support::*;
    }
}
"#,
        )
        .unwrap();

    assert_eq!(
        parsed
            .imports
            .iter()
            .map(|import| (import.path.as_str(), import.block_local))
            .collect::<Vec<_>>(),
        [("super::*", false), ("crate::support::*", true)]
    );
}
```

This catches the current `"use_wildcard" => {}` branch discarding both paths.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```text
cargo test parse::tests::retains_module_and_block_wildcard_import_paths -- --exact
```

Expected: FAIL because the parsed import list is empty instead of containing the two literal paths.

- [ ] **Step 3: Retain wildcard paths with the existing bounded join**

Replace the empty `use_wildcard` match arm in `flatten_use` with:

```rust
"use_wildcard" => paths.push(join_use(&prefix, text(node, source))?),
```

Do not add a wildcard-specific parser type. `join_use` already enforces `PATH_LIMIT` and correctly composes grouped forms such as `use crate::{support::*, Item};`.

- [ ] **Step 4: Run focused and parser tests and verify GREEN**

Run:

```text
cargo test parse::tests::retains_module_and_block_wildcard_import_paths -- --exact
cargo test parse::tests
```

Expected: the focused test and every parser test pass.

- [ ] **Step 5: Commit the parser slice**

```text
git add src/parse.rs
git commit -m "fix(rust): retain wildcard import paths"
```

---

### Task 2: Resolve calls through one module-level glob

**Files:**
- Modify: `src/index.rs:1663-1925`
- Test: `src/index.rs:3380-3610`

**Interfaces:**
- Consumes: `ParsedFile.imports` containing `Import.path` values ending in `::*`.
- Produces: `use_binding(raw: &str, module: &str, root: &str) -> Option<(String, String)>` with the existing binding name `"*"` for a normalized glob prefix.
- Produces: `glob_import_binding(imports: &ImportBindings, source: usize, module: Option<usize>) -> Option<&str>`.
- Preserves: existing `RefInput.resolved_target_key` resolution and `Binding::Ambiguous` behavior.

- [ ] **Step 1: Add the failing resolver test**

Add this test beside `resolves_inline_module_and_root_calls_with_exact_keys`:

```rust
#[test]
fn resolves_one_module_glob_after_lexical_candidates() {
    let sources = [Source {
        path: "src/lib.rs".into(),
        text: r#"
pub fn verify_chain() {}
pub fn public_entry() {}
pub struct AuditRecord;
impl AuditRecord { pub fn build() {} }

#[cfg(test)]
mod tests {
    use super::*;
    fn verify_chain() {}
    #[test]
    fn checks_chain() {
        verify_chain();
        public_entry();
        AuditRecord::build();
    }
}
"#
        .into(),
    }];
    let graph = build_graph(&sources, &AtomicBool::new(false)).unwrap();
    let node = |key: &str| {
        graph
            .nodes
            .iter()
            .find(|node| node.keys.iter().any(|candidate| candidate == key))
            .unwrap()
    };
    let test = node("rust:function:tests::checks_chain");

    for target in [
        node("rust:function:tests::verify_chain"),
        node("rust:function:public_entry"),
        node("rust:method:AuditRecord::build"),
    ] {
        assert!(graph.refs.iter().any(|reference| {
            reference.source_key == test.key
                && reference.resolved_target_key.as_deref() == Some(target.key.as_str())
        }));
    }

    let outer = node("rust:function:verify_chain");
    assert!(!graph.refs.iter().any(|reference| {
        reference.source_key == test.key
            && reference.resolved_target_key.as_deref() == Some(outer.key.as_str())
    }));
}
```

The expected targets are hand-selected. This catches both missing glob-derived keys and the unsafe ordering that would let a glob override the lexical `tests::verify_chain`.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```text
cargo test index::tests::resolves_one_module_glob_after_lexical_candidates -- --exact
```

Expected: FAIL because `public_entry` and `AuditRecord::build` have no resolved call from `checks_chain`; the lexical `verify_chain` assertion already passes.

- [ ] **Step 3: Normalize a wildcard import into the existing binding map**

At the start of `use_binding`, after separating an optional `as` alias and before the ordinary `normalize_use` call, add:

```rust
if alias.is_none()
    && let Some(prefix) = path.strip_suffix("::*")
{
    return Some(("*".into(), normalize_relative(prefix, module, root)?));
}
```

Keep `normalize_use` rejecting `*` so wildcard imports do not claim a single `IMPORTS` target edge. Existing `import_bindings` logic will keep one normalized `"*"` binding unique and turn different glob prefixes in the same scope into `Binding::Ambiguous`.

- [ ] **Step 4: Add one conservative glob lookup helper**

Place this beside `import_binding`:

```rust
fn glob_import_binding(
    imports: &ImportBindings,
    source: usize,
    module: Option<usize>,
) -> Option<&str> {
    match import_binding(imports, source, module, "*") {
        Some(Binding::Unique(path)) => Some(path),
        Some(Binding::Ambiguous) | None => None,
    }
}
```

Do not inspect block ancestry. `import_bindings` already stores block-local imports as `Binding::Ambiguous`, so this helper rejects them.

- [ ] **Step 5: Add glob candidates after ordinary unqualified candidates**

In the one-part branch of `call_keys`, keep value shadowing and explicit-import handling unchanged. After pushing the current source-scope and module keys, append:

```rust
if let Some(prefix) = glob_import_binding(&bindings.imports, source, module_index) {
    let target = join_path(prefix, name);
    keys.push(format!("rust:function:{target}"));
    keys.push(item_key(&target));
}
```

Then keep the existing `dedup_keys(keys)` return. This ordering makes a real lexical definition resolve before a glob candidate.

- [ ] **Step 6: Add glob owners after ordinary qualified owners**

Replace the single `absolute_owner` calculation in the qualified-call branch with an ordered owner list:

```rust
let first = parts[0];
let mut owners = Vec::with_capacity(2);
match import_binding(&bindings.imports, source, module_index, first) {
    Some(Binding::Unique(path)) => owners.push(if parts.len() == 2 {
        path.clone()
    } else {
        join_path(path, &parts[1..parts.len() - 1].join("::"))
    }),
    Some(Binding::Ambiguous) => {
        return vec![format!("rust:ambiguous-import:{owner}::{method}")];
    }
    None => {
        if let Some(local) = normalize_relative(&owner, module, root) {
            owners.push(local);
        }
        if !matches!(first, "crate" | "self" | "super")
            && let Some(prefix) =
                glob_import_binding(&bindings.imports, source, module_index)
        {
            owners.push(join_path(prefix, &owner));
        }
    }
}
let mut keys = Vec::with_capacity(owners.len().saturating_mul(3));
for owner in owners {
    let target = join_path(&owner, method);
    keys.push(format!("rust:function:{target}"));
    keys.push(format!("rust:method:{target}"));
    keys.push(item_key(&target));
}
dedup_keys(keys)
```

This retains explicit-import authority, preserves lexical-owner priority, and produces no glob candidate for absolute `crate`, `self`, or `super` calls.

- [ ] **Step 7: Run focused and index tests and verify GREEN**

Run:

```text
cargo test index::tests::resolves_one_module_glob_after_lexical_candidates -- --exact
cargo test index::tests
```

Expected: the focused test and every index test pass with no warning.

- [ ] **Step 8: Verify Magnus's exact audit range**

Create an isolated checkout and build the current debug binary:

```text
audit_repro=$(mktemp -d /tmp/graphr-issue2.XXXXXX)
git clone --no-local /home/eike/workspace/github.com/infinite-research/infinite-rfq-platform "$audit_repro/repo"
git -C "$audit_repro/repo" checkout --detach ebee315
cargo build --locked
```

Start `target/debug/graphr serve "$audit_repro/repo"` in a terminal session. Send these newline-delimited MCP messages:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"issue-2-verification","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"changes","arguments":{"base":"165e3f7","depth":6,"max_nodes":50,"dependency_mode":"boundary"}}}
```

For every returned `graph_next_cursor=name` line, send another `changes`
request with `base`, `depth`, `max_nodes`, and `dependency_mode` unchanged and
`cursor` set to the complete text after the first `=`. Increment the JSON-RPC
request ID each time. Stop only when a response has no `graph_next_cursor`
line.

Expected after concatenating graph pages:

- The risk lines for `genesis_hash`, `verify_chain`, `validate_audit_chain_id`, `validate_text`, and `record_hash` do not contain `no-static-test-path` when a resolved static path exists, and state `test_path_confidence=heuristic` plus `test_path_provenance=resolved-static-call-graph`.
- Covered non-direct helpers may contain `indirect-test-covered`.
- `analysis_complete=true`, `changed_symbols_omitted=0`, and `review_complete_when_pages_exhausted=true` remain unchanged.

Human acceptance: the 0.5.0 module-glob fix and honest provenance are the
accepted scope. General Rust call extraction inside macro token trees and
inferred receiver typing are deferred to 0.6.0.

Use only the temporary clone for this reproduction; do not index or modify the source repository.

- [ ] **Step 9: Commit the resolver slice**

```text
git add src/index.rs
git commit -m "fix(rust): resolve calls through module globs"
```

---

### Task 3: Close the client-contract and repository-binding documentation gaps

**Required skill:** Read and use `superpowers:writing-skills` before modifying the bundled review skill.

**Files:**
- Modify: `README.md:25-38`
- Modify: `src/mcp.rs:190-230`
- Modify: `.agents/skills/graphr-review/SKILL.md:8-24`

**Interfaces:**
- Documents the existing standalone `name=value` cursor lines; no output shape changes.
- Documents existing `analysis_complete`, `review_complete`, and `review_complete_when_pages_exhausted` fields; no new completion field.
- Documents existing `graphr serve PATH` binding; no new head argument.

- [ ] **Step 1: Add the cursor and completeness contract to README**

After the paragraph beginning `The server indexes when it starts`, add concise prose with these exact rules:

```text
Each continuation token is emitted on its own `name=value` line. Split on the first `=`, preserve the complete value unchanged, and pass it back with the original arguments. Continue until all four cursor names are absent. `analysis_complete` is local to the graph or artifact analyzer; `review_complete=false` means the current response has not exhausted all pages; whole-change coverage is complete only after all cursors are absent and `review_complete_when_pages_exhausted=true`.
```

Do not introduce a JSON cursor example: the line grammar is deliberately plain text.

- [ ] **Step 2: Document one server per checked-out repository**

After the rename-detection paragraph, add:

```text
Each `graphr serve PATH` process is bound to that repository and working tree for its lifetime. `changes(base=...)` compares `base` with the bound working tree; it does not accept a separate head ref. To review a committed range whose head is not checked out, create a temporary clone or worktree at that head and run a separate Graphr server for it.
```

- [ ] **Step 3: Mirror the contract in MCP instructions**

Update the `changes` tool description and `ServerHandler::get_info` instructions without changing their schemas. Include these three facts in the existing compact strings:

```text
Continuation tokens are standalone name=value lines; split on the first = and return the complete value verbatim.
Analyzer-local analysis_complete is distinct from whole-change review_complete_when_pages_exhausted.
One server is bound to its startup repository path and working tree; use a separate server at the desired head for an unchecked-out committed range.
```

Keep the current instruction to call `index` only after current-session Rust or Python edits.

- [ ] **Step 4: Tighten the bundled review skill**

In step 3 of `.agents/skills/graphr-review/SKILL.md`, state that continuation tokens are standalone `name=value` lines and the value after the first `=` is passed verbatim. In step 6, retain the two-part terminal predicate: all cursor names absent, then `review_complete_when_pages_exhausted=true`.

Do not add a fallback repository scan or a second cursorless `changes` call.

- [ ] **Step 5: Pressure-test the instruction changes**

Using the `superpowers:writing-skills` verification method, test these two scenarios against the edited skill:

1. Initial output contains `diff_next_cursor=v1:d:2864:e65f6b8a50b15018df0a248e5d5d3353a709fd45b8fcd58624aded8a13e09554` and `review_complete=false`: the reviewer must extract the text after the first `=`, call `changes` with that exact cursor, and not conclude.
2. The requested range is `A..B` while the bound worktree is at `A`: the reviewer must report the binding limitation or use an explicitly supplied separate server at `B`, never claim an empty review.

Expected: both scenarios follow the specified behavior without adding repository-wide fallback reads.

- [ ] **Step 6: Run documentation-adjacent checks**

Human prose earns no brittle source-text snapshot. Run the existing real MCP and formatting checks instead:

```text
cargo fmt --check
cargo test mcp::tests
git diff --check
```

Expected: all commands pass.

- [ ] **Step 7: Commit the documentation slice**

```text
git add README.md src/mcp.rs .agents/skills/graphr-review/SKILL.md
git commit -m "docs: clarify review completion and repository binding"
```

---

### Task 4: Prepare and verify Graphr 0.5.0

**Required skills:** Use `graphr-review` for the completed implementation diff
and `superpowers:verification-before-completion` before any success claim.

**Files:**
- Modify: `Cargo.toml:1-6`
- Modify: `Cargo.lock` package entry for `graphr`

**Interfaces:**
- Produces: `env!("CARGO_PKG_VERSION") == "0.5.0"` in CLI and MCP server metadata.
- Produces: a locally verified `graphr-0.5.0.crate` under an isolated Cargo target directory.

- [ ] **Step 1: Review the completed implementation diff**

Resolve the plan commit as the review base:

```text
git log -1 --format=%H -- docs/superpowers/plans/2026-08-11-magnus-feedback-release-0.5.0.md
```

Invoke `$graphr-review` on the current branch using that commit as `base`.
Because Tasks 1 and 2 changed Rust source after Graphr started, call `index`
once, then exhaust every returned files, diff, artifacts, and graph cursor.

Expected: no correctness, regression, safety, or coverage blocker. If review
finds a real defect, reproduce it with a failing test, apply the minimum root
fix, rerun the focused test green, and commit the fix before continuing.

- [ ] **Step 2: Bump the package version in both Cargo files**

Change only the local Graphr package version:

```toml
# Cargo.toml
version = "0.5.0"
```

```toml
# Cargo.lock, [[package]] name = "graphr"
version = "0.5.0"
```

Do not update dependencies.

- [ ] **Step 3: Verify the consumer-visible version**

Run:

```text
cargo run --locked --quiet -- --version
cargo test --test cli version_is_stable -- --exact
```

Expected output from the first command:

```text
graphr 0.5.0
```

Expected: the CLI integration test passes and stderr remains empty.

- [ ] **Step 4: Commit the release version**

```text
git add Cargo.toml Cargo.lock
git commit -m "chore(release): prepare 0.5.0"
```

- [ ] **Step 5: Run all required repository checks from a clean tree**

Run each command separately:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
git status --short --branch
```

Expected:

- Formatting and Clippy pass with no warning.
- All unit, CLI, and E2E tests pass.
- The locked release build succeeds.
- `git diff --check` emits nothing.
- The working tree is clean and `main` is ahead of `origin/main` only by committed work.

- [ ] **Step 6: Verify the package in an isolated target directory**

Use a fresh temporary directory so Cargo package verification cannot contaminate the normal debug fingerprint:

```text
package_target=$(mktemp -d /tmp/graphr-0.5.0-package.XXXXXX)
cargo package --locked --offline --target-dir "$package_target"
```

Expected:

- Cargo builds and verifies `graphr v0.5.0` successfully.
- The package contains the Rust sources, queries, README, Cargo files, and license metadata expected by `Cargo.toml`.
- No tag, push, publish, or GitHub mutation occurs.

- [ ] **Step 7: Prepare issue closure evidence without posting it**

Record these exact evidence points in the final handoff, one per issue:

```text
#1: README/MCP/skill document standalone name=value cursor parsing and the terminal predicate.
#2: The exact 165e3f7..ebee315 reproduction resolves the five named covered symbols where a static path exists, while retaining heuristic confidence and resolved-static-call-graph provenance; macro token-tree extraction and inferred receiver typing are deferred to 0.6.0.
#3: Documentation distinguishes analyzer-local analysis_complete from whole-change review_complete_when_pages_exhausted, with artifact omissions enumerated.
#4: README/MCP document one server per bound worktree and the temporary checkout workflow.
```

Leave GitHub issues #1 through #4 open until the verified commits are pushed and a later explicit approval authorizes comments or closure.
