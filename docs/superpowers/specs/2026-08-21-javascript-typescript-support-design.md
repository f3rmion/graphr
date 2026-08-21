# JavaScript and TypeScript Support Design

## Context

Graphr currently indexes Rust and Python into one compact SQLite graph and
routes every other safe changed file through artifact review. JavaScript and
TypeScript are the next product languages. Support must preserve the existing
immutable snapshot workflow, bounded MCP output, conservative static
resolution, incremental rebuild safety, and one-binary MCP stdio boundary.

The accepted first release provides the same observable graph contract as
Python: indexed files, definitions, types, methods, imports, direct
re-exports, statically resolvable calls, test edges, source diffs, search,
view, changes, and incremental indexing. It covers JavaScript, JSX,
TypeScript, and TSX without adding a type checker, package manager, or general
language-plugin framework.

## Goals

- Index .js, .jsx, .mjs, .cjs, .ts, .d.ts, .mts, .cts, and .tsx files.
- Extract stable type, function, method, test, import, export, call, and JSX
  component evidence from complete and incomplete source.
- Resolve unique repository-local relative ESM and CommonJS relationships
  across JavaScript and TypeScript files.
- Preserve ambiguity instead of guessing when multiple files or symbols can
  satisfy the same reference.
- Give JS/TS changes the existing source-diff, graph-impact, affected-flow,
  risk, search, and view behavior.
- Reuse the existing graph, reference, SQLite, snapshot, and worker machinery
  with the smallest durable language-specific addition.
- Keep output deterministic and compact.

## Non-goals

- Bare-package, package.json exports/imports, Node module-condition, bundler,
  tsconfig baseUrl, or tsconfig paths resolution.
- TypeScript type checking, declaration merging, overload selection, inferred
  receiver types, source maps, or language-server integration.
- Computed or runtime module specifiers, arbitrary dynamic property
  resolution, eval, macros, decorators beyond their attached source range, or
  framework-specific dependency injection.
- General transitive re-export closure beyond Graphr's existing direct alias
  mechanism.
- Arbitrary export-star name forwarding; export * still records its direct
  module import edge.
- Special node_modules capture or dependency-boundary behavior.
- Runtime test discovery or proof that a test executes.
- A generic analyzer trait, registry, factory, plugin system, migration, or Go
  support.
- New MCP tools, graph node kinds, edge kinds, schema tables, HTTP, UI, or
  editor integration.

## Supported Files and Dialects

Language detection remains case-sensitive and repository-path based.

| Extensions | Stored language | Parser dialect |
| --- | --- | --- |
| .js, .jsx, .mjs, .cjs | javascript | JavaScript, including JSX |
| .ts, .d.ts, .mts, .cts | typescript | TypeScript |
| .tsx | typescript | TSX |

The .d.ts suffix is recognized before the ordinary .ts suffix when deriving a
module name. A ScriptDialect value is internal to the analyzer; it is not
stored as another public language or database value. JavaScript and
TypeScript remain distinct stored languages for filtering and reporting, but
share script-resolution keys so a local JS file can resolve a TS export and
vice versa.

src/git.rs extends the one existing language classifier and every source
pathspec derived from it. Supported script extensions enter the regular
source patch and source inventory streams and are excluded from the artifact
stream. The same extension set is used when counting rejected non-UTF-8 or
unsafe source paths. Existing regular-file, canonical-path, source-size, Git
object, layer, rename, and immutable-capture checks remain authoritative.

## Dependencies and Parser Layout

Add only the official tree-sitter-javascript and tree-sitter-typescript
grammar crates. At design time the compatible published APIs are
tree-sitter-javascript 0.25.0 and tree-sitter-typescript 0.23.2. The former
provides one JavaScript grammar with JSX; the latter provides distinct
TypeScript and TSX grammars. Cargo.lock pins the selected releases.

A new src/javascript.rs owns all JavaScript-family parsing and graph
construction. It does not share a new trait or generic framework with the
working Rust and Python analyzers.

Each indexing worker lazily creates one ScriptParsers value. It holds parser,
query, and cursor state only for dialects that worker encounters and reuses
that state across files. Three small query sources separate syntax that the
grammars cannot all compile:

- queries/ecmascript.scm for common JavaScript/TypeScript syntax;
- queries/typescript.scm for TypeScript-only declarations and imports;
- queries/jsx.scm for JSX syntax used by JavaScript/JSX and TSX.

