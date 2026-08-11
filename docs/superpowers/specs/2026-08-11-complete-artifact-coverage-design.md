# Complete Artifact Coverage Design

**Status:** Approved

**Date:** 2026-08-11

## Goal

Make one `changes` snapshot provide bounded review coverage for every safe
changed text file. Rust and Python keep their existing source diff and graph
analysis. Markdown and TSV gain compact semantic summaries in addition to exact
raw diffs. Every other UTF-8 text format gains exact bounded diff coverage.

This is complete artifact coverage, not generic multi-language support.

## Current behavior

`changes` captures every safe changed path in its manifest, but its diff stream
contains only Rust and Python. A reviewer must exhaust the files, diff, and graph
cursors and then leave Graphr to inspect Markdown, TSV, and other text through a
bounded shell fallback. The fallback is outside the immutable review snapshot,
so `review_complete_when_pages_exhausted` remains false for those paths.

The existing Rust/Python capture, graph mapping, dependency boundary, stable
two-sample worktree read, and single retained review snapshot remain the basis
of the design.

## Scope

- Add an independently paged artifact section to `changes`.
- Capture exact zero-context diffs for safe non-source UTF-8 files.
- Add deterministic Markdown and TSV semantic analysis alongside raw diffs.
- Preserve explicit omission reasons and conservative completion flags.
- Update MCP guidance, the README, and the bundled review skill to exhaust the
  artifact cursor.
- Keep the implementation dependency-free and use a static analyzer dispatch
  table rather than traits or dynamic registration.

## Snapshot and cursor architecture

One cursorless `changes` call builds one immutable snapshot containing four
sections in this order:

```text
files
diff
artifacts
graph
```

The `diff` section remains Rust/Python source-only. The new `artifacts` section
contains semantic records followed by exact raw diffs for non-source text. Its
continuation token is named `artifacts_next_cursor`; its internal section code
is `a`. Files, source diff, artifacts, and graph all participate in the snapshot
checksum. A cursor remains valid only with the original base, depth, max-nodes,
and dependency-mode arguments. A new cursorless call replaces the retained
snapshot.

The initial response remains capped at 8 KiB. Its fixed section budgets are:

| Section | Bytes |
| --- | ---: |
| Files | 1,792 |
| Source diff | 2,432 |
| Artifacts | 1,920 |
| Graph | 1,920 |

The budgets total 8,064 bytes and leave 128 bytes for final completion metadata.
Each continuation response can use the existing full 8 KiB response budget
minus its completion line and section framing.

`review_complete` is true only when the initial page exhausts all four sections
and the immutable snapshot is complete. `review_complete_when_pages_exhausted`
describes whether exhausting every emitted cursor will make the snapshot
complete.

`index` remains necessary only after Rust or Python source edits. Artifact-only
changes do not alter the code graph and do not require indexing.

## Artifact capture

`Repository::worktree_changes` retains its two-sample stability check and keeps
the existing Rust/Python Git invocation unchanged. Each sample also captures a
non-source artifact stream with these properties:

- Zero lines of diff context.
- No external diff command or text conversion.
- Git's normal binary classification; binary content is never forced through a
  text diff.
- Rust and Python pathspecs excluded.
- `.cargo/vendor` internals excluded in boundary mode and retained in full mode.
- The existing Git timeout and aggregate output limit retained.

The artifact capture runs alongside the existing tracked source, inventory, and
untracked capture. Its bytes and semantic-input hashes join the stable-sample
signature. Any mismatch rejects the complete read with the existing retry
error; Graphr never combines source and artifact data from different worktree
states.

Safe untracked artifacts reuse the existing no-follow regular-file reader. A
file is eligible for text diffing only when it is at most 2 MiB, contains no NUL
byte, and is valid UTF-8. The tracked diff metadata retains each base blob OID;
`git cat-file blob` supplies immutable old content for semantic analysis, while
the safe reader supplies current and untracked content.

The in-memory change result separates:

