# Complete Artifact Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make one bounded `changes` snapshot cover every safe changed text file, with independent artifact pagination and Markdown/TSV semantic summaries.

**Architecture:** Preserve the existing Rust/Python capture and graph path. Add an `ArtifactReview` sidecar to `WorktreeChanges`, populate it from a parallel non-source Git diff plus safe untracked reads, and expose its deterministic semantic records and raw patch through a fourth review section.

**Tech Stack:** Rust 2024, Rust standard library, Git CLI, existing `blake3`, `rmcp`, SQLite-backed graph store, MCP stdio.

## Global Constraints

- Build one Rust binary for Codex and Claude over MCP stdio.
- Rust and Python remain the only indexed source languages.
- Add no crate, configuration file, compatibility alias, migration, HTTP surface, UI, or editor integration.
- Do not add more source languages, a dynamic analyzer framework, a deep artifact tool, a pinned external specification root, generic-method resolution, builder/parser invariant comparison, general trust-state tracking, or test-gap confidence changes.
- Keep all output deterministic, line-safe, compact, and at most 8,192 bytes per MCP response.
- Keep trust-boundary path validation, no-follow reads, two-sample rollback safety, Git timeouts, and aggregate output limits.
- Use a 2 MiB per-file limit for untracked text and full-file Markdown/TSV semantic analysis.
- Treat binary, invalid UTF-8, oversized, unsafe, non-regular, type-changed, and unmerged content as incomplete.
- Run production changes through a failing test first.
- Required final checks are `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build --locked --release`.

---

### Task 1: Deterministic Markdown and TSV analyzers

**Files:**
- Create: `src/artifact.rs`
- Modify: `src/main.rs:1-6`

**Interfaces:**
- Consumes: artifact path plus `Option<&str>` old and new UTF-8 content.
- Produces: `AnalyzerKind`, `Analysis`, `analyzer_kind(path)`, and `analyze(path, old, new)` for later capture code.

- [ ] **Step 1: Add the Markdown failing test before the analyzer implementation**

Add `mod artifact;` beside the existing module declarations in `src/main.rs`. Create `src/artifact.rs` with only this test module, so the first run fails because the required API does not exist:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_reports_semantic_changes_and_claimed_digests() {
        let old_digest = "a".repeat(64);
        let new_digest = "b".repeat(64);
        let old = format!(
            "See [REQ-1](specs/old.md#req-1) and `src/old.rs`.\n[old-ref]: docs/old.md\n\n```rust\nsha256:{old_digest}\n```\n"
        );
        let new = format!(
            "See [REQ-2](specs/new.md#req-2) and `src/new.rs`.\n[new-ref]: docs/new.md\n\n```rust\nsha256:{new_digest}\n```\n"
        );

        let analysis = analyze("README.md", Some(&old), Some(&new)).unwrap();
        assert_eq!(analysis.kind, AnalyzerKind::Markdown);
        assert!(analysis.output.contains("change=removed kind=requirement value=\"REQ-1\""));
        assert!(analysis.output.contains("change=added kind=requirement value=\"REQ-2\""));
        assert!(analysis.output.contains("kind=spec-citation target=\"specs/new.md#req-2\""));
        assert!(analysis.output.contains("kind=reference-definition label=\"new-ref\" target=\"docs/new.md\""));
        assert!(analysis.output.contains("kind=path value=\"src/new.rs\""));
        assert!(analysis.output.contains("kind=digest state=claimed"));
        assert!(analysis.output.contains("kind=fence marker=\"```\" info=\"rust\""));
        assert!(!analysis.output.contains('\t'));
    }

    #[test]
    fn markdown_reports_unclosed_fences_without_output_injection() {
        let analysis = analyze(
            "notes.md",
            None,
            Some("REQ-9\n```text\nvalue\twith-tab\n"),
        )
        .unwrap();
        assert!(analysis.output.contains("issue=unclosed-fence"));
        assert!(analysis.output.contains("REQ-9"));
        assert!(!analysis.output.contains('\t'));
    }
}
```

- [ ] **Step 2: Run the Markdown test and verify RED**

Run:

```bash
cargo test artifact::tests::markdown_reports_semantic_changes_and_claimed_digests -- --exact
```

Expected: compilation fails because `analyze` and `AnalyzerKind` are not defined.

- [ ] **Step 3: Implement the public analyzer API and Markdown analyzer**

Start `src/artifact.rs` with this exact public boundary:

```rust
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AnalyzerKind {
    Generic,
    Markdown,
    Tsv,
}

impl AnalyzerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Markdown => "markdown",
            Self::Tsv => "tsv",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Analysis {
    pub kind: AnalyzerKind,
    pub output: String,
}

type Analyzer = fn(&str, Option<&str>, Option<&str>) -> Result<String, String>;

struct AnalyzerEntry {
    extensions: &'static [&'static str],
    kind: AnalyzerKind,
    run: Analyzer,
}

const ANALYZERS: &[AnalyzerEntry] = &[AnalyzerEntry {
    extensions: &["md", "markdown"],
    kind: AnalyzerKind::Markdown,
    run: analyze_markdown,
}];

