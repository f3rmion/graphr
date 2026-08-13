# Graphr 0.6.1 macOS Portability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `graphr index`, `serve`, and the full test suite run on macOS and Linux by replacing the four functions that reconstruct a path from an open descriptor via Linux `/proc/self/fd` with descriptor-native equivalents, then release 0.6.1 with macOS as a required CI job.

**Architecture:** No new module, no new dependency, no schema change, no cache-layout change. `src/workspace.rs` already uses the portable `*at` family (`mkdirat` :1172, `openat` :1237, `unlinkat` :1290, `renameat` :1301). This plan collects those into one named boundary, replaces the four Linux-only functions with descriptor-native calls that resolve *fewer* path components than today, deletes the `/proc` predicates, and enables three trust-boundary checks that are currently suppressed because of `/proc`.

**Tech Stack:** Rust 2024, Rust standard library, `libc` 0.2.189 (already a dependency), Git CLI, BLAKE3, SQLite/rusqlite, Cargo.

**Source design:** `docs/superpowers/specs/2026-08-12-macos-portability-design.md`.

## Global Constraints

- Build one Rust binary for Codex and Claude over MCP stdio.
- Add no crate. `libc` supplies every syscall this plan needs.
- No `cfg(target_os)` branch in the cache layer. One code path for Linux and macOS.
- Do not port `/proc/self/fd` to `/dev/fd` or `F_GETPATH`. Both are unsound; the design records the measurements.
- Do not write a custom SQLite VFS and do not hook `xSetSystemCall` / `aSyscall["open"]`.
- Do not use `AT_SYMLINK_NOFOLLOW_ANY`, `AT_RESOLVE_BENEATH`, or `AT_UNIQUE`. macOS-only and absent from `libc` 0.2.189.
- No change to the on-disk cache layout, the `graphr/v6` namespace, the manifest format, `snapshot_id` derivation, `SCHEMA_VERSION`, or any MCP wire contract. Nothing in this milestone is breaking.
- Do not implement anything from `2026-08-12-review-coverage-design.md`. That is 0.7.0 and is out of scope.
- Preserve canonical-path checks, regular-file and size checks, capture validation, Git deadlines, cancellation, SQLite rollback, and explicit incomplete-coverage semantics.
- Use test-driven development for every production behavior: write the failing test first, then the implementation.
- Every task's tests must pass on **both** Linux and macOS. Until Task 5 lands, `cargo test` cannot pass on macOS, so per-task macOS verification runs the tests that task touches, not the whole suite.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build --locked --release` before the completion claim.

---

### Task 0: Canonicalise the test temp root — COMPLETE

**Problem:** Discovered during execution, not planning. Of the 30 `cargo test`
failures on macOS, six were **not** the `/proc` defect. The test helpers
`temp_root` (`src/git.rs:6152`, `src/workspace.rs:2951`) return
`std::env::temp_dir().join(..)` without canonicalising. On macOS `TMPDIR` is
under `/var/folders/...`, and `/var` is a symlink to `/private/var`, so
`fs::canonicalize(candidate) == candidate` — the regular-file safety check at
`src/git.rs:1879`, `:1896`, and `:2091` — is never true and untracked source
files are silently dropped.

**This is test hygiene, not a product defect.** Production canonicalises the
root at every entry point (`src/git.rs:374`, `:396`; `src/workspace.rs:314`,
`:361`), so a real repository never reaches that check with a non-canonical
root. The helpers constructed a root production cannot produce. Note the
sibling helper `private_dir` (`src/git.rs:6120`) already canonicalises — the
two helpers had drifted.

**Files:**
- Modify: `src/git.rs:6152` (`temp_root`)
- Modify: `src/workspace.rs:2951` (`temp_root`)

**Steps:**

- [x] Confirm the mechanism: `/var/folders/…` canonicalises to
      `/private/var/folders/…` on darwin 25.5.0.
- [x] Verify by canonicalising the root in one failing test in isolation and
      observing it pass.
- [x] Canonicalise `std::env::temp_dir()` in both helpers before joining, with
      `unwrap_or_else` falling back to the uncanonicalised base.
- [x] Leave passing tests untouched — an inline third site was reverted to keep
      the change minimal.
- [x] Measure the split: 30 failures become 24, all six cleared are
      `git::tests`, and no new failure appears.

**Verification:** `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` pass. macOS failures drop from 30
to 24, and the remaining 24 (`index` 11, `workspace` 12, `store` 1) are the
`/proc` defect that Tasks 1 through 5 address. This task changes no production
code, so Linux behaviour is unaffected.

---

### Task 1: Mark the descriptor boundary — COMPLETE (premise corrected)

