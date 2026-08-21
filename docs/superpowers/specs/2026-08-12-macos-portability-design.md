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
- Do hook SQLite's `xSetSystemCall` / `aSyscall["open"]` table. This reverses
  an earlier decision in this document, on review. It was rejected as
  process-global mutable state installed to avoid a problem solved by other
  means; that was written before it was clear three existing tests depend on
  the descriptor-pinning invariant, and that no other mechanism preserves it.
  Scope and soundness are recorded in section 5.
- Do not use `AT_SYMLINK_NOFOLLOW_ANY`, `AT_RESOLVE_BENEATH`, or `AT_UNIQUE`.
  macOS-only and absent from `libc 0.2.189`.
- No change to the on-disk cache layout, the `graphr/v6` namespace, manifest
  format, or `snapshot_id` derivation. This is a portability fix, not a format
  change.

## Platform Scope

This design targets **POSIX**: Linux, macOS, and the BSDs. Every replacement
primitive is POSIX, and all 21 `libc::` symbols in `src/` already are.

**Windows is out of scope and is not enabled by this work**, but this work is a
precondition for it. The design's core move — expressing every operation as
*(directory handle + single component)* rather than a composite path — is
exactly the shape Windows requires. Today the cache layer is not expressible on
Windows at all; afterwards there is a seam a second backend could plug into.

To keep that option cheap, the descriptor operations must sit behind a narrow
internal boundary — `open_child`, `create_child`, `link_at`, `rename_at`,
`unlink_at`, `read_dir_at`, `stat_at` — rather than being scattered through
`workspace.rs`. That costs nothing now and makes a future Windows backend a
contained `cfg` split instead of an excavation. It is a requirement of this
milestone, not a nicety.

What Windows would additionally require, recorded so the question is not
re-litigated:

- A `windows-sys` dependency. `libc` supplies nothing on Windows, and
  `AGENTS.md` pushes back on new dependencies.
- `ntdll` for two of six primitives. Handle-relative open needs `NtCreateFile`
  with `OBJECT_ATTRIBUTES.RootDirectory`; handle-relative hard link needs
  `NtSetInformationFile` with `FILE_LINK_INFORMATION.RootDirectory`. There is no
  Win32 route to either — `CreateHardLinkW` is path-only, has no fail-if-exists
  flag, and does not work on ReFS. The NT form does map cleanly:
  `ReplaceIfExists = FALSE` yields `STATUS_OBJECT_NAME_COLLISION`, matching the
  `EEXIST` branch `publish_no_replace` depends on.
- Rename, directory enumeration, and file identity have clean Win32 equivalents
  (`FILE_RENAME_INFO` with `RootDirectory`, `GetFileInformationByHandleEx` with
  `FileIdBothDirectoryInfo`, and `FILE_ID_INFO`). Note that 64-bit file IDs are
  not unique on ReFS and identity is unreliable over SMB.
- `FILE_FLAG_OPEN_REPARSE_POINT` *opens* a symlink rather than failing, so the
  `O_NOFOLLOW` equivalent requires an explicit attribute check, and it protects
  only the final component.
- `std::os::windows::fs::FileExt::seek_read` is **not** equivalent to
  `read_at`: it moves the file cursor, including on short reads, so the hashing
  change in this design is not portable as written.
- A rewrite of `src/git.rs`'s byte-oriented `OsStrExt` handling, which parses
  git's NUL-separated `-z` output as bytes. Windows `OsStr` is WTF-16 with no
  `as_bytes()`. This is a larger change than the cache layer itself, and the
  `MetadataExt` device/inode worktree signature has no direct analogue.
- No off-the-shelf help: `cap-std` supports Windows but reconstructs composite
  paths via `GetFinalPathNameByHandleW` for link, rename, and directory reads —
  the exact technique this design removes.

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

### 5. SQLite — pinned through the VFS syscall table

There is no descriptor-based open. `sqlite3_open_v2` takes a filename only; URI
parameters recognise `vfs`, `cache`, and `mode` and nothing descriptor-shaped;
no built-in VFS accepts a descriptor; `rusqlite 0.40.1` exposes only
`AsRef<Path>` variants and has no VFS registration API at all.

