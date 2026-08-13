# Graphr macOS Portability Design

## Context

Graphr 0.6.0 cannot index on macOS. The first command a user runs fails:

```text
$ graphr index --worktree-root "$PWD" --base HEAD~1 --head HEAD --target commit
graphr: cannot create database directory: No such file or directory (os error 2)
```

The crate is published, so every macOS installation is affected. All 30
`cargo test` failures on darwin 25.5.0 have this single cause. `cargo fmt`,
`cargo clippy`, and `cargo build --locked --release` all pass, which is why it
shipped unnoticed: the defect is invisible to every check the project runs, and
there is no CI.

The cause is narrow. `src/workspace.rs` already uses the portable `*at` family
correctly — `mkdirat` (:1172), `openat` (:1237), `unlinkat` (:1290), `renameat`
(:1301) all work on macOS. Exactly four functions reconstruct a *path* from an
open descriptor using Linux's `/proc/self/fd`, which does not exist on macOS:

| Function | Line | Consumers |
| --- | --- | --- |
| `stable_directory_path` | 1130 | `fs::read_dir` (:517), `CacheDirectory::child` (:1113) |
| `stable_file_path` | 1134 | hashing and image validation (:618, :829, :1038, :1529) |
| `link_file_at` | 1266 | `publish_no_replace` (:1387) |
| `is_process_descriptor_path` | 1437 | `O_NOFOLLOW` branch (:1423), metadata branch (:1503) |

`/proc/self/fd/N` is a Linux *magic link*: resolving it re-reaches the pinned
inode regardless of later renames. It is an adapter that lets a path-taking API
operate on a pinned descriptor without re-walking an attacker-mutable prefix. It
is not a security primitive in its own right.

**There is no macOS equivalent, and every apparent substitute is a trap.**
Measured on darwin 25.5.0:

- `/dev/fd/N` for a *directory* fd: `open()` returns `ENOTDIR`, and
  `/dev/fd/N/child` returns `ENOENT`. It is not a namespace entry point. Worse,
  `stat()` reports it *as a directory*, so `metadata.is_dir()` returns true and
  a naive port would pass the check at `store.rs:189` and fail later.
- `/dev/fd/N` for a *regular file* fd works, but is `dup`, not reopen: the file
  offset is shared, the mode cannot be upgraded, and — unlike Linux — it is not
  a symlink, which inverts the `O_NOFOLLOW` and `symlink_metadata` branches.
- `fcntl(fd, F_GETPATH)` returns a snapshot string of one name the inode once
  had. It is firmlink-resolved, ambiguous for hard-linked files (which this
  cache deliberately creates), and returns a stale path with `rc=0` after
  `unlink`. Reopening its result is a genuine TOCTOU hole. It would look like a
  port and would silently reintroduce every race the `/proc` form closes.

## Goals

- `graphr index`, `serve`, and the full test suite run on macOS and Linux.
- One code path. No `cfg(target_os)` branch in the cache layer.
- No new dependency.
- No reduction in trust-boundary strength, stated claim by claim rather than
  asserted globally.

## Product Boundaries

Carried unchanged:

- One Rust binary over MCP stdio; Rust and Python as the only indexed languages.
- No HTTP, UI, editor integration, embeddings, plugins, migrations, or
  compatibility layers.
- Use the standard library and existing dependencies only. `libc 0.2.189` is
  already a dependency and supplies every syscall this design needs.

Added:

- Do not port `/proc/self/fd` to `/dev/fd` or `F_GETPATH`. Both are unsound
  here, for the reasons measured above.
- Do not write a custom SQLite VFS. SQLite's own cut-down reference VFS is
  ~800 lines of C; a Rust equivalent is several hundred lines of
  `unsafe extern "C"` in the one component being hardened.
- Do not hook SQLite's `xSetSystemCall` / `aSyscall["open"]` table. It is
  reachable through `rusqlite::ffi` and would work, but it is process-global
  mutable state installed to avoid a problem this design removes by other means.
- Do not use `AT_SYMLINK_NOFOLLOW_ANY`, `AT_RESOLVE_BENEATH`, or `AT_UNIQUE`.
  macOS-only and absent from `libc 0.2.189`.
- No change to the on-disk cache layout, the `graphr/v6` namespace, manifest
  format, or `snapshot_id` derivation. This is a portability fix, not a format
  change.

## Architectural Decision

**Delete the need for a path rather than port the mechanism.** Each of the four
functions has a direct descriptor-native replacement that resolves fewer path
components than the `/proc` form does, not more.

This is the technique `cap-std` uses on every non-Linux platform: resolve
component by component through directory descriptors and never hand the kernel
a composite path. Graphr is already most of the way there — `component_cstring`
enforces single components, and the `*at` calls are already in place. The four
functions are the last sites where a path is reconstituted from a descriptor.

### 1. `link_file_at` → `linkat` with two directory descriptors

`publish_no_replace` (:1370-1400) already holds `source_directory` and
`source_name` as parameters and opens `source` from them at :1375. The
`/proc/self/fd` round-trip throws that pair away and rebuilds a path. It is
gratuitous **even on Linux**.