**The planned premise was wrong.** This task was written as "the descriptor
operations are scattered through `src/workspace.rs` with inconsistent naming".
They are not. Verified before starting: `cache_child`, `create_child_directory`,
`open_child_directory`, `component_cstring`, `create_file_at`,
`open_regular_at`, `link_file_at`, `unlink_at`, `rename_at`, and
`entry_exists_at` already occupy one contiguous block at
`src/workspace.rs:1138-1347`, with consistent `_at` naming. Higher-level
functions begin at `private_name` (`:1349`) and path-based ones at
`open_regular` (`:1420`).

The planned renames — `open_child_directory` → `open_child`,
`create_child_directory` → `create_child` — were **dropped**. They are churn
across every call site for no behavioural or structural gain, and `AGENTS.md`
favours the simplest change that meets the requirement. `link_at`, `stat_at`,
and `read_dir_at` arrive with Tasks 2, 5, and 4 respectively, where they have
callers; adding them here would have landed unreachable code.

What remained genuinely useful, and what was done: a header comment marking the
block as the portability seam, stating the single-component invariant, naming
the Windows mapping for each primitive, and forbidding the reintroduction of a
path-taking helper.

**Files:**
- Modify: `src/workspace.rs:1138` (section header comment only)

**Steps:**

- [x] Verify the premise before acting; record that it did not hold.
- [x] Add the seam header comment above `cache_child`.
- [x] Drop the planned renames as unjustified churn.

**Verification:** Comment-only change. `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` pass; macOS failures stay at 24,
confirming behaviour is untouched.

---

### Task 2: Replace `link_file_at` with `linkat` on a descriptor pair

**Problem:** `link_file_at` (`src/workspace.rs:1266`) calls `linkat(AT_FDCWD, "/proc/self/fd/N", targetdir, name, AT_SYMLINK_FOLLOW)`. Its only caller, `publish_no_replace` (`:1370`), *already holds* `source_directory` and `source_name` and opens `source` from them at `:1375`. The `/proc` round-trip discards that pair and rebuilds a path. It is gratuitous even on Linux.

POSIX guarantees both properties the call site depends on: `link` "shall atomically create a new hard link", and `[EEXIST]` when the target exists — the `Ok(false)` "someone else published first" branch at `:1389`.

**Files:**
- Modify: `src/workspace.rs:1266-1288` (`link_file_at` → `link_at`)
- Modify: `src/workspace.rs:1370-1400` (`publish_no_replace`)
- Test: `src/workspace.rs`

**Interfaces:**

```rust
// linkat(src_fd, src_name, dst_fd, dst_name, 0).
// On macOS some filesystems return ENOTSUP for flags = 0; retry once with
// AT_SYMLINK_FOLLOW. Safe here because the caller opened src_name with
// O_NOFOLLOW and confirmed is_file(), so it is not a symlink and following
// is a no-op. Record that reasoning at the call site.
fn link_at(
    source_directory: &CacheDirectory,
    source_name: &OsStr,
    target_directory: &CacheDirectory,
    target_name: &OsStr,
) -> std::io::Result<()>;
```

**Steps:**

- [ ] Write a failing test: publishing into a directory where the target name already exists returns the "already published" branch, not an error and not a replacement.
- [ ] Write a failing test: publishing a fresh name links the file and leaves the source private name in place until the explicit `unlink_at` at `:1392`.
- [ ] Write a failing test: the published entry and the source refer to the same inode (same device and inode via `stat_at`).
- [ ] Implement `link_at` using `libc::linkat` with both directory descriptors, relative names, and `flags = 0`.
- [ ] Add the single `ENOTSUP` retry with `AT_SYMLINK_FOLLOW`, and a comment recording why it is safe.
- [ ] Change `publish_no_replace` to pass `source_directory` and `source_name` instead of the opened `source` handle; keep the `openat` + `O_NOFOLLOW` + `is_file()` validation at `:1376-1386`, which the retry's safety argument depends on.
- [ ] Delete `link_file_at`.

**Verification:** `cargo test` passes on Linux. The publish tests pass on macOS. No `/proc` string remains in the publish path.

---

### Task 3: Hash and validate through the open descriptor

**Problem:** `hash_file` (`src/workspace.rs:1479`) takes a `&Path` and reopens it via `open_regular` (`:1420`). Every caller — `:618`, `:829`, `:1038`, `:1529` — already holds the `File` and converts it to a `/proc` path solely to satisfy that signature. `validate_published_image` (`:1501`) branches on `is_process_descriptor_path` at `:1503` to choose `fs::metadata` over `fs::symlink_metadata`, purely because `/proc/self/fd/N` is a symlink on Linux.