What the unix VFS does expose is a replaceable syscall table. `src/pinned.rs`
captures the table's `open` once and installs an override. `pin(path, file)`
then scopes a diversion to the calling thread and one exact path: while the
guard is live, a read-only `open` of that path returns `F_DUPFD_CLOEXEC` of the
validated descriptor and resolves nothing. Every other path, thread, access
mode, and syscall reaches the captured original unchanged.

Three properties make the duplicate sound:

- SQLite reads with `pread`. `sqlite3.c` defines `HAVE_PREAD` / `HAVE_PWRITE`
  for `__APPLE__` and `__linux__`, so `seekAndRead` never consults the file
  offset a duplicate shares with its original.
- The diversion is restricted to read-only opens, and rejects
  `O_CREAT`/`O_TRUNC`/`O_EXCL`, so the pinned read-only descriptor always
  satisfies the access mode SQLite asked for. A writer keeps normal resolution
  and meets the image's 0444 mode.
- The duplicate clears `SQLITE_MINIMUM_FILE_DESCRIPTOR`, which SQLite requires
  of any database descriptor.

`Store::open_reader` and `validate_pinned_image` are therefore reached through
a pin rather than through a bare path. Three checks that are **currently
disabled because of `/proc`** become unconditional:

- `O_NOFOLLOW` in `open_regular` (:1459-1462), suppressed today only because
  `/proc/self/fd/N` is a symlink that must be followed.
- `SQLITE_OPEN_NOFOLLOW` (`store.rs:200-202`, `:230-231`, `:812-813`).
- `require_no_sidecars` (`store.rs:807`), which today probes
  `/proc/self/fd/5-wal` — a path that can never exist, so the check silently
  enforces nothing.

## Security Analysis

Presented as three separable claims, because their strengths differ and a
single "equivalent security" assertion would hide where each one comes from.

**Link publication — strictly stronger.** `linkat` with two directory
descriptors resolves exactly one component from each pinned directory. The
`/proc` form resolves four (`/proc`, `self`, `fd`, `N`) and then the target
name. Atomicity and `EEXIST` are POSIX guarantees, not properties of `/proc`.

**Hashing and metadata validation — strictly stronger.** `read_at` and
`fstat` on the pinned `File` resolve zero path components, against four today.
Seed copies read the entry's descriptor directly (`copy_descriptor`), and
catalog enumeration runs on `fdopendir` of an `openat(".")` rather than on a
rebuilt directory path.

**SQLite open — preserved, by a different mechanism.** Today a `SnapshotEntry`
retains a `/proc/self/fd/N` string kept live by a held `File`, so every later
`open_reader` re-resolves to the exact inode validated at snapshot time, even
after a rename or unlink-and-recreate. The syscall-table pin reproduces that
property without `/proc`: the diverted open returns a duplicate of the
validated descriptor, so a final-component rename between validation and open
cannot redirect the read. This is the invariant
`seed_copy_uses_validated_graph_after_candidate_replacement`,
`snapshot_queries_keep_the_validated_graph_after_filename_replacement`, and
`exact_reuse_does_not_publish_a_replaced_graph_name` assert, and they are
unchanged by this milestone.

The cost is a process-global override, bounded four ways:

1. One syscall (`open`) on one VFS, installed once, forwarding everything it
   does not divert.
2. A diversion requires a live pin, and a pin covers one exact path on one
   thread. Concurrent readers on other threads are unaffected, which
   `a_pin_is_confined_to_the_thread_that_took_it` and
   `concurrent_pinned_readers_each_see_the_pinned_image` assert.
3. Read-only opens only; a writable open is never diverted
   (`a_pin_never_diverts_a_writable_open`).
4. Nesting restores the enclosing pin rather than clearing it
   (`nested_pins_restore_the_enclosing_pin`).

Net posture improves on every axis: path resolution leaves the link, hash, and
enumeration paths, three presently-inert checks become real, and the SQLite
invariant that `/proc` provided is retained rather than traded away.

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