- The existing graph-facing changed source files and path records.
- `source_patch` for Rust/Python.
- `artifact_patch` for non-source text.
- `artifact_analysis` for Markdown/TSV semantic records.
- A per-path coverage class and optional omission reason.

Conceptually, coverage classes are `source rust`, `source python`, `artifact
text analyzer=markdown|tsv|generic`, and `artifact omitted reason=...`. These
classes replace the current `supported`/`unsupported` output terminology; no
compatibility alias is retained.

Tracked artifact renames are detected within the artifact stream. Rust/Python
renames remain detected within the source stream. A rename crossing those
streams is conservatively represented as deletion plus addition, preserving
content coverage without claiming a cross-format identity.

## Analyzer dispatch

A focused `src/artifact.rs` exposes one shared analyzer function signature. A
static table maps `.md` and `.markdown` to the Markdown analyzer and `.tsv` to
the TSV analyzer. Extension matching is ASCII case-insensitive. Files without a
matching entry are classified as generic text and receive only exact raw diff
coverage.

Analyzers receive the path plus optional old and new UTF-8 content. Added and
untracked files have only new content; deleted files have only old content.
Analyzer records are line-safe escaped and sorted by path, semantic kind, then
identity or value. Malformed input produces issue records instead of aborting
the snapshot. Internal analyzer errors abort `changes` and prevent snapshot
storage.

No analyzer trait, factory, runtime registry, configuration file, or new crate
is added. A third analyzer can be added later with one function and one static
dispatch entry.

## Markdown semantics

Markdown analysis is a conservative line-oriented summary. The raw Git diff is
the exact source of artifact coverage; the summary does not claim full
CommonMark parsing.

The analyzer reports additions and removals for:

- Inline links and reference definitions.
- Requirement tokens matching `PREFIX-123`, where `PREFIX` begins with an
  uppercase ASCII letter and otherwise contains uppercase ASCII letters,
  digits, or underscores.
- Spec citations whose local link target ends in `.md` or `.markdown`, with an
  optional fragment.
- Repository-looking paths found in link targets or inline-code spans.
- Algorithm-tagged digests and bare 40- or 64-character hexadecimal digests.

Every extracted digest is labeled `state=claimed`; Graphr does not verify it
against a file or external specification in this slice.

Fenced blocks recognize an opener indented by at most three spaces and made of
at least three identical backticks or tildes. A closer uses the same character
and at least the opener's length. Each changed fence record includes its marker,
info string, line range, and BLAKE3 body digest. The line range is location
metadata, not part of the record's comparison identity. Unclosed fences emit an
issue record and are still inventoried through end of file.

Semantic comparison uses multisets of normalized records. Moving an unchanged
semantic value does not create a semantic addition/removal; its exact movement
remains visible in the raw diff.

## TSV semantics

TSV analysis uses tabs as field separators, strips one trailing carriage return
from CRLF rows, and does not implement quoting or type inference. The first
physical row is the header. An empty file has an empty schema and no data rows.

The analyzer reports:

- Old and new header schemas.
- Duplicate header names.
- Rows whose field count differs from the corresponding header width.
- Duplicate first-column keys and their counts on each side.
- Added, removed, and modified rows.

The first column is always the row key and output includes
`key_basis=first-column`. Rows are matched by `(key, occurrence)`, where
occurrence is the one-based appearance count for that key on each side. Any
duplicate key emits `identity=ambiguous`; the analyzer never silently infers a
different key.

Rows with the same identity are compared positionally. A modified-row record
names every changed column. A column uses its header name only when that name is
present and unique; otherwise it uses a one-based `column_N` label. Extra
duplicate occurrences become additions or removals. Pure row movement is not a
semantic change and remains visible in the raw diff.

An explicit key or composite key may be supplied by a future pinned-spec
feature. This slice adds no key configuration.

## Artifact page and manifest output

The manifest accounts for every safely renderable path using explicit
source/artifact coverage classes. Omitted artifacts include one of these
reasons:

- `binary`
- `invalid-utf8`
- `oversized`
- `non-regular`
- `type-changed`
- `unmerged`

