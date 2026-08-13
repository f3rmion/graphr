# Graphr 0.7.0 Coverage Defects Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Repair three correctness defects in the shipped 0.6.0 review surface — a silently shallow default blast radius, a two-dot diff where review needs merge-base, and a valid snapshot that reports as missing after a server restart — and release them as 0.7.0.

**Architecture:** No new module, no new dependency, no schema change. Task 1 removes two `serde` defaults in `src/mcp.rs`. Task 2 resolves the review baseline to a merge-base inside the existing one-shot ref resolution in `src/workspace.rs`, so every downstream consumer — capture, `snapshot_id`, provenance — is unchanged and automatically correct. Task 3 adds a bounded attach-and-retry to the single snapshot lookup in `src/index.rs`.

**Tech Stack:** Rust 2024, Rust standard library, Git CLI, existing BLAKE3, serde, rmcp stdio, SQLite/rusqlite, Cargo.

**Source design:** `docs/superpowers/specs/2026-08-12-review-coverage-design.md`, section "Coverage Defects in 0.6.0".

## Global Constraints

- Build one Rust binary for Codex and Claude over MCP stdio.
- Rust and Python remain the only indexed source languages.
- Add no crate, migration, compatibility layer, fallback, HTTP surface, UI, editor integration, embeddings, or plugin system.
- `SCHEMA_VERSION` does not change in this milestone. No task may alter the SQLite schema.
- Do not implement evidence tiers, inference rules, relevance ranking, weighted impact traversal, or entry-point changes. Those are later milestones in the source design and are out of scope here.
- Keep output deterministic, compact, line-safe, independently paged, and bounded by the existing response budgets.
- Preserve no-follow reads, canonical-path checks, regular-file and size checks, capture validation, Git deadlines, cancellation, SQLite rollback, and explicit incomplete-coverage semantics.
- Treat Git refs as labels only. Resolve base and head once and use exact OIDs thereafter.
- Never write HEAD, refs, the worktree index, Git objects, attributes, configuration, or worktree files.
- Use the standard library and existing dependencies only.
- Use test-driven development for every production behavior: write the failing test first, then the implementation.
- Do not use any customer, incident, or ticket-specific name in fixtures, docs, code, or commit messages.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build --locked --release` before the completion claim.

---

### Task 1: Make `changes` depth and node budget explicit

**Problem:** `ChangesParams` defaults `depth` to `1` while `get_info` instructs the agent to call with depth 6. A client that omits the parameter silently receives a six-times-shallower blast radius with no error. `index` already requires all five of its parameters; `changes` should match.

**Files:**
- Modify: `src/mcp.rs:117-132` (`ChangesParams`)
- Modify: `src/mcp.rs:439-445` (`default_changes_depth`, `default_changes_max_nodes`)
- Test: `src/mcp.rs` (unit tests module)
- Test: `tests/e2e.rs`

**Interfaces:**

```rust
#[derive(Clone, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct ChangesParams {
    #[schemars(length(min = 64, max = 64))]
    snapshot_id: String,
    #[schemars(range(min = 0, max = 6))]
    depth: u32,
    #[schemars(range(min = 1, max = 50))]
    max_nodes: u32,
    #[serde(default)]
    #[schemars(length(min = 1, max = 160))]
    cursor: Option<String>,
}
```

**Steps:**

- [ ] Write a failing unit test asserting that deserializing a `changes` payload without `depth` produces the structured invalid-parameters error, and the same for `max_nodes`.
- [ ] Write a failing unit test asserting a payload carrying `snapshot_id`, `depth`, and `max_nodes` still deserializes, and that `cursor` remains optional.
- [ ] Remove `#[serde(default = "default_changes_depth")]` and `#[serde(default = "default_changes_max_nodes")]` from `ChangesParams`.
- [ ] Delete `default_changes_depth` and `default_changes_max_nodes`; confirm no other caller references them.
- [ ] Confirm the emitted JSON schema marks `depth` and `max_nodes` required, so a schema-aware client sees the contract.
- [ ] Extend one existing `tests/e2e.rs` `changes` call path to assert the omitted-parameter error surfaces as a structured error rather than a silent shallow result.
- [ ] Verify every in-repo caller — `tests/e2e.rs`, `tests/cli.rs`, and `.agents/skills/graphr-review/SKILL.md` — already passes both parameters explicitly, and update any that do not.