src/index.rs adds one optional ScriptParsers argument beside its existing Rust
and Python parser state and dispatches JavaScript and TypeScript files to
javascript::add_file. The existing Graph, FileInput, NodeInput, RefInput, and
global resolution path remain the only graph-construction interface.

## Module Identity and Relative Resolution

Every script file has one canonical repository-relative module stem:

- remove .d.ts as one suffix;
- otherwise remove one supported source extension;
- retain the remaining normalized slash-separated repository path.

The file node advertises a shared script:module key for that stem. A file
named index at any depth also advertises its non-empty parent directory as a
module alias. Thus src/a.ts advertises src/a, while src/a/index.ts advertises
both src/a/index and src/a.

A static module specifier is eligible only when it starts with ./ or ../ and
contains no query, fragment, backslash, absolute prefix, control character, or
path traversal above the repository root. Resolution starts at the importing
file's parent directory, lexically normalizes dot segments, removes a
supported source extension from the specifier when present, and checks the
resulting script:module key. This intentionally lets a TypeScript source
import ending in .js resolve its corresponding .ts module.

All supported source dialects share module keys. If two files advertise the
same key, including file-versus-index or JavaScript-versus-TypeScript
collisions, the existing candidate resolver marks that key ambiguous and
emits no edge. No extension-precedence or package heuristic guesses a winner.

Side-effect imports target the file node. Named and default bindings target
export keys. Namespace bindings retain the module stem and resolve a static
member at the call site. Only literal relative specifiers participate;
eligible syntax with a bare or dynamic specifier produces no resolved edge
rather than an indexing error.

## Definitions and Graph Keys

The analyzer maps script syntax onto existing NodeKind values:

- Type: class declarations, stably named class expressions, interfaces, type
  aliases, enums, and TypeScript namespaces;
- Function: function and generator declarations, class methods, and function
  or arrow expressions assigned to a stable identifier;
- Test: accepted test and it callback forms;
- File: the existing source-file node, including ownership of top-level calls.

Nested definitions use the closest captured definition as their parent.
Methods use their class as owner. Anonymous default-exported functions and
classes use the stable local name default. Signatures use the existing bounded
declaration-header convention, and decorator ranges are included when attached
to a captured declaration.

Node identities include the stored language and source path so JavaScript and
TypeScript nodes cannot collide accidentally. Resolution keys use a shared
script namespace:

- module keys identify source files;
- local value keys identify executable definitions inside a module;
- exported value keys identify callable/default/named exports;
- type keys identify interfaces, aliases, classes, enums, and namespaces;
- method keys identify statically owned methods.

Classes and other declarations that inhabit both TypeScript's type and value
spaces may advertise both keys to the same node. Type-only declarations never
advertise a callable value key. Calls consult value or method keys only, so an
interface cannot become a false call target. Import edges may consult the
appropriate type or value key according to import syntax.

## Imports, Exports, and Re-exports

The ESM subset includes:

- default, named, aliased, namespace, side-effect, and type-only imports;
- declarations exported in place;
- local export lists;
- default exports;
- direct named, default, and namespace re-exports from a relative
  module.

An export * declaration records an Imports edge to its relative module but
does not forward arbitrary names. Doing so would require wildcard or
transitive alias semantics that the existing exact alias resolver does not
provide.

The CommonJS subset includes:

- literal relative require calls;
- stable whole-module and destructured require bindings;
- module.exports assignment;
- module.exports.name and exports.name assignment;
- stable object-literal properties assigned through module.exports.

TypeScript import-equals with a literal relative require and export-equals use
the same CommonJS default/module rules. Computed export names, mutation whose
target cannot be named statically, and multi-hop alias chains remain
unresolved.

Direct definitions advertise export keys only when syntax exports them.
Re-exports use the existing RefInput alias_key mechanism: the reference points
at the source module's export key and advertises the current module's export
key as its alias. Existing unique-candidate and ambiguity behavior remains
authoritative. No second resolver or JS-specific edge store is added.

## Calls and Binding Safety

Calls are owned by the closest captured function, method, test, type
initializer, or file node. The analyzer records only candidates supported by
static syntax:

- bare calls and constructor calls to a unique lexical or imported value;
- this.method calls owned by the enclosing class;
- static members on a known local class or namespace import;
- optional-call forms when their underlying target is otherwise supported;
- uppercase JSX/TSX identifier or static member component tags.

Lowercase JSX intrinsic elements are ignored. A JSX component produces the
same Calls reference as an ordinary invocation, so existing affected-flow and
risk logic needs no special edge kind.