**Files:**
- Modify: `src/workspace.rs:1479-1499` (`hash_file`)
- Modify: `src/workspace.rs:1501-1513` (`validate_published_image`)
- Modify: `src/workspace.rs:618`, `:829`, `:1038`, `:1529` (call sites)
- Test: `src/workspace.rs`

**Interfaces:**

```rust
// Reads through FileExt::read_at with an explicit running offset, so the
// shared file offset is never mutated and no try_clone or extra descriptor
// is needed. Zero path resolution.
fn hash_file(file: &File, cancelled: &AtomicBool) -> Result<String, OperationError>;
```

**Steps:**

- [ ] Write a failing test: hashing a fixture file through the descriptor returns the same digest the path-based implementation produced for that fixture.
- [ ] Write a failing test: hashing does not disturb the file offset of a concurrently held handle to the same file.
- [ ] Reimplement `hash_file` to take `&File` and read via `std::os::unix::fs::FileExt::read_at` with an explicit offset, preserving the existing 64 KiB buffer and cancellation checks.
- [ ] Update all four call sites to pass the `File` they already hold, deleting the `stable_file_path` conversion at each.
- [ ] Replace the `is_process_descriptor_path` branch in `validate_published_image` with `file.metadata()`, an `fstat` on the pinned inode.
- [ ] Confirm `stable_file_path` has no remaining callers.

**Verification:** `cargo test` passes on Linux; the hashing and validation tests pass on macOS. `stable_file_path` is unreferenced.

---

### Task 4: Enumerate the catalog through `fdopendir`

**Problem:** The catalog scan at `src/workspace.rs:517` calls `fs::read_dir(stable_directory_path(directory))`. This is the last `stable_directory_path` consumer that genuinely needs enumeration; `CacheDirectory::child` (`:1113`) already carries a real `path` field.

**Constraint — this is the subtle task.** macOS `DIRECTORY(3)` states that after `fdopendir` the descriptor "is under the control of the system", any other use is undefined behaviour, and `closedir` closes it. Handing the long-lived pinned `CacheDirectory` descriptor to `fdopendir` would close it out from under `Arc<File>` and make the `sync_all` at `:1126` UB. `dup` is not a fix: dup'd descriptors share one file description and therefore one directory position.

**Files:**
- Modify: `src/workspace.rs:502-556` (`SnapshotCatalog::attach` scan)
- Modify: `src/workspace.rs` (add `read_dir_at` to the Task 1 boundary)
- Test: `src/workspace.rs`

**Interfaces:**

```rust
// Obtains an INDEPENDENT description via openat(dir, ".") before fdopendir,
// per rustix::fs::Dir. Returns owned names; the DIR* never escapes.
fn read_dir_at(directory: &CacheDirectory) -> Result<Vec<OsString>, OperationError>;

// RAII wrapper. Drop calls closedir exactly once. Deliberately does NOT
// implement AsRawFd: exposing the descriptor after fdopendir is UB.
struct DirStream(*mut libc::DIR);
```

**Steps:**

- [ ] Write a failing test: enumerating a directory returns every entry, and the caller's long-lived `CacheDirectory` descriptor is still usable afterwards (`sync()` succeeds).
- [ ] Write a failing test: two consecutive enumerations of the same directory return identical entry sets, proving the directory position was not shared.
- [ ] Write a failing test: enumeration of a directory containing a name with non-UTF-8 bytes preserves that name.
- [ ] Implement `DirStream` with a `Drop` that calls `closedir` once, and no `AsRawFd`.
- [ ] Implement `read_dir_at`: `openat(dir_fd, ".", O_RDONLY | O_CLOEXEC | O_DIRECTORY)` for an independent description, then `fdopendir`; on `fdopendir` failure close the descriptor explicitly, since ownership did not transfer.
- [ ] Loop `readdir`, clearing `errno` before each call to distinguish end-of-directory from error; skip `.` and `..`; collect `OsString` from the `d_name` bytes.
- [ ] Replace the `fs::read_dir` call at `:517`, keeping the existing `manifests.sort_by_key` so ordering stays deterministic despite unspecified `readdir` order.
- [ ] Do not read `d_type`; macOS may report `DT_UNKNOWN`. The existing loop stats separately.
- [ ] Confirm `stable_directory_path` has no remaining callers.

**Verification:** `cargo test` passes on Linux; the catalog tests pass on macOS. `stable_directory_path` is unreferenced. No code path calls `close` on a descriptor owned by a `DIR*`.

---

### Task 5: Delete the `/proc` predicates and enable the three suppressed checks

**Problem:** With no `/proc` paths remaining, `is_process_descriptor_path` (`src/workspace.rs:1437`), `is_process_descriptor_directory` (`src/store.rs:836`), and `has_process_descriptor_boundary` (`src/store.rs:855`) are dead. Three trust-boundary checks are currently disabled *because* of `/proc` and become unconditional. This is the task that makes `cargo test` pass on macOS.