**Verification:** `cargo test` passes. A `changes` call omitting `depth` returns a structured error naming the missing parameter. No call site in the repository relies on a default.

---

### Task 2: Resolve the review baseline to a merge-base

**Problem:** `run_final_diff` (`src/git.rs:1418-1456`) pushes `base_oid` and `head_oid` as separate positional arguments to `git diff`, which is two-dot semantics. When the base branch has advanced past the branch point, commits the author never wrote are reported as changes. The bundled skill instructs the agent to select merge-base as the review base for a branch or pull request, so this is the common case, not an edge case.

**Approach:** Fix it once, at the single point where refs become OIDs. `resolve_request` (`src/workspace.rs:1721`) already resolves `base` and `head` exactly once and every consumer downstream — capture, `snapshot_id`, provenance — uses the resolved OIDs. Resolving `base_oid` to `merge-base(base, head)` there fixes all three targets with no change to `src/git.rs`, and makes `snapshot_id` correctly bind the commit that was actually diffed from.

The rule is uniform across `commit`, `index`, and `worktree` targets: a worktree reviewed against an advanced `main` exhibits the identical defect, and one rule is simpler than a per-target branch.

**Files:**
- Modify: `src/workspace.rs:1721-1760` (`resolve_request`)
- Modify: `src/workspace.rs:32-51` (`ErrorCode`) if a distinct unrelated-history code is warranted
- Test: `src/workspace.rs` (unit tests module, alongside `resolve_request_pins_base_and_head_oids` at `:2607`)
- Test: `tests/e2e.rs`

**Interfaces:**

```rust
// Resolves the merge-base of the requested base and the resolved head.
// Returns the merge-base OID, which becomes the request's base_oid.
// Errors when the two commits share no ancestor.
fn resolve_merge_base(
    repository_root: &Path,
    base_oid: &str,
    head_oid: &str,
    cancelled: &AtomicBool,
) -> Result<String, OperationError>;
```

**Steps:**

- [ ] Write a failing unit test: a repository where `base` has advanced past the branch point resolves `base_oid` to the branch point, not to the tip of `base`.
- [ ] Write a failing unit test: two commits with unrelated histories produce a structured error rather than a full-tree diff.
- [ ] Write a failing unit test: when `base` is already an ancestor of `head`, the resolved `base_oid` is unchanged, so existing snapshots keep their identity.
- [ ] Implement `resolve_merge_base` using `git merge-base <base_oid> <head_oid>` through the existing `run_git` helper, honouring the existing deadline, output limits, and cancellation flag.
- [ ] Call it from `resolve_request` after both refs resolve, and assign the result as the request's `base_oid`.
- [ ] Extend `resolve_request_pins_base_and_head_oids` (`src/workspace.rs:2607`) to cover the advanced-base case.
- [ ] Add a `tests/e2e.rs` case: a commit-target review whose base branch has advanced reports only head-side changes.
- [ ] Add a `tests/e2e.rs` case: a worktree-target review against an advanced base reports only worktree-side changes.
- [ ] Confirm `git.rs` is unmodified by this task.

**Verification:** `cargo test` passes. A review whose base branch advanced no longer reports base-side commits. Unrelated histories fail with a structured error. A base that is already an ancestor produces a byte-identical `snapshot_id` to the previous behaviour.

---

### Task 3: Attach on snapshot lookup miss, exactly once

**Problem:** `SnapshotCatalog::attach` (`src/workspace.rs:502`) runs only from `build_snapshot` (`src/index.rs:98`) and `inspect_root` (`src/index.rs:368`). After a server restart, a valid on-disk `snapshot_id` returns `SnapshotNotFound` (`src/workspace.rs:1600`) until something re-attaches its root, so a resumed review sees a false "snapshot gone".

**Constraint:** `attach` takes a `RootIdentity`, and nothing maps a content-addressed `snapshot_id` back to its owning root, so "attach the owning root" is not derivable at lookup time. The only implementable shape is to attach every allowed root. `attach` is expensive and destructive — it enumerates and revalidates every manifest, hashes graph images, and calls `reconcile` (`src/workspace.rs:558`), which evicts loaded snapshots whose manifest has disappeared — so the rescan must be bounded to one attempt per lookup.

**Files:**
- Modify: `src/index.rs:65-99` (`Engine::snapshot`)
- Modify: `src/workspace.rs` (`SnapshotCatalog`) to expose a bounded refresh
- Test: `src/workspace.rs` (unit tests module)
- Test: `tests/e2e.rs`