Unsafe path bytes remain an aggregate `skipped unsafe paths` count. Graphr does
not echo an unsafe name merely to attach a per-path reason.

The artifact section starts with page metadata equivalent to the existing
sections. It reports emitted, prior, remaining, partial, and total counts for
artifact files, semantic records, raw diff hunks, and bytes. Semantic records
precede raw patch sections in deterministic path order. Byte cursors may split
an oversized line at a valid UTF-8 boundary exactly as existing source-diff
cursors do; concatenating page payloads reconstructs the stored artifact
section byte-for-byte.

Binary patch metadata may be emitted, but binary content is classified as
omitted and does not become complete. A tracked text diff can exceed the 2 MiB
semantic-read limit and still have exact raw diff coverage; a Markdown or TSV
semantic omission is then reported separately and keeps completion false.
Untracked files over 2 MiB receive no synthetic raw patch and are omitted as
oversized. Aggregate Git output-cap overflow aborts `changes` rather than
returning a partial snapshot.

## Completeness and failures

`review_complete_when_pages_exhausted=true` requires all of the following:

- Every safe Rust/Python path has exact source diff coverage and complete graph
  mapping.
- Every safe non-source UTF-8 path has exact artifact diff coverage.
- Every Markdown/TSV path completed semantic analysis.
- Every boundary-mode dependency change is represented by the existing package
  boundary summary.
- There is no binary, invalid UTF-8, oversized, unsafe, non-regular,
  type-changed, unmerged, stale, capture, or analysis omission.
- Existing graph analysis, changed-symbol, neighborhood, and mapping completion
  conditions remain true.

Malformed Markdown fences, TSV schemas, duplicate keys, and row widths are
review findings, not capture failures: the analyzer emits deterministic issue
records and remains complete. Concurrent mutation, inconsistent Git metadata,
timeout, aggregate output overflow, or internal analyzer failure returns an
error and stores no review snapshot.

All values derived from repository content are escaped so tabs, quotes, and
other characters cannot inject synthetic output records.

## Documentation and review workflow

The MCP tool description, server instructions, README, and bundled
`graphr-review` skill will state that reviewers must exhaust every
`files_next_cursor`, `diff_next_cursor`, `artifacts_next_cursor`, and
`graph_next_cursor` using the exact original arguments.

Captured artifact text no longer triggers the ordinary Markdown/TSV shell
fallback. Any explicit artifact omission leaves coverage incomplete. The docs
will call the capability complete artifact coverage and will not describe it as
generic language support. They will also say to run `index` only after source
edits.

## Verification

Implementation follows test-first red/green cycles. Focused tests cover:

- Tracked, deleted, renamed, and untracked artifact capture without changing the
  existing Rust/Python patch.
- Binary, invalid UTF-8, oversized, unsafe, non-regular, type-changed, and
  unmerged omission reasons.
- Markdown links, requirement IDs, spec citations, paths, claimed digests,
  fenced blocks, malformed fences, escaping, and deterministic ordering.
- TSV schemas, first-column keys, positional row changes, duplicate occurrences,
  malformed widths, escaping, and deterministic ordering.
- Artifact cursor parsing, stale checks, checksums, page accounting, exact
  Unicode/oversized-line reconstruction, and the 8 KiB initial response cap.
- Completion flags with complete generic text and incomplete omitted artifacts.
- An end-to-end mixed review containing Rust, Markdown, TSV, generic text, and
  binary changes, with all four cursor families exhausted.

The final verification commands are:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

## Non-goals and follow-ups

This slice does not add:

- More programming languages.
- A dynamic analyzer plugin or trait framework.
- A path-scoped deep `artifact` tool. A future tool may explicitly page an
  oversized valid UTF-8 path, but it must never bypass unsafe-path validation or
  present arbitrary binary bytes as source.
- Pinned external specification roots.
- Generic-method resolution improvements.
- Builder/parser invariant comparison.
- General claimed-versus-verified trust tracking beyond labeling extracted
  digest values as claimed.
- Confidence or provenance changes for test-gap heuristics.

Each follow-up requires its own design and implementation plan.