Replace with `linkat(srcdir_fd, srcname, dstdir_fd, dstname, 0)`. POSIX.1-2024
guarantees both properties the call site depends on: `link` "shall *atomically*
create a new hard link", and `[EEXIST]` when the target exists — which is
exactly the `Ok(false)` "someone else published first" branch at :1389. The
POSIX rationale names this as the intended use: opening descriptors for both
directories and using `linkat` guarantees both names are in the intended
directories "without exposure to race conditions". The `/proc` form exists only
to substitute for `AT_EMPTY_PATH` when the source has *no* name. Graphr's source
is a named private file, so neither is needed.

`AT_EMPTY_PATH` is rejected: Linux-only and requires `CAP_DAC_READ_SEARCH`.
`renameat` is rejected: it silently replaces an existing target, destroying the
`EEXIST` semantics; the no-replace variants (`renameat2 RENAME_NOREPLACE`,
`renameatx_np RENAME_EXCL`) are per-OS *and* per-filesystem, requiring a runtime
capability probe to replace one already-portable call.

One macOS caveat to handle explicitly: `link(2)` documents `[ENOTSUP]` when
`flags` is not `AT_SYMLINK_FOLLOW` "(some file systems only)". APFS and HFS+ do
not, and `cap-std` ships `AtFlags::empty()` on macOS in production. Ship
`flags = 0` and retry once with `AT_SYMLINK_FOLLOW` on `ENOTSUP`. The retry is
safe here specifically because the source was opened with `O_NOFOLLOW` and
confirmed `is_file()` at :1376-1386, so it is not a symlink and following is a
no-op. That reasoning must be recorded at the call site.

### 2. `stable_file_path` → operate on the open `File`

Every consumer already holds the descriptor and converts it to a path solely to
call a path-taking helper.

- `hash_file` takes `&File` and reads through
  `std::os::unix::fs::FileExt::read_at` with an explicit offset. `read_at` does
  not mutate the shared file offset, so no `try_clone` and no extra descriptor.
  Zero path resolution, against four components today.
- `validate_published_image`'s metadata branch collapses to `file.metadata()`
  — an `fstat` on the pinned inode. The `is_process_descriptor_path` branch at
  :1503 existed only because `/proc/self/fd/N` *is* a symlink on Linux.

### 3. `stable_directory_path` → `openat(".")` then `fdopendir`

The catalog scan at :517 is the only consumer that genuinely needs directory
enumeration. Obtain an **independent** description first:

```text
fd   = openat(dir_fd, ".", O_RDONLY | O_CLOEXEC | O_DIRECTORY)
dirp = fdopendir(fd)            // fd is now owned by dirp
...  readdir(dirp) ...
closedir(dirp)                  // closes fd; never close(fd) separately
```

The `openat(".")` reopen is mandatory, not stylistic. macOS `DIRECTORY(3)`
states that after `fdopendir` the descriptor "is under the control of the
system" and any other use is undefined behaviour, and `closedir` closes it —
which would close the long-lived pinned `CacheDirectory` handle out from under
`Arc<File>` and make the `sync_all` at :1126 UB. `dup` is not a fix: dup'd
descriptors share one file description and therefore one directory position.
This is precisely what `rustix::fs::Dir` does and why.

Wrap the `DIR*` in a type whose `Drop` calls `closedir` exactly once, and do not
implement `AsRawFd` on it. Distinguish `readdir` EOF from error by clearing
`errno` before each call. `d_type` may be `DT_UNKNOWN` on macOS; the existing
loop uses only `file_name()` and sorts afterwards, so neither matters.

`CacheDirectory::child` (:1113) already carries a real `path` field and needs no
descriptor path at all.

### 4. `is_process_descriptor_path` → deleted

With no `/proc` paths in the system, this predicate and its siblings
(`is_process_descriptor_directory`, `has_process_descriptor_boundary`) become
dead code and are removed rather than left as no-ops.

### 5. SQLite — the one place a path remains

There is no descriptor-based open. `sqlite3_open_v2` takes a filename only; URI
parameters recognise `vfs`, `cache`, and `mode` and nothing descriptor-shaped;
no built-in VFS accepts a descriptor; `rusqlite 0.40.1` exposes only
`AsRef<Path>` variants and has no VFS registration API at all.

`Store::open_reader` and `validate_image` therefore keep taking a real path
under the pinned, exclusively-created, 0700 cache directory. Three checks that
are **currently disabled because of `/proc`** become unconditional:

- `O_NOFOLLOW` in `open_regular` (:1459-1462), suppressed today only because
  `/proc/self/fd/N` is a symlink that must be followed.
- `SQLITE_OPEN_NOFOLLOW` (`store.rs:200-202`, `:230-231`, `:812-813`).
- `require_no_sidecars` (`store.rs:807`), which today probes
  `/proc/self/fd/5-wal` — a path that can never exist, so the check silently
  enforces nothing.

Post-open identity verification is added: `fstatat` the name before open, and
compare device and inode against the opened handle afterwards.

## Security Analysis

Presented as three separable claims, because their strengths differ and a
single "equivalent security" assertion would hide the one real regression.