**Interfaces:**

```rust
impl Engine {
    // On a catalog miss, inspect and attach every allowed root once,
    // then retry the lookup once. Never rescans more than once per call.
    pub fn snapshot(&self, snapshot_id: &str) -> Result<Arc<SnapshotEntry>, OperationError>;
}
```

**Steps:**

- [ ] Write a failing unit test: a catalog with no attached roots resolves a snapshot id that exists on disk, after exactly one rescan.
- [ ] Write a failing unit test: an unknown snapshot id triggers at most one rescan and then returns `SnapshotNotFound`; assert the rescan count, not just the error.
- [ ] Write a failing unit test: a rescan triggered by a lookup miss does not evict a snapshot that is still loaded and whose manifest is still present.
- [ ] Implement the bounded attach-and-retry in `Engine::snapshot`, iterating the allowed roots, honouring cancellation, and tolerating a per-root attach failure without failing the whole lookup.
- [ ] Confirm the retry cannot recurse: the second lookup never triggers a further rescan.
- [ ] Add a `tests/e2e.rs` case: build a snapshot, drop and rebuild the engine to simulate a restart, and query `changes` with the retained `snapshot_id` without an intervening `inspect_root`.
- [ ] Confirm `search` and `view` inherit the fix through the same lookup, and assert one of them in the restart test.

**Verification:** `cargo test` passes. A retained `snapshot_id` resolves after a restart with no intervening `inspect_root`. An unknown id costs at most one catalog rescan. No loaded snapshot is evicted by a lookup-triggered rescan.

---

### Task 4: Release 0.7.0

**Problem:** Task 1 makes `depth` and `max_nodes` required, which rejects a previously valid `changes` call from any client that is not the bundled skill. A breaking wire change cannot ship as a patch release.

**Files:**
- Modify: `Cargo.toml:3`
- Modify: `Cargo.lock`
- Modify: `README.md`

**Steps:**

- [ ] Set `version = "0.7.0"` in `Cargo.toml` and refresh `Cargo.lock` with a locked build.
- [ ] Update `README.md` where it documents `changes` parameters, stating that `depth` and `max_nodes` are required and that the review baseline is the merge-base of `base` and `head`.
- [ ] Confirm `README.md` carries no benchmark claim that this milestone invalidates; benchmark removal belongs to the measurement milestone and is not in scope here.
- [ ] Verify `cargo package` succeeds.

**Verification:** All four required checks pass on Linux. `cargo package` succeeds. The README describes the required parameters and the merge-base baseline.

---
## Requirements Traceability

| Source design item | Task | Acceptance test |
|---|---|---|
| `changes` called without `depth` fails with a structured error | 1 | Design acceptance test 16 |
| Commit-target request whose base branch advanced reports only head-side changes | 2 | Design acceptance test 17 |
| Snapshot id valid on disk resolves after restart; unknown id triggers at most one rescan | 3 | Design acceptance test 18 |
| Breaking wire change ships as a minor release | 4 | `Cargo.toml` version, `cargo package` |

## Out of Scope

**Blocking prerequisite discovered during planning — macOS portability.** Graphr
0.6.0 cannot index on macOS. `stable_directory_path` and `stable_file_path`
(`src/workspace.rs:1130-1136`) construct `/proc/self/fd/{fd}` paths and
`src/workspace.rs:1271` passes one to `linkat`; `/proc/self/fd` is Linux-only,
so `src/store.rs:182`'s non-recursive `fs::create_dir` fails with
`cannot create database directory: No such file or directory`. Reproduced with
`graphr index --worktree-root "$PWD" --base HEAD~1 --head HEAD --target commit`
on darwin 25.5.0, and it accounts for all 30 `cargo test` failures on that
platform. `cargo fmt`, `cargo clippy`, and `cargo build --locked --release` all
pass, so only `cargo test` and runtime are affected. The crate is published, so
every macOS installation is affected. This needs its own design and plan and
should land before or alongside this milestone.

Deferred to later milestones in the source design, and explicitly not
implemented here: the evaluation harness and every benchmark, README benchmark
removal, relevance ordering and BM25, evidence tiers and `ambiguous_callers`,
inference rules and the `inferred` tier, weighted impact traversal,
`Test`-node entry points, and any `SCHEMA_VERSION` change.