Parameters, local declarations, catch bindings, imports, and nested
definitions form conservative lexical binding maps. A local binding suppresses
an imported or outer candidate with the same name. Multiple bindings or
candidate nodes make the reference unresolved. Member calls requiring
receiver-type inference, computed members, call/apply/bind target recovery,
callbacks passed through values, and dynamic imports are not guessed.

## Test Nodes

A call becomes a Test definition only when:

- its file path has a .test or .spec segment immediately before a supported
  source extension, or lies below a __tests__ directory; and
- its callee is test or it, optionally followed by only or skip; and
- it has a function or arrow callback.

The test node name comes from a static string or template without
substitutions. If no static title exists, the deterministic fallback is
test@LINE. The callback range owns calls and nested definitions. describe is
not a test node, but tests nested inside its callback are still found.

Because the source node kind is Test, the existing resolver emits TEST_CALLS
without any schema or risk-analysis change. This is static framework-shaped
evidence, not runtime execution proof.

## Incomplete Syntax, Limits, and Errors

Tree-sitter error nodes do not reject a whole file. Captures outside broken
regions still produce partial graph evidence, matching existing Rust and
Python behavior. Missing optional fields, unsupported syntax, and unresolved
references are skipped conservatively.

The current UTF-8 requirement, 2 MiB source limit, immutable capture checks,
cancellation checks, Tree-sitter match limit, 1,024-byte qualified-path limit,
line-number conversion checks, and bounded signatures apply unchanged.
Required query captures are validated at parser construction. Grammar/query
construction failures, match-limit exhaustion, path-limit overflow, and
numeric overflow fail the index job with a dialect-specific error rather than
publishing a partial corrupt graph.

## Incremental and Cache Behavior

Language joins path, Git object or captured-content identity, parse context,
and byte size in the existing reuse decision. Script parse context records
only the selected dialect; module identity remains path-derived. Unchanged
files reuse stored graph rows, while additions, edits, deletions, renames, and
language changes use the current replacement and global reference-resolution
path.

GRAPH_ANALYZER_VERSION is incremented. This gives new graph images a distinct
identity, preventing an old Rust/Python-only image from being accepted as a
JS/TS-complete build. The SQLite schema, CACHE_FORMAT_VERSION, and
REVIEW_FORMAT_VERSION do not change because their structures and encodings do
not change. Existing immutable snapshots remain immutable; new indexing
builds use the new analyzer identity. No migration or compatibility layer is
added.

## User-Facing Contract

README, Cargo package description and keywords, MCP tool/server guidance, and
the bundled graphr-review skill state that Graphr indexes Rust, Python,
JavaScript, and TypeScript, including JSX and TSX. Source-diff and rename
documentation names all four stored languages rather than hard-coding Rust
and Python.

Search and view require no new filters: javascript and typescript are stored
language values, while node kinds and edge kinds stay unchanged. changes
continues to use the same bounded files, diff, artifacts, and graph cursor
streams and the same terminal completeness predicate.

## Tests and Verification

Development is test-driven and leaves the smallest checks that exercise the
new semantics:

1. Add one focused javascript.rs analyzer test that parses incomplete JS,
   TypeScript, and TSX inputs and asserts representative definitions, direct
   exports, ESM/CommonJS bindings, test callbacks, shadow suppression, calls,
   and JSX component evidence.
2. Add focused git.rs coverage proving every approved extension enters the
   source inventory and patch, leaves the artifact stream, and contributes to
   rejected-source counts for unsafe or non-UTF-8 paths. Preserve existing
   non-regular, oversized, type-changed, and unmerged omission behavior.
3. Add one mixed end-to-end fixture covering every extension, a .d.ts type,
   cross-JS/TS relative imports, extension and index aliases, ESM exports and
   direct re-exports, CommonJS require/exports, a class method, stable arrow,
   conservative collisions, test/it callbacks, and JSX/TSX components.
4. Through that fixture, assert stored language counts, representative
   IMPORTS/CALLS/TEST_CALLS edges, absence of ambiguous or shadowed edges, and
   MCP search, view, and changes output.
5. Mutate the fixture through edit, add, rename, delete, and re-add operations.
   After each operation, compare the incremental immutable graph with a fresh
   oracle graph and verify affected edges appear or disappear.
6. Update user-facing language statements and test exact MCP guidance where
   it is already asserted.
7. Run:

       cargo fmt --check
       cargo clippy --all-targets -- -D warnings
       cargo test
       cargo build --locked --release
       git diff --check

No benchmark, release bump, tag, push, package publish, package-resolution
fixture, or general analyzer refactor is part of this change.