pub fn analyzer_kind(path: &str) -> AnalyzerKind {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    ANALYZERS
        .iter()
        .find(|entry| {
            entry
                .extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
        .map_or(AnalyzerKind::Generic, |entry| entry.kind)
}

pub fn analyze(
    path: &str,
    old: Option<&str>,
    new: Option<&str>,
) -> Result<Analysis, String> {
    let kind = analyzer_kind(path);
    let output = ANALYZERS
        .iter()
        .find(|entry| entry.kind == kind)
        .map_or_else(|| Ok(String::new()), |entry| (entry.run)(path, old, new))?;
    Ok(Analysis { kind, output })
}
```

Implement the Markdown internals with these private units and rules:

- `markdown_records(text) -> Vec<Record>` scans lines once and tracks fenced blocks.
- A fence opener allows zero through three leading spaces and at least three identical backticks or tildes. A closer uses the same character and at least the opener length.
- A fence record's comparison key is marker, info string, and BLAKE3 body digest; its line range is output metadata only.
- `inline_links`, reference definitions, inline-code spans, uppercase `PREFIX-123` tokens, algorithm-tagged or bare 40/64 hex digests, and local Markdown targets each produce typed records.
- A local `.md` or `.markdown` target with an optional fragment produces both link and spec-citation records. A link target or inline-code span containing a repository-looking slash produces a path record.
- `diff_records` uses `BTreeMap<RecordKey, Vec<Location>>` multisets. Emit only excess old records as `change=removed` and excess new records as `change=added`.
- Quote every path, label, target, value, marker, and info string with `format!("{value:?}")`; never interpolate raw repository text outside quotes.
- Sort output by semantic kind, identity, change kind, and line metadata.
- Emit an unclosed-fence issue record while hashing the body through end of file.

Use one line per record in this field order:

```text
markdown path="README.md" change=added kind=requirement value="REQ-2" line=1
markdown path="README.md" change=added kind=spec-citation target="specs/new.md#req-2" line=1
markdown path="README.md" change=added kind=digest state=claimed value="sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" line=4
markdown path="README.md" change=added kind=fence marker="```" info="rust" digest=af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262 lines=3-5
markdown path="README.md" kind=issue issue=unclosed-fence marker="```" line=3
```

- [ ] **Step 4: Run the Markdown test and verify GREEN**

Run:

```bash
cargo test artifact::tests::markdown_reports_semantic_changes_and_claimed_digests -- --exact
```

Expected: PASS.

- [ ] **Step 5: Add TSV tests and verify they fail before TSV dispatch exists**

Add:

```rust
#[test]
fn tsv_reports_schema_rows_duplicates_and_width_issues() {
    let old = "id\tvalue\na\told\na\tsecond\nb\tkeep\nd\tgone\n";
    let new = "id\tamount\na\tnew\na\tsecond\nc\tadded\nb\ttoo\twide\n";
    let analysis = analyze("fixtures/data.TSV", Some(old), Some(new)).unwrap();

    assert_eq!(analysis.kind, AnalyzerKind::Tsv);
    for expected in [
        "kind=schema old=[\"id\", \"value\"] new=[\"id\", \"amount\"]",
        "kind=key key_basis=first-column old=\"id\" new=\"id\"",
        "kind=duplicate side=old key=\"a\" count=2 identity=ambiguous",
        "kind=duplicate side=new key=\"a\" count=2 identity=ambiguous",
        "change=modified kind=row key=\"a\" occurrence=1 identity=ambiguous columns=[\"amount\"]",
        "change=removed kind=row key=\"d\" occurrence=1",
        "change=added kind=row key=\"c\" occurrence=1",
        "kind=row-width side=new line=5 expected=2 actual=3",
    ] {
        assert!(analysis.output.contains(expected), "missing {expected}: {}", analysis.output);
    }
}

#[test]
fn tsv_reports_duplicate_headers() {
    let analysis = analyze("data.tsv", None, Some("id\tid\na\t1\n")).unwrap();
    assert!(
        analysis
            .output
            .contains("kind=duplicate-header side=new name=\"id\" count=2")
    );
}

#[test]
fn generic_text_has_no_semantic_output() {
    let analysis = analyze("config.toml", Some("old\n"), Some("new\n")).unwrap();
    assert_eq!(analysis.kind, AnalyzerKind::Generic);
    assert!(analysis.output.is_empty());
}
```

Run:

```bash
cargo test artifact::tests::tsv_reports_schema_rows_duplicates_and_width_issues -- --exact
```

Expected: FAIL because `.TSV` still resolves to `AnalyzerKind::Generic`.

- [ ] **Step 6: Implement TSV dispatch and comparison**

Add this entry to `ANALYZERS`:

```rust
AnalyzerEntry {
    extensions: &["tsv"],
    kind: AnalyzerKind::Tsv,
    run: analyze_tsv,
},
```

Implement TSV parsing with these exact rules:

- Split physical rows on `\n`; remove one terminal `\r` from each row.
- Treat the first row as the header and remaining rows as data. Do not parse quotes or types.
- Always emit a schema record and a key record. An empty side uses `[]` and `null` respectively.
- Emit duplicate-header records per side.
- Emit one row-width issue for each row whose field count differs from that side's header width.
- Assign each data row `(first_field, one_based_occurrence)`. Empty rows have an empty first field.
- Match old and new rows by that identity. Emit added, removed, and modified rows; do not emit row moves.
- Compare matched rows by column position. Use a unique non-empty new header name, then a unique old header name, otherwise `column_N`.
- Mark row records `identity=ambiguous` when either side contains that key more than once.
- Use `BTreeMap` and sorted vectors so hash-map iteration can never affect output.

- [ ] **Step 7: Run analyzer tests and commit**

Run:

```bash
cargo fmt
cargo fmt --check
cargo test artifact::tests
```

Expected: PASS.

Commit:

```bash
git add src/main.rs src/artifact.rs
git commit -m "feat: analyze markdown and tsv artifacts"
```

---

### Task 2: Capture tracked artifact diffs and semantic inputs

**Files:**
- Modify: `src/git.rs:14-170,260-390,646-1442`
- Modify: `src/store.rs:3485-4575` (mechanical test-literal defaults only)
- Modify: `src/index.rs:315-378,2253-2637` (mechanical source-patch field rename and test defaults only)

**Interfaces:**
- Consumes: Task 1 `AnalyzerKind`, `analyzer_kind`, and `analyze`.
- Produces: `ArtifactOmission`, `ArtifactFile`, `ArtifactReview`, and `WorktreeChanges.artifacts` for pagination.

- [ ] **Step 1: Write a tracked-artifact integration test in `src/git.rs`**

Add this test beside `change_inventory_reports_unsupported_untracked_files_and_ignores_ignored_files`:

```rust
#[test]
fn tracked_artifacts_are_separate_analyzed_and_classified() {
    let root = temp_root("tracked-artifacts");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn before() {}\n").unwrap();
    fs::write(root.join("README.md"), "See [REQ-1](docs/old.md).\n").unwrap();
    fs::write(root.join("data.tsv"), "id\tvalue\na\told\n").unwrap();
    fs::write(root.join("docs/old.txt"), "plain\n").unwrap();
    fs::write(root.join("docs/deleted.txt"), "deleted\n").unwrap();
    fs::write(root.join("blob.bin"), [0, 1, 2]).unwrap();
    fs::write(root.join("large.md"), vec![b'a'; SOURCE_LIMIT as usize + 1]).unwrap();
    test_git(&root, &["init", "--quiet"]);
    test_git(&root, &["add", "--", "."]);
    test_git(
        &root,
        &[
            "-c",
            "user.name=Graphr Test",
            "-c",
            "user.email=graphr@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ],
    );

    fs::write(root.join("src/lib.rs"), "pub fn after() {}\n").unwrap();
    fs::write(root.join("README.md"), "See [REQ-2](docs/new.md).\n").unwrap();
    fs::write(root.join("data.tsv"), "id\tvalue\na\tnew\n").unwrap();
    fs::rename(root.join("docs/old.txt"), root.join("docs/new.txt")).unwrap();
    fs::remove_file(root.join("docs/deleted.txt")).unwrap();
    fs::write(root.join("docs/added.txt"), "added\n").unwrap();
    test_git(&root, &["add", "--", "docs/added.txt"]);
    fs::write(root.join("blob.bin"), [0, 3, 4]).unwrap();
    fs::write(root.join("large.md"), vec![b'b'; SOURCE_LIMIT as usize + 1]).unwrap();

    let repository = Repository {
        root: fs::canonicalize(&root).unwrap(),
        database: root.join(".git/graphr/index.db"),
    };
    let changes = repository
        .worktree_changes("HEAD", DependencyMode::Boundary, &AtomicBool::new(false))
        .unwrap();

    assert!(changes.source_patch.contains("src/lib.rs"));
    assert!(!changes.source_patch.contains("README.md"));
    assert!(changes.artifacts.patch.contains("README.md"));
    assert!(changes.artifacts.patch.contains("data.tsv"));
    assert!(changes.artifacts.patch.contains("docs/new.txt"));
    assert!(changes.artifacts.patch.contains("docs/deleted.txt"));
    assert!(changes.artifacts.patch.contains("docs/added.txt"));
    assert!(!changes.artifacts.patch.contains("src/lib.rs"));
    assert!(changes.artifacts.analysis.contains("REQ-1"));
    assert!(changes.artifacts.analysis.contains("REQ-2"));
    assert_eq!(
        changes.artifacts.file("blob.bin").unwrap().omission,
        Some(ArtifactOmission::Binary)
    );
    let large = changes.artifacts.file("large.md").unwrap();
    assert!(large.diff_complete);
    assert!(!large.analysis_complete);
    assert_eq!(large.omission, Some(ArtifactOmission::Oversized));
    assert!(changes.paths.iter().any(|path| {
        path.status == ChangeStatus::Renamed
            && path.old_path.as_deref() == Some("docs/old.txt")
            && path.path == "docs/new.txt"
    }));

    fs::remove_dir_all(root).unwrap();
}
```

- [ ] **Step 2: Run the tracked-artifact test and verify RED**

Run:

```bash
cargo test git::tests::tracked_artifacts_are_separate_analyzed_and_classified -- --exact
```

Expected: compilation fails because `WorktreeChanges.artifacts` and artifact coverage types do not exist.

- [ ] **Step 3: Add the artifact sidecar data model**

Import `crate::artifact::{AnalyzerKind, analyze, analyzer_kind}` and add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactOmission {
    Binary,
    InvalidUtf8,
    Oversized,
    NonRegular,
    TypeChanged,
    Unmerged,
}

impl ArtifactOmission {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::InvalidUtf8 => "invalid-utf8",
            Self::Oversized => "oversized",
            Self::NonRegular => "non-regular",
            Self::TypeChanged => "type-changed",
            Self::Unmerged => "unmerged",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ArtifactFile {
    pub path: String,
    pub analyzer: AnalyzerKind,
    pub diff_complete: bool,
    pub analysis_complete: bool,
    pub omission: Option<ArtifactOmission>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct ArtifactReview {
    pub files: Vec<ArtifactFile>,
    pub analysis: String,
    pub patch: String,
}

impl ArtifactReview {
    pub fn file(&self, path: &str) -> Option<&ArtifactFile> {
        self.files
            .binary_search_by_key(&path, |file| file.path.as_str())
            .ok()
            .map(|index| &self.files[index])
    }

    pub fn is_complete(&self) -> bool {
        self.files
            .iter()
            .all(|file| file.diff_complete && file.analysis_complete)
    }

    pub fn analysis_complete(&self) -> bool {
        self.files.iter().all(|file| file.analysis_complete)
    }
}
```

Rename `WorktreeChanges.patch` to `source_patch` and add
`pub artifacts: ArtifactReview`; include both in `WorktreeChanges::is_empty`.
Rename the field and add `artifacts: Default::default()` in every existing
`WorktreeChanges` test literal in `src/git.rs`, `src/index.rs`, and
`src/store.rs`. This is the only change to `src/store.rs` in this task.

- [ ] **Step 4: Retain blob OIDs in parsed Git metadata**

Extend `RawHeader` and `RawChange` with `old_oid: Option<String>` and
`new_oid: Option<String>`. Convert an all-zero object ID to `None`; retain other
validated 40/64-character OIDs. Do not change existing raw path validation.

Add this test before changing `RawHeader`; run it and confirm the missing-field
compile failure, then populate both OID fields in `parse_raw_header` and
`parse_raw_changes`:

```rust
#[test]
fn parse_raw_header_retains_nonzero_oids() {
    let zero = "0".repeat(OID.len());
    let modified = parse_raw_header(
        format!(":100644 100644 {OID} {OID} M").as_bytes(),
    )
    .unwrap();
    assert_eq!(modified.old_oid.as_deref(), Some(OID));
    assert_eq!(modified.new_oid.as_deref(), Some(OID));

    let added = parse_raw_header(
        format!(":000000 100644 {zero} {OID} A").as_bytes(),
    )
    .unwrap();
    assert_eq!(added.old_oid, None);
    assert_eq!(added.new_oid.as_deref(), Some(OID));
}
```

- [ ] **Step 5: Add the parallel tracked-artifact Git worker**

Add this sidecar to `WorktreeCapture`:

```rust
struct TrackedArtifactSnapshot {
    review: ArtifactReview,
    stats: Vec<TrackedStat>,
    renames: Vec<(String, String)>,
    signature: [u8; 32],
}
```

Spawn it inside the existing scoped capture with these arguments, where
`revision` is the validated `format!("{base}^{{commit}}")` already built by
`worktree_changes`:

```rust
let mut artifact_args = vec![
    "diff-index",
    "--raw",
    "-z",
    "--patch",
    "--unified=0",
    "--abbrev=64",
    "--find-renames=50%",
    "-l0",
    "--diff-filter=AMDR",
    "--diff-algorithm=myers",
    "--no-indent-heuristic",
    "-O/dev/null",
    "--no-color",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--no-ext-diff",
    "--no-textconv",
    "--ignore-submodules=all",
    revision.as_str(),
    "--",
    ".",
    ":(exclude)*.rs",
    ":(exclude)*.py",
];
if dependency_mode == DependencyMode::Boundary {
    artifact_args.push(":(glob,exclude).cargo/vendor/*/**");
}
```

Do not add `--text`.

- [ ] **Step 6: Parse artifact sections and run semantic analysis**

Implement `capture_tracked_artifacts` by reusing `parse_raw_changes` and
`parse_patch_hunks`:

- A section containing `Binary files ` or `GIT binary patch` is binary; retain
  its line-safe metadata in `ArtifactReview.patch`, mark `diff_complete=false`,
  and do not analyze it.
- A non-binary section must be valid UTF-8 before it enters
  `ArtifactReview.patch`; otherwise mark it `InvalidUtf8` and omit its bytes.
- Use the retained `old_oid` with
  `run(&self.root, &["cat-file", "blob", old_oid], cancelled)` for old
  Markdown/TSV content. Pass old and current bytes through one `semantic_text`
  helper that rejects content over 2 MiB as `Oversized`, NUL as `Binary`, and
  other invalid UTF-8 as `InvalidUtf8`.
- Use `read_regular_file` for current Markdown/TSV content. Distinguish missing,
  oversized, NUL-containing, and invalid UTF-8 bytes.
- Generic text does not require full-file semantic reads.
- Invoke `analyze` only when every required semantic side is present. Append its
  output and set `analysis_complete=true`.
- When a tracked Markdown/TSV file exceeds the semantic limit but its Git patch
  is valid text, keep `diff_complete=true`, set `analysis_complete=false`, and
  attach `ArtifactOmission::Oversized`.
- Hash raw artifact Git bytes plus every semantic input and analyzer output into
  `TrackedArtifactSnapshot.signature`.
- Skip patch sections whose raw endpoint failed safe path parsing; only the
  aggregate skipped-path count may represent them.
- Sort artifact files by path and analysis records by their already deterministic
  analyzer output.

- [ ] **Step 7: Merge artifact stats and renames into the stable sample**

Generalize rename coalescing so source records and artifact `(old, new)` pairs
can each coalesce the inventory's conservative add/delete records. Keep artifact
renames out of graph-facing `PathRecord`. Rename `SupportedProjection` and
`validate_supported_projection` to `SourceProjection` and
`validate_source_projection`; rename `coalesce_supported_renames` to the shared
`coalesce_renames` helper.

Generalize tracked stat application so exact artifact text hunks populate
`ChangedPath.additions` and `deletions`; binary and omitted content retains
unknown stats. Rename `apply_supported_stats` to `apply_captured_stats` and pass
the exact source/artifact path set it may update.

Add tracked artifact bytes and the artifact signature to `worktree_signature`.
Merge only the second stable sample into `WorktreeChanges.artifacts`.

- [ ] **Step 8: Run focused and regression tests**

Run:

```bash
cargo fmt
cargo fmt --check
cargo test git::tests::tracked_artifacts_are_separate_analyzed_and_classified -- --exact
cargo test git::tests
cargo test store::tests
```

Expected: PASS.

- [ ] **Step 9: Commit tracked capture**

```bash
git add src/git.rs src/index.rs src/store.rs
git commit -m "feat: capture tracked artifacts"
```

---

### Task 3: Capture untracked artifacts without fallback caps

**Files:**
- Modify: `src/git.rs:14-19,155-160,461-625,1354-1442,2452-2575`

**Interfaces:**
- Consumes: Task 2 `ArtifactReview`, `ArtifactFile`, and `ArtifactOmission`.
- Produces: source and artifact patches from one stable untracked scan.

- [ ] **Step 1: Change the existing untracked inventory test to require native artifact coverage**

Rename `change_inventory_reports_unsupported_untracked_files_and_ignores_ignored_files`
to `change_inventory_captures_untracked_artifacts_and_ignores_ignored_files`,
then replace its final patch assertions with:

```rust
assert!(changes.source_patch.contains("+pub fn added() {}"));
assert!(!changes.source_patch.contains("tracked.tsv"));
assert!(changes.artifacts.patch.contains("alias-registry.v1.tsv"));
assert!(changes.artifacts.patch.contains("tracked.tsv"));
assert!(changes.artifacts.analysis.contains("key_basis=first-column"));
assert_eq!(
    changes.artifacts.file("tests/fixtures/blob.bin").unwrap().omission,
    Some(ArtifactOmission::Binary)
);
assert_eq!(
    changes
        .artifacts
        .file("tests/fixtures/large.data")
        .unwrap()
        .omission,
    Some(ArtifactOmission::Oversized)
);
assert_eq!(
    changes.artifacts.file("tests/fixtures/link.rs").unwrap().omission,
    Some(ArtifactOmission::NonRegular)
);
assert_eq!(
    changes
        .artifacts
        .file("tests/fixtures/invalid.txt")
        .unwrap()
        .omission,
    Some(ArtifactOmission::InvalidUtf8)
);
```

Update the expected `ChangedPath` stats so both TSV paths have exact additions
and deletions. Add
`fs::write(root.join("tests/fixtures/invalid.txt"), [0xff]).unwrap();` before
capturing changes. Keep the ignored and unsafe-path assertions.

- [ ] **Step 2: Run the untracked test and verify RED**

Run:

```bash
cargo test git::tests::change_inventory_captures_untracked_artifacts_and_ignores_ignored_files -- --exact
```

Expected: FAIL because non-source untracked patches and analyzer output are absent.

- [ ] **Step 3: Remove aggregate sampling caps and split the untracked result**

Replace the aggregate sampling constants with the existing per-file
`SOURCE_LIMIT`; remove `UNTRACKED_STATS_FILE_LIMIT` and
`UNTRACKED_STATS_BYTE_LIMIT`. Every safe untracked path is attempted until the
shared Git deadline or aggregate output limit is reached.

Change `UntrackedSnapshot` to carry source and artifact output separately:

```rust
struct UntrackedSnapshot {
    paths: Vec<ChangedPath>,
    source_patch: Vec<u8>,
    artifacts: ArtifactReview,
    skipped_paths: usize,
    signature: [u8; 32],
}
```

Update empty-return construction and destructuring sites immediately, using an
empty `ArtifactReview` and an empty `source_patch`, so the crate compiles before
the classification loop changes.

- [ ] **Step 4: Classify and diff each untracked path**

For each untracked path:

- Keep the before/read/after no-follow metadata checks.
- Files over 2 MiB become `ArtifactOmission::Oversized` when non-source.
- NUL-containing files become `Binary`; other invalid UTF-8 becomes
  `InvalidUtf8`.
- Safe UTF-8 Rust/Python files retain the existing source patch behavior.
- Safe UTF-8 non-source files use the existing isolated no-index patch command,
  append to `artifacts.patch`, and receive exact stats.
- Markdown/TSV content runs through `analyze`; generic text has empty analysis.
- Symlinks and other non-regular paths become artifact `NonRegular` omissions,
  including paths whose extension would identify source if they were regular.
- Unsafe path bytes only increment `skipped_paths`; never render the name.
- Hash classification, file metadata, read bytes, source patch, artifact patch,
  and analyzer output into the untracked signature.

- [ ] **Step 5: Merge untracked artifacts and finalize non-content omissions**

In `merge_changes`, append `source_patch` to `WorktreeChanges.source_patch` and merge
untracked artifact files, analysis, and patch into the tracked `ArtifactReview`.
Reject a combined source-plus-artifact payload over `STDOUT_LIMIT`. Sort and
deduplicate artifact coverage by path; disagreement is a retry error rather
than last-write-wins behavior.

After merging inventory and captured patches, add omitted artifact records for
safe non-source `TypeChanged` and `Unmerged` paths not represented by a patch.
Drive the helper with this test:

```rust
#[test]
fn finalizes_non_content_artifact_omissions() {
    let paths = vec![
        ChangedPath {
            status: ChangeStatus::TypeChanged,
            old_path: None,
            old_language: None,
            path: "typed.txt".into(),
            language: None,
            additions: None,
            deletions: None,
        },
        ChangedPath {
            status: ChangeStatus::Unmerged,
            old_path: None,
            old_language: None,
            path: "conflict.txt".into(),
            language: None,
            additions: None,
            deletions: None,
        },
    ];
    let mut review = ArtifactReview::default();
    finalize_artifact_omissions(&paths, &mut review, DependencyMode::Boundary);
    assert_eq!(
        review.file("typed.txt").unwrap().omission,
        Some(ArtifactOmission::TypeChanged)
    );
    assert_eq!(
        review.file("conflict.txt").unwrap().omission,
        Some(ArtifactOmission::Unmerged)
    );
}
```

The helper skips dependency-boundary paths and paths already present in the
sorted review.

- [ ] **Step 6: Run focused tests and commit**

Run:

```bash
cargo fmt
cargo fmt --check
cargo test git::tests::change_inventory_captures_untracked_artifacts_and_ignores_ignored_files -- --exact
cargo test git::tests
```

Expected: PASS.

Commit:

```bash
git add src/git.rs
git commit -m "feat: capture untracked artifacts"
```

---

### Task 4: Add artifact manifest classes, pagination, and completion

**Files:**
- Modify: `src/index.rs:22-27,135-858,2180-2677`

**Interfaces:**
- Consumes: Tasks 2-3 `WorktreeChanges.artifacts` and per-path coverage.
- Produces: `ReviewSection::Artifacts`, `artifacts_next_cursor`, artifact page metadata, and artifact-aware completion.

- [ ] **Step 1: Write failing artifact-page tests**

Add a helper in `src/index.rs` tests:

```rust
fn complete_artifact(path: &str, analyzer: AnalyzerKind) -> ArtifactFile {
    ArtifactFile {
        path: path.into(),
        analyzer,
        diff_complete: true,
        analysis_complete: true,
        omission: None,
    }
}
```

Add:

```rust
#[test]
fn artifact_pages_are_independent_reconstructable_and_complete() {
    let artifact_patch = format!(
        "diff --git a/README.md b/README.md\n@@ -1 +1 @@\n-old\n+{}\n",
        "é".repeat(5_000)
    );
    let changes = WorktreeChanges {
        files: vec![],
        records: vec![],
        paths: vec![ChangedPath {
            status: ChangeStatus::Modified,
            old_path: None,
            old_language: None,
            path: "README.md".into(),
            language: None,
            additions: Some(1),
            deletions: Some(1),
        }],
        source_patch: String::new(),
        artifacts: ArtifactReview {
            files: vec![complete_artifact("README.md", AnalyzerKind::Markdown)],
            analysis: "markdown path=\"README.md\" change=added kind=requirement value=\"REQ-2\" line=1\n".into(),
            patch: artifact_patch.clone(),
        },
        skipped_paths: 0,
    };
    let graph = "risk overall=0.0000 changed_symbols_total=0 changed_symbols_analyzed=0 changed_symbols_emitted=0 changed_symbols_omitted=0 flows_total=0 test_gaps=0 analysis_complete=true analysis_roots_omitted=0 deleted_paths_unanalyzed=0 neighborhood_omitted=false unmapped_ranges=0\n";
    let snapshot = ReviewSnapshot::new(
        "HEAD",
        6,
        50,
        DependencyMode::Boundary,
        changes,
        graph.into(),
    );

    let initial = review_context(&snapshot).unwrap();
    assert!(initial.contains("\nartifacts\n"), "{initial}");
    assert!(initial.contains("artifacts_next_cursor="), "{initial}");
    assert!(initial.contains("review_complete_when_pages_exhausted=true"));
    let mut stale = next_cursor(&initial, "artifacts_next_cursor").unwrap();
    let replacement = if stale.ends_with('0') { "1" } else { "0" };
    stale.replace_range(stale.len() - 1.., replacement);
    assert_eq!(
        render_section(&snapshot, &parse_review_cursor(&stale).unwrap()).unwrap_err(),
        "stale changes cursor"
    );

    let expected = format!(
        "artifact path=\"README.md\" analyzer=markdown diff_complete=true analysis_complete=true\n{}{}",
        snapshot.changes.artifacts.analysis,
        artifact_patch
    );
    let mut reconstructed = String::new();
    let mut offset = 0;
    loop {
        let (page, more) = render_section_page(
            &snapshot,
            ReviewSection::Artifacts,
            offset,
            SECTION_OVERHEAD + 257,
        )
        .unwrap();
        let metadata = page.lines().nth(1).unwrap();
        let emitted = metadata
            .split_ascii_whitespace()
            .find_map(|field| field.strip_prefix("emitted_bytes="))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let content_start = page.match_indices('\n').nth(1).unwrap().0 + 1;
        reconstructed.push_str(&page[content_start..content_start + emitted]);
        offset += emitted;
        if !more {
            break;
        }
    }
    assert_eq!(reconstructed, expected);
}

#[test]
fn omitted_artifact_keeps_review_incomplete() {
    let mut file = complete_artifact("image.bin", AnalyzerKind::Generic);
    file.diff_complete = false;
    file.analysis_complete = false;
    file.omission = Some(ArtifactOmission::Binary);
    let changes = WorktreeChanges {
        files: vec![],
        records: vec![],
        paths: vec![ChangedPath {
            status: ChangeStatus::Modified,
            old_path: None,
            old_language: None,
            path: "image.bin".into(),
            language: None,
            additions: None,
            deletions: None,
        }],
        source_patch: String::new(),
        artifacts: ArtifactReview {
            files: vec![file],
            analysis: String::new(),
            patch: String::new(),
        },
        skipped_paths: 0,
    };
    assert!(!change_content_complete(
        &changes,
        DependencyMode::Boundary
    ));
}
```

- [ ] **Step 2: Run the artifact-page test and verify RED**

Run:

```bash
cargo test index::tests::artifact_pages_are_independent_reconstructable_and_complete -- --exact
```

Expected: compilation fails because `ReviewSection::Artifacts` does not exist.

- [ ] **Step 3: Add the fourth snapshot section**

Set the initial budgets exactly:

```rust
const INITIAL_FILES_BUDGET: usize = 1792;
const INITIAL_DIFF_BUDGET: usize = 2432;
const INITIAL_ARTIFACTS_BUDGET: usize = 1920;
const INITIAL_GRAPH_BUDGET: usize = 1920;
```

Add `ReviewSection::Artifacts` with code `a`, header `artifacts`, and cursor label
`artifacts_next_cursor`. Accept `a` in `parse_review_cursor`.

Build one artifact value in `ReviewSnapshot::new`:

```rust
fn artifact_text(review: &ArtifactReview) -> String {
    let mut output = String::new();
    for file in &review.files {
        output.push_str(&format!(
            "artifact path={:?} analyzer={} diff_complete={} analysis_complete={}",
            file.path,
            file.analyzer.as_str(),
            file.diff_complete,
            file.analysis_complete,
        ));
        if let Some(reason) = file.omission {
            output.push_str(" reason=");
            output.push_str(reason.as_str());
        }
        output.push('\n');
    }
    output.push_str(&review.analysis);
    output.push_str(&review.patch);
    output
}
```

Store this string on `ReviewSnapshot`; add it to `review_snapshot` hashing and
`ReviewSnapshot::value`. Compute ranges for:

- Artifact file lines beginning `artifact `.
- Semantic lines beginning `markdown ` or `tsv `.
- Artifact raw hunks using `hunk_ranges` over the combined artifact string.

Render the initial sections in files, diff, artifacts, graph order. Include
`artifacts_more` in `review_complete`.

- [ ] **Step 4: Render artifact page accounting**

Add artifact page metadata with byte accounting plus these five-field groups:

```text
emitted_files partial_files total_files prior_files remaining_files
emitted_records partial_records total_records prior_records remaining_records
emitted_hunks partial_hunks total_hunks prior_hunks remaining_hunks
```

Also emit `analysis_complete=<bool>` from
`ArtifactReview::analysis_complete()` and the existing
`page_complete=<bool>`. Use `ArtifactReview::is_complete()` only for final
content completeness.

- [ ] **Step 5: Replace support terminology in the manifest**

Change manifest records to these shapes:

```text
changed source rust src/lib.rs status=modified additions=2 deletions=1
changed source python pkg/app.py status=modified additions=1 deletions=1
changed artifact text README.md analyzer=markdown additions=1 deletions=1
changed artifact text LARGE.md analyzer=markdown analysis=omitted reason=oversized additions=1 deletions=1
untracked artifact text fixtures/data.tsv analyzer=tsv additions=3 deletions=0
changed artifact omitted image.bin analyzer=generic reason=binary additions=unknown deletions=unknown
skipped 2 unsafe paths
```

Rename dependency summary field `supported_sources` to `source_files`, source
diff scope from `supported-source` to `source`, and rename-detection metadata to
`within-source-and-artifact`. Remove every compatibility emission of
`supported`, `unsupported`, and `supported-to-unsupported`.
Rename obsolete internal test and helper identifiers that use those terms when
they refer to source/artifact classification; retain unrelated Git parser errors
such as `unsupported diff metadata`.

- [ ] **Step 6: Wire source, artifact, and all-path accounting into completion**

Update patch accounting so source stats describe `changes.source_patch`, artifact stats
describe `changes.artifacts.patch`, and all-path totals count both once.

Replace `change_content_complete` with logic that:

- Accepts boundary-mode dependency paths.
- Requires existing exact source conditions for Rust/Python paths.
- Requires a matching complete `ArtifactFile` for every other safely renderable
  path.
- Rejects `skipped_paths > 0` and every artifact omission.

- [ ] **Step 7: Update unit expectations and run pagination tests**

Update `change_manifest_preserves_every_path_and_status`, source-diff metadata
assertions, cursor loops, and all obsolete support labels in `src/index.rs`
tests. Include `artifacts_next_cursor` in every loop that exhausts all snapshot
sections.

Run:

```bash
cargo fmt
cargo fmt --check
cargo test index::tests::artifact_pages_are_independent_reconstructable_and_complete -- --exact
cargo test index::tests::omitted_artifact_keeps_review_incomplete -- --exact
cargo test index::tests
```

Expected: PASS and every rendered response remains at most 8,192 bytes.

- [ ] **Step 8: Commit pagination and completion**

```bash
git add src/index.rs
git commit -m "feat: page artifact review coverage"
```

---

### Task 5: Prove the MCP workflow and update guidance

**Files:**
- Modify: `tests/e2e.rs:827-1038,1097-1178`
- Modify: `src/mcp.rs:192-230`
- Modify: `README.md:1-45`
- Modify: `.agents/skills/graphr-review/SKILL.md:1-35`

**Interfaces:**
- Consumes: all prior tasks.
- Produces: user-visible MCP guidance and a mixed-file end-to-end regression test.

- [ ] **Step 1: Extend the existing bounded-pagination E2E test and verify RED**

In `changes_pages_complete_inventory_diff_and_flows`:

- Commit a baseline `README.md` containing `REQ-1`, a local spec link, and one
  fenced Rust block.
- Modify it to `REQ-2`, a new spec link, and a changed fence body.
- Make the untracked TSV contain `id\tvalue` plus enough changed rows to exceed
  the initial artifact budget; make its final field
  `LAST_ARTIFACT_SENTINEL`.
- Commit `settings.toml` with `old=true`, then change it to `old=false`.
- Add an untracked NUL-containing `image.bin` so final coverage remains
  explicitly incomplete.

Replace the existing discarded initialize response with this assertion, then add
the initial `changes` assertions:

```rust
let initialized = client.request(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"graphr-test","version":"0"}}}"#,
);
assert!(initialized.contains("artifacts_next_cursor"), "{initialized}");

for expected in [
    "changed source rust src/lib.rs",
    "changed artifact text README.md analyzer=markdown",
    "untracked artifact text tests/fixtures/alias-registry.v1.tsv analyzer=tsv",
    "untracked artifact omitted image.bin analyzer=generic reason=binary",
    "markdown path=\"README.md\"",
    "artifacts_next_cursor=",
    "review_complete_when_pages_exhausted=false",
] {
    assert!(initial.contains(expected), "missing {expected}: {initial}");
}
```

Add artifact accounting and exhaustion using the existing helpers:

```rust
let artifact_totals = assert_page_accounting(
    &initial,
    "artifacts",
    [
        "emitted_records",
        "partial_records",
        "total_records",
        "prior_records",
        "remaining_records",
    ],
    "artifacts_next_cursor",
);
let mut artifact_pages = initial.clone();
let mut cursor = page_cursor(&initial, "artifacts_next_cursor");
while let Some(token) = cursor {
    let page = changes_page(&mut client, next_id, &token);
    next_id += 1;
    assert!(page.starts_with("artifacts\n"), "{page}");
    assert_eq!(
        assert_page_accounting(
            &page,
            "artifacts",
            [
                "emitted_records",
                "partial_records",
                "total_records",
                "prior_records",
                "remaining_records",
            ],
            "artifacts_next_cursor",
        ),
        artifact_totals
    );
    cursor = page_cursor(&page, "artifacts_next_cursor");
    artifact_pages.push_str(&page);
}
assert!(artifact_pages.contains("key_basis=first-column"));
assert!(artifact_pages.contains("LAST_ARTIFACT_SENTINEL"));
assert!(artifact_pages.contains("diff --git a/README.md b/README.md"));
```

Run:

```bash
cargo test --test e2e changes_pages_complete_inventory_diff_and_flows -- --exact
```

Expected: FAIL because the initialize instructions do not yet name
`artifacts_next_cursor`. The mixed artifact output assertions may already pass
from Tasks 1-4.

- [ ] **Step 2: Update MCP descriptions and review instructions**

Change the `changes` tool description and server instructions to require one
cursorless call followed by exact exhaustion of files, diff, artifacts, and
graph cursors. State:

- `max_nodes` limits graph records per page, not snapshot coverage.
- `index` is called only after Rust/Python source edits.
- Artifact text is part of the immutable snapshot.
- Binary, oversized, unsafe, and other explicit omissions keep coverage
  incomplete.
- The capability is complete artifact coverage, not additional source-language
  support.

Remove the claim that unsupported Markdown/TSV needs an external fallback.

- [ ] **Step 3: Update README and bundled review skill**

In `README.md`, update the four-tool overview and review workflow to include
bounded non-source text diffs, Markdown/TSV semantics, and
`artifacts_next_cursor`. Preserve Rust/Python as the only indexed languages.

In `.agents/skills/graphr-review/SKILL.md`:

- Step 2 says `index` only after Rust/Python edits made in the session.
- Step 3 exhausts `files_next_cursor`, `diff_next_cursor`,
  `artifacts_next_cursor`, and `graph_next_cursor` verbatim.
- Steps 5-6 no longer use ordinary diff/spec fallback for captured Markdown,
  TSV, or generic text.
- Any explicit artifact omission is reported as incomplete; the skill never
  reads an unsafe path or treats binary bytes as source.
- Keep the existing targeted `search`/`view` remediation rule for graph coverage.

- [ ] **Step 4: Run the E2E test and verify GREEN**

Run:

```bash
cargo test --test e2e changes_pages_complete_inventory_diff_and_flows -- --exact
```

Expected: PASS with every artifact continuation page at most 8,192 bytes and the
binary omission keeping final completeness false.

- [ ] **Step 5: Run all required verification**

Run each command separately and require a zero exit status:

```bash
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

Also run:

```bash
git diff --check
git status --short
```

Expected: all checks pass; status lists only the intended implementation and
documentation changes before commit.

- [ ] **Step 6: Commit the completed vertical slice**

```bash
git add src/mcp.rs README.md .agents/skills/graphr-review/SKILL.md tests/e2e.rs
git commit -m "feat: complete artifact review workflow"
```

Run `git status --short` once more. Expected: no output.
