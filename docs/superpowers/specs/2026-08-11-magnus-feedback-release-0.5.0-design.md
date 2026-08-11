# Magnus Feedback and Graphr 0.5.0 Design

## Context

Magnus reported four issues after reviewing the committed range
`165e3f7..ebee315` from `infinite-rfq-platform`. The range adds a 1,051-line
Rust audit module, a two-line Rust change, two Markdown documents, and one TSV
fixture. The prior report raised 32 unresolved static test paths, including
five symbols that the audit tests exercise directly or through public entry
points.

The complete-artifact-coverage work already supplies independent artifact
pages, Markdown and TSV analysis, explicit artifact omissions, and separate
native graph-analysis and whole-review completion signals. Graphr 0.5.0 will
finish the four issue dispositions, correct the remaining Rust call-resolution
gap, and document the client contract precisely.

## Goals

- Make the audit fixture's tests resolve through its module-level
  `use super::*` import.
- Preserve conservative call resolution: uncertain glob imports must not
  create guessed graph edges.
- Document the continuation-token grammar and terminal completion predicate.
- Document the distinction between native analysis completeness and complete
  changed-artifact coverage.
- Document that one `serve` process is bound to one checked-out repository
  path and how to review an unchecked-out committed range.
- Prepare version 0.5.0 locally and verify its package.

## Non-goals

- Do not add general Tree-sitter/LSP language support, a second changes API,
  per-client cursor state, an explicit head parameter, or an HTTP service.
- Do not implement unrestricted or ambiguous Rust glob resolution.
- Do not change risk weights, security-name matching, or the meaning of static
  affected flows.
- Do not tag, push, publish, or close GitHub issues during local preparation.

## Issue Dispositions

### Issue 1: Detect incomplete cursor consumption

Keep the stateless immutable-snapshot protocol. Document that each continuation
token is emitted on a standalone `name=value` line, where `name` is one of
`files_next_cursor`, `diff_next_cursor`, `artifacts_next_cursor`, or
`graph_next_cursor`. Clients split on the first `=` and return the value
verbatim with the original arguments.

Clients must continue until all four cursor names are absent and then require
`review_complete_when_pages_exhausted=true` for complete coverage.
`review_complete=false` is never terminal: follow any cursors, then report
incomplete coverage if none remain and the terminal predicate is false.

### Issue 2: Missing static test paths through public entry points

The exact reproduction uses an inline `tests` module with `use super::*`.
Graphr currently discards `use_wildcard` syntax, so calls such as
`verify_chain()` and `AuditRecord::build()` remain scoped to `audit::tests`
instead of falling back to `audit`. The existing bounded transitive test-path
query cannot find coverage when those call edges are absent.

Preserve a module-level glob prefix in the existing import-binding data.
Explicit imports remain authoritative. Without an explicit binding, call
resolution tries its current lexical candidates before adding glob-derived
candidates, and adds those candidates only when exactly one glob prefix is
available in that lexical module. Unqualified calls derive
`prefix::function`; qualified calls derive `prefix::Type::method`. Multiple,
invalid, or block-local glob imports add no fallback candidates.

This change supplies the missing exact edges; the existing transitive mapping
then marks covered helpers as direct or `indirect-test-covered` and discounts
the existing heuristic test-path risk component. Risk output states
`test_path_confidence=heuristic` and
`test_path_provenance=resolved-static-call-graph`; it does not validate the
absence of runtime tests. General Rust call extraction inside macro token trees
and inferred receiver typing are deferred to 0.6.0. Risk weights do not change.

### Issue 3: Separate native analysis from whole-change coverage

No new output field is needed. Graphr 0.5.0 already emits native graph
`analysis_complete`, per-artifact `analysis_complete`, explicit omission
reasons, transient `review_complete`, and terminal
`review_complete_when_pages_exhausted`. Document those responsibilities:

- `analysis_complete` reports whether its native graph or artifact analyzer
  completed its bounded work.
- `review_complete_when_pages_exhausted` reports whether exhausting all pages
  covers the entire changed source and artifact set without omissions.

Markdown, TSV, and generic text are now reviewed artifacts rather than
unsupported files. Binary, oversized, unsafe, non-regular, type-changed,
unmerged, and other explicit omissions keep whole-review coverage incomplete.

### Issue 4: One server, one repository path

Document that `graphr serve PATH` fixes the repository and working tree for the
process lifetime. `changes(base=...)` compares `base` with that working tree; it
does not accept an independent head ref. To review an unchecked-out committed
range, create a temporary clone or worktree at the desired head, index it, and
run a separate stdio server for that path.

## Data Flow and Safety

Tree-sitter parsing retains the bounded raw glob path without expanding its
contents. Index construction normalizes and consults that prefix only after
explicit-import handling and ordinary lexical candidates. Existing graph-key
lookup still decides whether a candidate resolves uniquely. Ambiguous glob
scopes remain unresolved, which favors missing evidence over false edges.

All existing path-length limits, deterministic ordering, bounded graph queries,
SQLite constraints, snapshot checksums, and output budgets remain unchanged.
No dependency is added.

## Tests and Verification

Use test-driven development:

1. Add a parser test showing that module-level wildcard imports are retained
   while malformed or oversized paths remain rejected by existing limits.
2. Add an index test with an inline `tests` module proving that both
   `verify_chain()` and `AuditRecord::build()` resolve through `use super::*`.
3. Run the focused tests red, implement the minimum parser/index changes, and
   rerun them green.
4. Re-run Graphr against `165e3f7..ebee315` and verify that the five named audit
   symbols have resolved static paths when available, with the heuristic
   confidence and provenance fields above.
5. Update README, MCP instructions, and the bundled review skill where the
   client contract is stated.
6. Bump `Cargo.toml` and `Cargo.lock` from 0.4.0 to 0.5.0.
7. Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`, `cargo build --locked --release`, `git diff --check`, and a
   locked package verification in an isolated target directory.

## GitHub and Release Boundary

Local preparation ends with verified commits on `main` and exact draft closure
notes for issues #1 through #4. The issues remain open until the 0.5.0 commits
are pushed. Tagging, publishing, pushing, and posting or closing GitHub issues
require a later explicit approval.