**Link publication — strictly stronger.** `linkat` with two directory
descriptors resolves exactly one component from each pinned directory. The
`/proc` form resolves four (`/proc`, `self`, `fd`, `N`) and then the target
name. Atomicity and `EEXIST` are POSIX guarantees, not properties of `/proc`.

**Hashing and metadata validation — strictly stronger.** `read_at` and
`fstat` on the pinned `File` resolve zero path components, against four today.

**SQLite open — one genuine regression, bounded and named.** Today a
`SnapshotEntry` retains a `/proc/self/fd/N` string kept live by a held `File`,
so every later `open_reader` re-resolves to the exact inode validated at
snapshot time, even after a rename or unlink-and-recreate. A real path does not
reproduce that. The residual window is between the pre-open `fstatat` and
SQLite's own independent `open()`, and it is a final-component rename swap.
`SQLITE_OPEN_NOFOLLOW` closes the symlink half; it does not close the rename
half.

Four mitigations bound the consequence:

1. The window is same-user only. The cache lives under `.git/graphr/v6/` in a
   0700 directory reached through a device/inode-verified handle. An attacker
   who can rename entries there can already write the user's `.git`, at which
   point the cache is not the interesting target. `/proc/self/fd` was never
   defending against a same-uid attacker; it was defending against path-prefix
   races, which this design eliminates entirely elsewhere.
2. Graph names are content-addressed. A substituted file either has different
   content and fails the checksum, or is byte-identical and the substitution is
   inert.
3. `validate_entry_graph` (:1517) recomputes the checksum and re-runs
   `validate_published_image` on *every* use, and a failure quarantines rather
   than proceeds. The worst outcome degrades to a forced rebuild, not a poisoned
   graph.
4. Post-open device/inode comparison catches the swap in the overwhelming
   majority of cases; it is a detection layer, not the primary defence.

Net posture improves: one narrow, same-uid, DoS-bounded window is introduced in
exchange for eliminating path resolution from the link and hash paths and
enabling three checks that are presently inert.

Device/inode comparison is used here as an assertion over an already-safe
resolution, never as the mechanism that makes resolution safe — the same
distinction `cap-std` draws, where `is_same_file` is a debug cross-check.

## Acceptance Tests

All tests use real temporary Git repositories and must pass on both Linux and
macOS.

1. `graphr index` completes on macOS against a real repository and publishes a
   queryable snapshot.
2. `search`, `view`, and `changes` return results for that snapshot on macOS.
3. Publishing a graph whose target name already exists returns the
   "already published" branch, not an error and not a replacement.
4. Publishing succeeds on a filesystem that rejects `linkat` with `flags = 0`,
   exercising the `ENOTSUP` retry.
5. The catalog scan enumerates every manifest under a root, and the scanned
   directory's long-lived descriptor remains usable afterwards.
6. Two consecutive catalog scans of one root return identical entries, proving
   the directory position was not shared.
7. Hashing a published graph returns the same digest as the pre-change
   implementation for a fixed fixture, and does not disturb the file offset of a
   concurrently held handle.
8. A published image whose file is replaced with different content fails
   validation and is quarantined.
9. `open_regular` refuses a symlink unconditionally.
10. A database path with a `-wal` or `-journal` sibling is rejected by
    `require_no_sidecars`, proving the check is live rather than vacuous.
11. No `/proc` string, `stable_directory_path`, `stable_file_path`, or
    `is_process_descriptor_*` symbol remains in `src/`.

## Delivery Sequence

One milestone, landing as a patch release. There is no wire-format change, no
schema change, and no cache-layout change, so nothing here is breaking.

1. **`linkat` by descriptor pair.** Self-contained; removes the first `/proc`
   site and is a strict improvement on Linux too.
2. **Descriptor-native hashing and metadata.** `hash_file(&File)` via `read_at`,
   `file.metadata()` for validation, removing `stable_file_path` consumers.
3. **`fdopendir` catalog scan.** Removes the last `stable_directory_path`
   consumer.
4. **Delete the four functions and enable the three suppressed checks.**
   Unconditional `O_NOFOLLOW`, unconditional `SQLITE_OPEN_NOFOLLOW`, live
   `require_no_sidecars`, plus post-open device/inode verification.
5. **Release 0.6.1** and add macOS to the CI matrix as a required job.

Steps 1 through 3 are independent and individually shippable. Step 4 must follow
all three, because it deletes the functions they stop using. Step 5 follows 4.

This milestone precedes the coverage-defects milestone in
`2026-08-12-review-coverage-design.md`, which becomes 0.7.0. Until this lands,
`cargo test` cannot be used as a local gate on macOS, so that plan's
verification steps require a Linux runner.

## Release Verification

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

All four must pass on **both** `ubuntu-latest` and `macos-latest`. The macOS job
added in `.github/workflows/ci.yml` is expected to be red until this milestone
lands and is the completion signal for it.

Additionally: `graphr index` followed by a `changes` query, run by hand on
macOS against a real repository, is the acceptance gate that `cargo test` alone
would not have caught — the defect this document fixes was invisible to `fmt`,
`clippy`, and `build`.