**Files:**
- Modify: `src/workspace.rs:1420-1435` (`open_regular`), `:1437-1450` (delete predicate)
- Modify: `src/store.rs:173-203` (`open_with_parent`), `:228-232`, `:806-815`, `:836-858` (delete predicates)
- Test: `src/workspace.rs`, `src/store.rs`, `tests/e2e.rs`

**Steps:**

- [ ] Write a failing test: `open_regular` refuses a symlink unconditionally.
- [ ] Write a failing test: a database path with a `-wal`, `-shm`, or `-journal` sibling is rejected by `require_no_sidecars` (`src/store.rs:860`), proving the check is live rather than vacuous.
- [ ] Write a failing test: opening a graph image whose name has been swapped for a different inode between validation and open is detected by the post-open device/inode comparison.
- [ ] Make `O_NOFOLLOW` unconditional in `open_regular`, deleting the `:1423` branch.
- [ ] Make `SQLITE_OPEN_NOFOLLOW` unconditional at all three sites: `src/store.rs:200-202` (the `descriptor_parent` branch), `:229-231`, and `:811-813`.
- [ ] Add post-open identity verification where SQLite opens a path: `stat_at` the name before open, compare device and inode against the opened handle afterwards, and fail with the existing cache-corrupt error on mismatch.
- [ ] Change `src/store.rs:182` from `fs::create_dir` to a `mkdirat`-based create through the pinned parent, or confirm the parent is guaranteed to exist and document why; the current non-recursive `create_dir` is what surfaces as `cannot create database directory` on macOS.
- [ ] Delete `is_process_descriptor_path`, `is_process_descriptor_directory`, `has_process_descriptor_boundary`, `stable_directory_path`, and `stable_file_path`.
- [ ] Grep `src/` for `proc/self/fd` and confirm zero matches.

**Verification:** `cargo test` passes on **both** Linux and macOS — this is the first task after which that is true. `graphr index --worktree-root "$PWD" --base HEAD~1 --head HEAD --target commit` succeeds on macOS, followed by a `changes` query against the returned `snapshot_id`.

---

### Task 6: Release 0.6.1 and make macOS a required check

**Problem:** The macOS CI job added in `.github/workflows/ci.yml` is red by design until Task 5 lands. Turning it green is the completion signal for this milestone.

**Files:**
- Modify: `Cargo.toml:3`
- Modify: `Cargo.lock`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`

**Steps:**

- [ ] Set `version = "0.6.1"` in `Cargo.toml` and refresh `Cargo.lock` with a locked build.
- [ ] Remove the "expected to fail until that is fixed" comment from the CI matrix, since the job now passes.
- [ ] State supported platforms in `README.md`: Linux and macOS. Do not claim Windows.
- [ ] Verify `cargo package` succeeds.
- [ ] Run the manual macOS acceptance from Task 5's verification one final time against a real repository.

**Verification:** All four required checks pass on both `ubuntu-latest` and `macos-latest`. `cargo package` succeeds. The README names the supported platforms.

---

## Requirements Traceability

| Design acceptance test | Task |
|---|---|
| 1. `graphr index` completes on macOS and publishes a queryable snapshot | 5 |
| 2. `search`, `view`, `changes` return results on macOS | 5 |
| 3. Publishing an existing target name returns "already published" | 2 |
| 4. Publishing succeeds where `linkat` rejects `flags = 0` | 2 |
| 5. Catalog scan enumerates all manifests; pinned descriptor still usable | 4 |
| 6. Two consecutive scans return identical entries | 4 |
| 7. Hashing matches the previous digest and preserves file offset | 3 |
| 8. A replaced published image fails validation and is quarantined | 5 |
| 9. `open_regular` refuses a symlink unconditionally | 5 |
| 10. `require_no_sidecars` rejects a `-wal` sibling | 5 |
| 11. No `/proc` or `*_process_descriptor_*` symbol remains in `src/` | 5 |

## Out of Scope

Windows. The design's Platform Scope section records what it would additionally require — a `windows-sys` dependency, `ntdll` for handle-relative open and hard link, a rewrite of `src/git.rs`'s byte-oriented `OsStrExt` handling, and a different `read_at` story. The Task 1 boundary is the seam a Windows backend would replace; nothing else in this plan anticipates it.

Everything in `2026-08-12-review-coverage-design.md`: the evaluation harness, relevance ordering, evidence tiers, inference rules, weighted impact traversal, entry-point changes, and the three coverage defects. Those are 0.7.0 and land after this milestone.
