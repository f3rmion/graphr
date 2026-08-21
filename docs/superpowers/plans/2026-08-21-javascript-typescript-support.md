# JavaScript and TypeScript Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add deterministic JavaScript, JSX, TypeScript, and TSX indexing with the same observable graph contract as Python: definitions, imports, direct re-exports, resolvable calls, conventional test callbacks, JSX component calls, MCP search/view, and incremental change graphs.

**Architecture:** Keep the existing Rust and Python analyzers unchanged. Add one self-contained `javascript` analyzer backed by the official Tree-sitter JavaScript, TypeScript, and TSX grammars; route all nine script extensions through the existing Git classifier and worker dispatch; and resolve cross-file script references through shared `script:` keys in the existing graph resolver. This is one concrete analyzer path, not a generic language framework.

**Tech Stack:** Rust 2024, `tree-sitter` 0.26.11, `tree-sitter-javascript` 0.25.0, `tree-sitter-typescript` 0.23.2, Git CLI, SQLite/rusqlite, Cargo.

**Spec:** `docs/superpowers/specs/2026-08-21-javascript-typescript-support-design.md`.

## Global Constraints

- Execute this plan from a dedicated feature worktree created with `superpowers:using-git-worktrees`; do not implement it in the planning checkout.
- Build one Rust binary for Codex and Claude over MCP stdio.
- Preserve the existing Rust and Python behavior. Add no language trait, factory, plugin registry, migration, HTTP surface, editor integration, or schema field.
- Add no package/condition/bundler/tsconfig resolver, type checker, declaration merger, overload selector, receiver inference, source-map/LSP support, special `node_modules` behavior, runtime test discovery, framework integration, Go support, or new MCP tool/node/edge kind.
- Recognize exactly `.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`, `.mts`, `.cts`, and `.d.ts`, case-sensitively.
- Store `Language::JavaScript` for JavaScript/JSX/MJS/CJS and `Language::TypeScript` for TypeScript/TSX/MTS/CTS/declaration files. Keep JavaScript, TypeScript, and TSX only as private parser dialects.
- Resolve only repository-local relative specifiers beginning `./` or `../`. Reject bare packages, aliases, absolute paths, backslashes, query/fragment suffixes, control bytes, and paths that escape the repository.
- Resolve a module alias only when exactly one indexed file advertises it. Never guess through an ambiguous file-versus-directory collision.
- Strip a supported extension from an import specifier before resolution so emitted `.js` specifiers can target TypeScript source. Strip `.d.ts` as one suffix when constructing a file stem.
- Support direct named/default/namespace re-exports. `export * from` creates only a direct module import edge; do not compute transitive export-star closure.
- Keep JavaScript value keys separate from TypeScript type keys. A class may advertise both; an interface or type alias must never satisfy a runtime call.
- Recognize tests only in `*.test.*`, `*.spec.*`, or a `__tests__` directory and only for `test`/`it` callback forms, including `.only` and `.skip`.
- Treat uppercase JSX identifiers and member roots as calls; ignore lowercase intrinsic elements.
- Reuse the current graph schema, resolver, immutable graph publication, rollback, cancellation, size, qualified-path, signature, match-limit, and deterministic-ordering behavior.
- Bump only `GRAPH_ANALYZER_VERSION`. Do not bump `SCHEMA_VERSION`, `CACHE_FORMAT_VERSION`, or `REVIEW_FORMAT_VERSION`.
- Add only the two official Tree-sitter grammar crates already selected in the spec.
- Do not change the package version, benchmark, tag, release, publish, or push.
- Use test-driven development: add the smallest failing check before each production slice.
- Before completion, run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --locked --release`, and `git diff --check`.

## File Responsibilities

- `Cargo.toml` / `Cargo.lock`: add the two official grammar crates and update package metadata.
- `src/main.rs`: register the concrete `javascript` module.
- `src/javascript.rs`: own script dialect selection, parser/query reuse, syntax extraction, module normalization, scope/binding tracking, graph-node construction, and script reference keys.
- `queries/ecmascript.scm`: capture syntax common to ECMAScript, TypeScript, and TSX.
- `queries/typescript.scm`: capture TypeScript-only declaration and module syntax.
- `queries/jsx.scm`: capture JSX element names.
- `src/git.rs`: remain the single source classifier and source/artifact routing boundary.
- `src/index.rs`: own worker-local parser state and dispatch into the concrete analyzer.
- `src/workspace.rs`: invalidate old immutable graphs by incrementing the analyzer version.
- `tests/e2e.rs`: prove all extensions, cross-language resolution, ambiguity handling, MCP behavior, and incremental parity.
- `src/mcp.rs`, `README.md`, `.agents/skills/graphr-review/SKILL.md`: describe the expanded supported-language contract.
- `src/store.rs` and the SQLite schema: no change.

---

### Task 1: Parse ECMAScript, TypeScript, and TSX with reusable worker state

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/main.rs`
- Create: `src/javascript.rs`
- Create: `queries/ecmascript.scm`
- Create: `queries/typescript.scm`
- Create: `queries/jsx.scm`

**Interfaces:**

Consumes:

```rust
path: &str
source: &str
```

Produces:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScriptDialect {
    JavaScript,
    TypeScript,
    Tsx,
}

#[derive(Default)]
pub(crate) struct ScriptParsers {
    javascript: Option<ScriptParser>,
    typescript: Option<ScriptParser>,
    tsx: Option<ScriptParser>,
}

impl ScriptParsers {
    fn parse(&mut self, path: &str, source: &str) -> Result<ParsedFile, String>;
}
```

`ParsedFile` remains private. It contains definitions, module statements, lexical bindings, and calls in source order. Each definition records its node kind, stable name, containing-definition index, source line range, bounded signature, and body byte range. Each lexical binding records its name and the narrowest byte range in which it is authoritative. Each call records its containing-definition index, byte offset, target shape, and line.

**Steps:**

- [ ] Add the grammar dependencies and module declaration, then add a failing unit test at the bottom of `src/javascript.rs`:

```toml
tree-sitter-javascript = "0.25.0"
tree-sitter-typescript = "0.23.2"
```

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzes_javascript_typescript_and_tsx() {
        let mut parsers = ScriptParsers::default();

        let javascript = parsers
            .parse(
                "src/service.js",
                r#"
                    import run, { work as execute } from "./worker.js";
                    export class Service {
                        dispatch() { execute(); }
                    }
                    export const helper = () => run();
                    const broken = (
                "#,
            )
            .unwrap();
        assert_eq!(
            javascript.definition_names(),
            ["Service", "dispatch", "helper"]
        );
        assert_eq!(javascript.relative_modules(), ["./worker.js"]);
        assert_eq!(javascript.call_names(), ["execute", "run"]);

        let typescript = parsers
            .parse(
                "src/contracts.d.ts",
                r#"
                    export interface Config { value: string }
                    export type Identifier = string;
                    export enum State { Ready }
                    export namespace API {
                        function load(): Config;
                    }
                "#,
            )
            .unwrap();
        assert_eq!(
            typescript.definition_names(),
            ["Config", "Identifier", "State", "API", "load"]
        );

        let tsx = parsers
            .parse(
                "src/panel.tsx",
                r#"
                    import { Widget as View } from "./widget";
                    export const Panel = () => <View />;
                "#,
            )
            .unwrap();
        assert_eq!(tsx.definition_names(), ["Panel"]);
        assert_eq!(tsx.jsx_component_names(), ["View"]);
    }
}
```

- [ ] Run `cargo test javascript::tests::analyzes_javascript_typescript_and_tsx -- --exact`. Expect compilation to fail because `ScriptParsers` and `ParsedFile` do not exist.

- [ ] Add the three query files. Keep the common query broad and let Rust inspect node kinds and fields:

```scheme
; queries/ecmascript.scm
[
  (class_declaration)
  (class)
  (function_declaration)
  (generator_function_declaration)
  (function_expression)
  (generator_function)
  (arrow_function)
  (method_definition)
] @definition

[
  (import_statement)
  (export_statement)
  (assignment_expression)
] @module

[
  (call_expression)
  (new_expression)
] @call

[
  (formal_parameters)
  (variable_declarator)
  (catch_clause)
] @binding
```

```scheme
; queries/typescript.scm
[
  (abstract_class_declaration)
  (interface_declaration)
  (type_alias_declaration)
  (enum_declaration)
  (internal_module)
  (module)
  (function_signature)
  (method_signature)
  (abstract_method_signature)
] @typescript_definition

(import_alias) @typescript_module
```

```scheme
; queries/jsx.scm
[
  (jsx_opening_element name: (_) @jsx)
  (jsx_self_closing_element name: (_) @jsx)
]
```

- [ ] Implement dialect selection with the `.d.ts` check before the ordinary extension check:

```rust
impl ScriptDialect {
    fn for_path(path: &str) -> Option<Self> {
        if path.ends_with(".d.ts") {
            return Some(Self::TypeScript);
        }
        match path.rsplit_once('.').map(|(_, extension)| extension) {
            Some("js" | "jsx" | "mjs" | "cjs") => Some(Self::JavaScript),
            Some("ts" | "mts" | "cts") => Some(Self::TypeScript),
            Some("tsx") => Some(Self::Tsx),
            _ => None,
        }
    }

    const fn parse_context(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
        }
    }
}
```

- [ ] Implement `ScriptParser::new` with one `tree_sitter::Parser`, one compiled `Query`, and one `QueryCursor`. Set the grammar from the dialect and build the query source exactly once:

```rust
const ECMASCRIPT_QUERY: &str = include_str!("../queries/ecmascript.scm");
const TYPESCRIPT_QUERY: &str = include_str!("../queries/typescript.scm");
const JSX_QUERY: &str = include_str!("../queries/jsx.scm");

fn query_source(dialect: ScriptDialect) -> String {
    match dialect {
        ScriptDialect::JavaScript => format!("{ECMASCRIPT_QUERY}\n{JSX_QUERY}"),
        ScriptDialect::TypeScript => format!("{ECMASCRIPT_QUERY}\n{TYPESCRIPT_QUERY}"),
        ScriptDialect::Tsx => {
            format!("{ECMASCRIPT_QUERY}\n{TYPESCRIPT_QUERY}\n{JSX_QUERY}")
        }
    }
}
```

Validate every required capture name during construction. Reuse Tree-sitter's bounded query cursor and check `did_exceed_match_limit` after every parse. Grammar setup, query compilation, parser execution, or query overflow returns a dialect-specific error.

- [ ] Add a private captured-node record. Collect query captures, deduplicate them by `(start_byte, end_byte, capture_name)`, sort that tuple, and check `did_exceed_match_limit` before interpreting syntax.

Here, stable means a direct identifier variable declarator or assignment, or a direct identifier class field. A class-field function is a method owned by that class. Destructuring targets, member assignments, computed fields, and object-literal property inference are out of scope.

- [ ] Interpret definition captures from node kinds and fields, compute the smallest containing-definition parent by byte range, and ignore an anonymous function unless it is a default export, stable initializer, or recognized test callback.

- [ ] Interpret binding captures and derive their lexical ranges from syntax ancestry: parameters and `var` bind in their function, `let`/`const` and class declarations bind in the nearest block, catch parameters bind in the catch body, imports bind in the module, and a named nested definition binds in its parent scope while retaining a self-binding for recursion.

- [ ] Interpret call captures only after definitions and bindings exist. Assign each call to the smallest containing definition body or to the file, and store its byte offset so later binding lookup can select the smallest containing range; two targets in the same winning range are ambiguous.

- [ ] Build signatures from the start of the declaration, including attached decorators, through the start of its body; trim trailing whitespace and truncate only at a UTF-8 boundary within the existing signature limit. Decorators affect source coverage and signature text only—do not interpret them.

- [ ] Implement only the private target shapes the graph slice needs:

```rust
enum CallTarget {
    Identifier(String),
    Member { object: String, property: String },
    ThisMethod(String),
    Jsx(String),
}

enum DefinitionKind {
    Type { runtime_value: bool },
    Function,
    Method,
    Test,
}
```

Extract bare and optional calls, `new` expressions, `this.method` calls, static identifier/member calls, and JSX names. Do not infer computed properties, arbitrary receiver types, `call`/`apply`, or lowercase JSX intrinsics. Put the assertion-only `definition_names`, `relative_modules`, `call_names`, `export_names`, `test_names`, and `jsx_component_names` accessors behind `#[cfg(test)]`.

- [ ] Preserve partial-file behavior: Tree-sitter error nodes do not invalidate the file. The Git capture boundary continues to enforce UTF-8 and the 2 MiB source limit; the analyzer enforces the existing 1,024-byte qualified-path limit, bounded signature length, line conversion, and query match-limit failure. Keep cancellation in the surrounding index/resolve loops rather than adding a second parser cancellation mechanism.

- [ ] Run the focused test again. Expect it to pass.

- [ ] Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] Commit:

```text
feat(parser): extract JavaScript and TypeScript syntax
```

---

### Task 2: Emit script definitions and local references through the existing graph model

**Files:**

- Modify: `src/git.rs`
- Modify: `src/javascript.rs`
- Modify: `src/index.rs`

**Interfaces:**

Consumes:

```rust
pub(crate) fn add_file(
    graph: &mut Graph,
    source: &Source,
    language: Language,
    parsers: &mut ScriptParsers,
) -> Result<(), String>;
```

Produces the existing `NodeInput` and `RefInput` records only. No store/schema type changes.

Script keys use these namespaces:

```text
script:module:<module-stem>
script:value:<module-stem>::<lexical-path>
script:type:<module-stem>::<lexical-path>
script:export-value:<module-stem>::<export-name>
script:export-type:<module-stem>::<export-name>
script:method:<module-stem>::<owner-path>::<method-name>
```

**Steps:**

- [ ] Extend the single focused analyzer test in `src/javascript.rs` with failing graph assertions:

```rust
let source = Source {
    path: "src/service.ts".to_owned(),
    text: r#"
        export interface Config { value: string }
        export class Service {
            dispatch(config: Config) { return this.finish(config); }
            finish(config: Config) { return run(config); }
        }
        export function run(config: Config) { return helper(config); }
        function helper(config: Config) { return config.value; }
        export function shadow(run: () => void) { run(); }
    "#
    .to_owned(),
};
let mut graph = Graph::default();
add_file(
    &mut graph,
    &source,
    Language::TypeScript,
    &mut ScriptParsers::default(),
)
.unwrap();

assert!(has_node(&graph, NodeKind::Type, "Config"));
assert!(has_node(&graph, NodeKind::Type, "Service"));
assert!(has_node(&graph, NodeKind::Function, "run"));
assert!(has_node(&graph, NodeKind::Function, "helper"));
assert!(has_node(&graph, NodeKind::Function, "dispatch"));
assert!(has_ref(
    &graph,
    "dispatch",
    "script:method:src/service::Service::finish"
));
assert!(has_ref(
    &graph,
    "run",
    "script:value:src/service::helper"
));
assert!(!has_ref(
    &graph,
    "shadow",
    "script:value:src/service::run"
));
```

Use small `has_node` and `has_ref` test helpers that inspect the existing graph vectors; do not expose a production query API.

- [ ] Run `cargo test javascript::tests::analyzes_javascript_typescript_and_tsx -- --exact`. Expect compilation to fail because the new `Language` variants and `add_file` do not exist.

- [ ] Extend the existing concrete language enum without changing serialization for Rust or Python:

```rust
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
}
```

Add exact lowercase `as_str`/`parse` values `javascript` and `typescript`. Update every exhaustive match; add no catch-all arm.

- [ ] Give every node a collision-safe identity containing stored language, source path, node kind, lexical path, line, and capture ordinal. Keep this identity separate from the shared cross-language resolution keys:

```rust
fn identity(
    language: Language,
    path: &str,
    kind: &str,
    scope: &str,
    line: usize,
    ordinal: usize,
) -> String {
    let language = language.as_str();
    format!(
        "script-node:{}#{language}:{}#{path}:{}#{kind}:{}#{scope}:{line}:{ordinal}",
        language.len(),
        path.len(),
        kind.len(),
        scope.len(),
    )
}
```

- [ ] Add key helpers in `src/javascript.rs`. Validate module stems and joined lexical paths against the same qualified-path limit used by Rust/Python before adding fixed key prefixes:

```rust
fn module_key(stem: &str) -> String {
    format!("script:module:{stem}")
}

fn local_value_key(stem: &str, lexical_path: &str) -> String {
    format!("script:value:{stem}::{lexical_path}")
}

fn local_type_key(stem: &str, lexical_path: &str) -> String {
    format!("script:type:{stem}::{lexical_path}")
}

fn export_value_key(stem: &str, name: &str) -> String {
    format!("script:export-value:{stem}::{name}")
}

fn export_type_key(stem: &str, name: &str) -> String {
    format!("script:export-type:{stem}::{name}")
}
```

- [ ] Emit one `NodeKind::Type` for classes, named class expressions, interfaces, type aliases, enums, and TypeScript namespaces. Classes, enums, and namespaces advertise the type and runtime-value keys their syntax represents; interface/type-alias nodes advertise only type keys. Emit `NodeKind::Function` for functions, generators, class methods, and function/arrow expressions assigned to a stable identifier. Use `default` for an anonymous default export.

- [ ] Preserve lexical ownership in the node identity and `parent_key`. Map `this.method` and a static member on a known local class to the containing class's method key. Before producing a bare-call reference, suppress an outer candidate when the identifier is shadowed by a parameter, local declaration, catch binding, or nested definition in the same or nearer scope. Task 4 adds imported bindings as targeted bindings rather than suppressing them.

- [ ] Emit calls with no containing definition from the file node. This preserves the approved top-level-call ownership contract without adding a new node kind.

- [ ] Add `ScriptDialect::language` and `javascript::parse_context(path)`:

```rust
pub(crate) fn parse_context(path: &str) -> Option<&'static str> {
    ScriptDialect::for_path(path).map(ScriptDialect::parse_context)
}
```

Extend the same focused test with all nine path-to-context assertions so `.jsx`/`.cjs` use `javascript`, `.ts`/`.d.ts`/`.mts`/`.cts` use `typescript`, and `.tsx` uses `tsx`.
At the start of `add_file`, reject a path whose selected dialect does not map to the supplied stored language; append one assertion that passing `Language::JavaScript` for `src/service.ts` returns an error.

- [ ] In `src/index.rs`, add one worker-local `ScriptParsers` beside the existing Rust and Python parser options, pass it through `build_file`, and dispatch both new stored languages to `javascript::add_file`. Do not construct a parser for a worker until that worker sees its first file of the dialect.

- [ ] Extend `assign_parse_contexts` to return `javascript`, `typescript`, or `tsx` for script paths while preserving Rust target-context and Python behavior.

- [ ] Run the focused analyzer test. Expect it to pass.

- [ ] Run the existing Rust and Python analyzer tests to prove the new dispatch did not alter them:

```bash
cargo test rust
cargo test python
```

- [ ] Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] Commit:

```text
feat(index): emit JavaScript and TypeScript graph nodes
```

---

### Task 3: Route every supported script extension through source capture

**Files:**

- Modify: `src/git.rs`
- Modify: `src/workspace.rs`
- Test: `src/git.rs`

**Interfaces:**

The existing `language_for_path` remains the single classifier. The existing source pathspec and artifact-exclusion pathspec must contain the same extension set.

**Steps:**

- [ ] Add a failing table-driven classifier test:

```rust
#[test]
fn classifies_every_script_extension_case_sensitively() {
    let cases = [
        ("src/a.js", Language::JavaScript),
        ("src/a.jsx", Language::JavaScript),
        ("src/a.mjs", Language::JavaScript),
        ("src/a.cjs", Language::JavaScript),
        ("src/a.ts", Language::TypeScript),
        ("src/a.tsx", Language::TypeScript),
        ("src/a.mts", Language::TypeScript),
        ("src/a.cts", Language::TypeScript),
        ("src/a.d.ts", Language::TypeScript),
    ];
    for (path, expected) in cases {
        assert_eq!(language_for_path(path), Some(expected), "{path}");
    }
    assert_eq!(language_for_path("src/a.JS"), None);
    assert_eq!(language_for_path("src/a.TS"), None);
}
```

- [ ] Add a failing source-routing test which creates one untracked file for every extension, calls `Repository::capture_sources` with untracked files enabled, and asserts:

```rust
let expected = [
    ("src/a.cjs", Language::JavaScript),
    ("src/a.cts", Language::TypeScript),
    ("src/a.d.ts", Language::TypeScript),
    ("src/a.js", Language::JavaScript),
    ("src/a.jsx", Language::JavaScript),
    ("src/a.mjs", Language::JavaScript),
    ("src/a.mts", Language::TypeScript),
    ("src/a.ts", Language::TypeScript),
    ("src/a.tsx", Language::TypeScript),
];
assert_eq!(
    sources
        .files
        .iter()
        .map(|source| (source.path.as_str(), source.language))
        .collect::<Vec<_>>(),
    expected
);

let mut untracked_inventory = Vec::new();
for (path, _) in expected {
    untracked_inventory.extend_from_slice(path.as_bytes());
    untracked_inventory.push(0);
}
let untracked = capture_untracked(
    &root,
    &untracked_inventory,
    DependencyMode::Boundary,
    true,
    &AtomicBool::new(false),
)
.unwrap();
assert!(untracked.artifacts.files.is_empty());
assert!(untracked.artifacts.analysis.is_empty());
for (path, _) in expected {
    assert!(String::from_utf8_lossy(&untracked.source_patch).contains(path));
}
```

Build `untracked_inventory` as the NUL-separated byte list already consumed by `capture_untracked`. Also call `parse_inventory_path` with non-UTF-8 and control-character paths ending in supported script suffixes; assert each is skipped and increments the existing rejected-source counter exactly once. Assert absolute and parent-traversing script paths still return the existing terminal `"Git returned an unsafe changed path"` error rather than being downgraded to a skip.

- [ ] Run the two tests. Expect the classifier test to return `None` and the routing test to classify the files as artifacts or omit them.

- [ ] Extend the existing source pathspec list and matching artifact exclusions with all nine script suffixes. Keep the lists explicit and adjacent to the Rust/Python entries; do not add a configurable extension registry.

- [ ] Implement `language_for_path` with `Path::extension` for ordinary suffixes and the explicit `.d.ts` check first. Keep the existing safe-relative-path validation before classification.

- [ ] Extend the raw-byte supported-source suffix check used by inventory parsing so invalid UTF-8 script paths are skipped safely rather than entering artifact capture.

- [ ] Increment `GRAPH_ANALYZER_VERSION` from `1` to `2` in `src/workspace.rs`. Change no other version constant.

- [ ] Run:

```bash
cargo test git::tests::classifies_every_script_extension_case_sensitively -- --exact
cargo test git::tests::routes_script_sources_away_from_artifact_capture -- --exact
cargo test git::
```

Expect all Git capture tests to pass.

- [ ] Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] Commit:

```text
feat(git): capture JavaScript and TypeScript sources
```

---

### Task 4: Resolve relative ESM imports and direct re-exports

**Files:**

- Modify: `src/javascript.rs`
- Modify: `tests/e2e.rs`

**Interfaces:**

```rust
fn module_aliases(path: &str) -> Result<Vec<String>, String>;
fn relative_module(importer: &str, specifier: &str) -> Result<Option<String>, String>;
```

`module_aliases` returns the extensionless file stem and, for an `index` file, its parent-directory alias. `relative_module` returns one normalized repository-relative extensionless stem, `Ok(None)` for intentionally unsupported syntax, or an error for bounded-path overflow.

The parsed module model distinguishes value and type spaces:

```rust
enum ImportedName {
    Default,
    Named(String),
    Namespace,
}

struct ImportBinding {
    local: String,
    imported: ImportedName,
    type_only: bool,
}

struct Import {
    source: Option<usize>,
    module: String,
    bindings: Vec<ImportBinding>,
    line: usize,
}

enum ExportTarget {
    Definition(usize),
    Local(String),
    ReExport {
        module: String,
        imported: ImportedName,
    },
    Star {
        module: String,
    },
}

struct Export {
    target: ExportTarget,
    exported: Option<String>,
    type_only: bool,
    line: usize,
}
```

**Steps:**

- [ ] Extend the single focused analyzer test with failing normalization assertions:

```rust
assert_eq!(module_aliases("src/core.ts").unwrap(), ["src/core"]);
assert_eq!(
    module_aliases("src/core/index.tsx").unwrap(),
    ["src/core/index", "src/core"]
);
assert_eq!(module_aliases("src/types.d.ts").unwrap(), ["src/types"]);
assert!(module_aliases("../src/core.ts").is_err());
assert!(module_aliases("/src/core.ts").is_err());
assert_eq!(
    relative_module("src/client.ts", "./core.js")
        .unwrap()
        .as_deref(),
    Some("src/core")
);
assert_eq!(
    relative_module("tests/client.spec.ts", "../src/core")
        .unwrap()
        .as_deref(),
    Some("src/core")
);
for rejected in [
    "react",
    "@/core",
    "/src/core",
    "./core?raw",
    "./core#fragment",
    ".\\core",
    "../../outside",
] {
    assert_eq!(
        relative_module("src/client.ts", rejected).unwrap(),
        None,
        "{rejected}"
    );
}
```

- [ ] Run `cargo test javascript::tests::analyzes_javascript_typescript_and_tsx -- --exact`. Expect compilation to fail because the helpers do not exist.

- [ ] Implement suffix stripping in this order:

```rust
const SCRIPT_SUFFIXES: [&str; 9] = [
    ".d.ts", ".jsx", ".mjs", ".cjs", ".tsx", ".mts", ".cts", ".js", ".ts",
];

fn strip_script_suffix(value: &str) -> &str {
    SCRIPT_SUFFIXES
        .iter()
        .find_map(|suffix| value.strip_suffix(suffix))
        .unwrap_or(value)
}
```

Use `std::path::Component` to normalize against the importer's parent. Return `Ok(None)` for a non-relative prefix, any `RootDir`/`Prefix` component, an unmatched `ParentDir`, empty stem, backslash, control character, query, or fragment. Return `Err` when the normalized output exceeds the existing path limit. Do not read `package.json` or `tsconfig.json`.
`module_aliases` applies the existing safe-relative-source-path rules and returns an error for an unsupported suffix, empty/unsafe path, or path-limit overflow. It simply omits the empty parent alias for a root `index` file.

- [ ] Add an ESM-only end-to-end fixture in `tests/e2e.rs`. Construct identical incremental and oracle repositories, create these exact files, commit the baseline in both, and initially index only the incremental repository:

```rust
const TYPES: &str = r#"
    export interface Config { value: string }
"#;
const CORE: &str = r#"
    import type { Config } from "./types.js";
    function helper(config: Config) { return config.value; }
    export { helper as exposedHelper };
    export function run(config: Config) { return helper(config); }
    export class Service {
        static create() { return new Service(); }
        dispatch(config: Config) { return this.finish(config); }
        finish(config: Config) { return run(config); }
    }
    export function makeService() { return Service.create(); }
    export function misuseType() { return Config(); }
    export function shadow(run: () => void) { run(); }
"#;
const BRIDGE: &str = r#"
    export { run as execute } from "./core.js";
    export * as widgets from "./widget";
    export * from "./types.js";
"#;
const WIDGET: &str = r#"
    export default function DefaultWidget() { return <section />; }
    export function Widget() { return <div />; }
    export function div() { return null; }
"#;
const UI: &str = r#"
    import DefaultWidget, { Widget, div } from "./widget";
    import * as UI from "./widget";
    export const Panel = () =>
        <><DefaultWidget /><Widget /><UI.Widget /><div /></>;
"#;
const MODERN: &str = r#"
    import { execute } from "./bridge";
    import { exposedHelper } from "./core";
    export function invoke() { return execute({ value: "mts" }); }
    export function invokeLocalExport() {
        return exposedHelper({ value: "local" });
    }
"#;
const ENTRY: &str = r#"
    import "./bridge";
    import { duplicate } from "./collision";
    import { indexed } from "./directory";
    function bootstrap() { return true; }
    bootstrap();
    export function unresolved() { return duplicate(); }
    export function fromIndex() { return indexed(); }
"#;

fn write_script_fixture(root: &Path) {
    fs::create_dir_all(root.join("src/collision")).unwrap();
    fs::create_dir_all(root.join("src/directory")).unwrap();
    fs::write(root.join("src/types.d.ts"), TYPES).unwrap();
    fs::write(root.join("src/core.ts"), CORE).unwrap();
    fs::write(root.join("src/bridge.js"), BRIDGE).unwrap();
    fs::write(root.join("src/widget.jsx"), WIDGET).unwrap();
    fs::write(root.join("src/ui.tsx"), UI).unwrap();
    fs::write(root.join("src/modern.mts"), MODERN).unwrap();
    fs::write(root.join("src/entry.mjs"), ENTRY).unwrap();
    fs::write(
        root.join("src/collision.js"),
        "export function duplicate() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/collision/index.ts"),
        "export function duplicate() { return 2; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/directory/index.ts"),
        "export function indexed() { return 3; }\n",
    )
    .unwrap();
}
```

- [ ] Add one path-sensitive edge helper instead of repeating SQL:

```rust
fn named_edge_kind_count(
    path: &Path,
    source_path: &str,
    source: &str,
    target_path: &str,
    target: &str,
    kind: &str,
) -> i64 {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM edges edge
               JOIN nodes source ON source.id=edge.source_id
               JOIN files source_file ON source_file.id=source.file_id
               JOIN nodes target ON target.id=edge.target_id
               JOIN files target_file ON target_file.id=target.file_id
              WHERE source_file.path=?1 AND source.name=?2
                AND target_file.path=?3 AND target.name=?4 AND edge.kind=?5",
            [source_path, source, target_path, target, kind],
            |row| row.get(0),
        )
        .unwrap()
}
```

- [ ] Add the failing ESM assertions:

```rust
#[test]
fn javascript_typescript_index_search_view_and_incremental_changes_over_mcp() {
    let incremental = Fixture::new();
    let oracle = Fixture::new();
    for root in [&incremental.path, &oracle.path] {
        write_script_fixture(root);
        init_git(root);
        git(root, &["add", "--", "."]);
        git(
            root,
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
    }

    index_repository(&incremental.path);
    assert_eq!(language_file_count(&incremental.path, "javascript"), 4);
    assert_eq!(language_file_count(&incremental.path, "typescript"), 6);
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/core.ts",
            "run",
            "src/core.ts",
            "helper",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/entry.mjs",
            "fromIndex",
            "src/directory/index.ts",
            "indexed",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/entry.mjs",
            "src/entry.mjs",
            "src/entry.mjs",
            "bootstrap",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/core.ts",
            "makeService",
            "src/core.ts",
            "create",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/modern.mts",
            "invoke",
            "src/core.ts",
            "run",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/modern.mts",
            "invokeLocalExport",
            "src/core.ts",
            "helper",
            "CALLS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/entry.mjs",
            "src/entry.mjs",
            "src/bridge.js",
            "src/bridge.js",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/bridge.js",
            "src/bridge.js",
            "src/types.d.ts",
            "src/types.d.ts",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/bridge.js",
            "src/bridge.js",
            "src/widget.jsx",
            "src/widget.jsx",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(
        named_edge_kind_count(
            &incremental.path,
            "src/core.ts",
            "src/core.ts",
            "src/types.d.ts",
            "Config",
            "IMPORTS",
        ),
        1
    );
    assert_eq!(named_edge_count(&incremental.path, "shadow", "run"), 0);
    assert_eq!(named_edge_count(&incremental.path, "misuseType", "Config"), 0);
    assert_eq!(named_edge_count(&incremental.path, "unresolved", "duplicate"), 0);
}
```

Add the compact SQLite helper used by the assertions:

```rust
fn language_file_count(path: &Path, language: &str) -> i64 {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM files WHERE language=?1",
            [language],
            |row| row.get(0),
        )
        .unwrap()
}

fn stored_file_language_and_context(path: &Path, source_path: &str) -> (String, String) {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT language, parse_context FROM files WHERE path=?1",
            [source_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}
```

Run the test; expect the cross-file import/re-export assertions to fail.

- [ ] Make each script file node advertise every key from `module_aliases`. Because `src/collision.js` and `src/collision/index.ts` both advertise `script:module:src/collision`, the existing `Candidate::Ambiguous` path in `index::resolve` must suppress the edge without special-case code.

- [ ] Parse side-effect and default ESM imports with a static string source. Emit a module-file import for the former and an exported-value import plus module-scope local binding for the latter.

- [ ] Parse named and aliased imports, including statement-level and specifier-level `type` modifiers. Emit the imported value/type key selected by the syntax.

- [ ] Parse namespace imports. Emit their direct module import edge and retain the normalized module stem so `namespace.member` can target that module's exported-value key.

If the same local import name is declared more than once, mark the binding ambiguous. A type-only import occupies its lexical name but exposes no runtime call candidate, which is why `misuseType → Config` remains absent.

- [ ] Add exported value/type keys directly to declarations exported in place. For `export default`, add the `default` key and use the stable node name `default` only when the declaration itself is anonymous.

- [ ] Resolve local export lists against unique local top-level definitions. Add every value/type key actually owned by the definition, while `export type` adds only the type key.

- [ ] Emit direct named/default re-exports as `RefKind::Imports` from the source export key with the destination export key as `alias_key`.

- [ ] Emit a namespace re-export from the source module key with the destination exported-value key as `alias_key`. Emit `export * from` as one import reference to the source module with no alias.

Do not follow an alias while building another alias. The existing resolver deliberately supports only one direct re-export hop.

- [ ] Map calls through local definitions and ESM bindings. A static member on an imported class uses that class's source-module method key; a static member on a namespace import uses the member's exported-value key. Candidate keys must be ordered from most specific to least specific, but an ambiguous candidate stops resolution exactly as `reference_target` already requires.

- [ ] Run the focused analyzer test and the ESM e2e test. Expect both to pass, including the cross-extension `"./core.js"` → `src/core.ts` edge, the unique directory-index edge, and the collision's absent edge.

- [ ] Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] Commit:

```text
feat(index): resolve ECMAScript modules
```

---

### Task 5: Resolve literal CommonJS and TypeScript import-equals syntax

**Files:**

- Modify: `src/javascript.rs`
- Modify: `tests/e2e.rs`

**Interfaces:**

Reuse `Import`, `Export`, `relative_module`, and the existing graph keys. Add no CommonJS-specific graph type.

**Steps:**

- [ ] Extend the focused parser test with failing CommonJS and TypeScript assertions:

```rust
let commonjs = parsers
    .parse(
        "src/common.cjs",
        r#"
            const { run: execute } = require("./core");
            const invoke = () => execute();
            module.exports = { invoke };
            exports.direct = execute;
        "#,
    )
    .unwrap();
assert_eq!(commonjs.relative_modules(), ["./core"]);
assert_eq!(
    commonjs.export_names(),
    ["invoke", "direct"]
);

let cts = parsers
    .parse(
        "src/consumer.cts",
        r#"
            import core = require("./common.cjs");
            const consume = () => core.invoke();
            export = consume;
        "#,
    )
    .unwrap();
assert_eq!(cts.relative_modules(), ["./common.cjs"]);
assert_eq!(cts.export_names(), ["default"]);
```

- [ ] Extend `write_script_fixture`:

```rust
const COMMON: &str = r#"
    const { run } = require("./core");
    const invokeCommon = () => run({ value: "cjs" });
    module.exports = { invokeCommon };
"#;
const CONSUMER: &str = r#"
    import common = require("./common.cjs");
    const consume = () => common.invokeCommon();
    export = consume;
"#;

fs::write(root.join("src/common.cjs"), COMMON).unwrap();
fs::write(root.join("src/consumer.cts"), CONSUMER).unwrap();
```

Update the expected initial language counts to:

```rust
assert_eq!(language_file_count(&incremental.path, "javascript"), 5);
assert_eq!(language_file_count(&incremental.path, "typescript"), 7);
```

Add failing assertions for `invokeCommon → run` and `consume → invokeCommon`.

- [ ] Run the focused parser test and the e2e test. Expect the CommonJS export and call assertions to fail.

- [ ] Extend `ImportedName` with `CommonJsModule`. Recognize `require` only when it is the direct callee and has exactly one static string argument accepted by `relative_module`.

- [ ] Parse `require("./module")` as a side-effect import and `const module = require("./module")` as a CommonJS whole-module binding.

- [ ] Parse `const { name, original: local } = require("./module")` into named value bindings. Ignore defaults, rest elements, computed keys, and nested patterns.

- [ ] Parse `import name = require("./module")` as the TypeScript whole-module equivalent.

Ignore dynamic arguments, computed destructuring, chained assignments, and member calls on an unbound `require` result. A lexical binding named `require`, `module`, or `exports` suppresses the corresponding CommonJS special form in that scope.

- [ ] Parse `module.exports = local` and `export = local` as a default-value export.

- [ ] Parse `module.exports.name = local` and `exports.name = local` as named exports.

- [ ] Parse `module.exports = { local, exported: local }` as stable identifier-valued named properties.

An object-literal assignment publishes its stable named properties, not a fabricated default-definition node. Add export keys directly when the right-hand local definition is unique. When the right-hand identifier is an imported binding, emit one alias reference to its resolved value key. Ignore unresolved identifiers, computed property names, spreads, methods, getters/setters, and non-identifier values.

- [ ] Map a bare call on a CommonJS whole-module binding to its default export key, and map its static member calls plus destructured binding calls to the same named `script:export-value` keys used by ESM. This is what permits `consumer.cts` to call an export from `common.cjs` without a compatibility layer.

- [ ] Run:

```bash
cargo test javascript::tests::analyzes_javascript_typescript_and_tsx -- --exact
cargo test --test e2e javascript_typescript_index_search_view_and_incremental_changes_over_mcp -- --exact
```

Expect both to pass.

- [ ] Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] Commit:

```text
feat(index): resolve CommonJS modules
```

---

### Task 6: Emit conventional test callbacks and JSX component calls

**Files:**

- Modify: `src/javascript.rs`
- Modify: `tests/e2e.rs`

**Interfaces:**

```rust
fn is_test_path(path: &str) -> bool;
fn test_callee(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<&'static str>;
fn jsx_target(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<CallTarget>;
```

The helpers remain private. Test callbacks become ordinary `NodeKind::Test` nodes, so the existing resolver automatically converts their resolved `RefKind::Calls` records into `EdgeKind::TestCalls`.

**Steps:**

- [ ] Extend the single focused analyzer test with failing test-path, callback, fallback-title, and JSX assertions:

```rust
for path in [
    "tests/core.test.ts",
    "tests/core.spec.tsx",
    "src/__tests__/core.js",
] {
    assert!(is_test_path(path), "{path}");
}
for path in ["src/core.ts", "src/contest.ts", "src/test-helper.ts"] {
    assert!(!is_test_path(path), "{path}");
}

let tests = parsers
    .parse(
        "tests/core.test.ts",
        "test.only(\"runs\", () => run());\n\
         describe(\"nested\", () => {\n\
           it.skip(\"renders\", () => Panel());\n\
         });\n\
         test(title, () => fallback());\n",
    )
    .unwrap();
assert_eq!(tests.test_names(), ["runs", "renders", "test@5"]);

let ordinary = parsers
    .parse("src/core.ts", r#"test("not a test", () => run());"#)
    .unwrap();
assert!(ordinary.test_names().is_empty());

let jsx = parsers
    .parse(
        "src/panel.tsx",
        r#"const Panel = () => <><Widget /><UI.Widget /><div /></>;"#,
    )
    .unwrap();
assert_eq!(jsx.jsx_component_names(), ["Widget", "UI.Widget"]);
```

- [ ] Run `cargo test javascript::tests::analyzes_javascript_typescript_and_tsx -- --exact`. Expect the new test-path/callback and JSX filtering assertions to fail.

- [ ] Implement `is_test_path` by checking slash-separated components for `__tests__`, or the filename before its supported script suffix for a final `.test`/`.spec` segment. Do not accept substring matches such as `contest.ts`.

- [ ] During parse, recognize only:

```text
test(title, callback)
test.only(title, callback)
test.skip(title, callback)
it(title, callback)
it.only(title, callback)
it.skip(title, callback)
```

Require the callback to be an arrow or function expression. Use a static string/template-with-no-substitution title; otherwise name the node `test@LINE`. The callback body is the test node's ownership range. Do not create a Test node for `describe`, but continue walking its callback so nested `test`/`it` calls are captured.

- [ ] Deduplicate the test call expression from ordinary call extraction. Calls inside the callback belong to the Test node; calls outside a definition remain owned by the file node.

- [ ] For JSX, accept an uppercase identifier or an uppercase namespace/member root. Map:

```text
<Widget />      -> Identifier("Widget")
<UI.Widget />   -> Member { object: "UI", property: "Widget" }
<div />         -> ignored
```

Do not emit a second call for a matching closing element. The query captures opening/self-closing elements only.

- [ ] Extend the e2e fixture with `tests/core.test.ts`:

```rust
const SCRIPT_TESTS: &str = r#"
    import { execute } from "../src/bridge";
    import { Service } from "../src/core";
    import { Panel } from "../src/ui";
    import { future } from "../src/future";

    test.only("runs", () => execute?.({ value: "test" }));
    describe("nested", () => {
        it.skip("constructs", () => new Service());
    });
    test("static factory", () => Service.create());
    test("renders", () => Panel());
    test("future", () => future());
"#;

fs::create_dir_all(root.join("tests")).unwrap();
fs::write(root.join("tests/core.test.ts"), SCRIPT_TESTS).unwrap();
```

Update the stored counts and assert every persisted language/context pair:

```rust
assert_eq!(language_file_count(&incremental.path, "javascript"), 5);
assert_eq!(language_file_count(&incremental.path, "typescript"), 8);
for (path, language, context) in [
    ("src/bridge.js", "javascript", "javascript"),
    ("src/widget.jsx", "javascript", "javascript"),
    ("src/entry.mjs", "javascript", "javascript"),
    ("src/common.cjs", "javascript", "javascript"),
    ("src/core.ts", "typescript", "typescript"),
    ("src/types.d.ts", "typescript", "typescript"),
    ("src/modern.mts", "typescript", "typescript"),
    ("src/consumer.cts", "typescript", "typescript"),
    ("src/ui.tsx", "typescript", "tsx"),
] {
    assert_eq!(
        stored_file_language_and_context(&incremental.path, path),
        (language.to_owned(), context.to_owned()),
        "{path}"
    );
}
```

Add these failing edge assertions:

```rust
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "runs",
        "src/core.ts",
        "run",
        "TEST_CALLS",
    ),
    1
);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "static factory",
        "src/core.ts",
        "create",
        "TEST_CALLS",
    ),
    1
);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "constructs",
        "src/core.ts",
        "Service",
        "TEST_CALLS",
    ),
    1
);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "renders",
        "src/ui.tsx",
        "Panel",
        "TEST_CALLS",
    ),
    1
);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "src/ui.tsx",
        "Panel",
        "src/widget.jsx",
        "Widget",
        "CALLS",
    ),
    1
);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "src/ui.tsx",
        "Panel",
        "src/widget.jsx",
        "DefaultWidget",
        "CALLS",
    ),
    1
);
assert_eq!(named_edge_count(&incremental.path, "Panel", "div"), 0);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "future",
        "src/future.ts",
        "future",
        "TEST_CALLS",
    ),
    0
);
```

The final assertion proves the missing `src/future.ts` target remains unresolved before Task 7.

- [ ] Run the focused analyzer test and the e2e test. Expect both to pass.

- [ ] Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] Commit:

```text
feat(index): add JavaScript tests and JSX calls
```

---

### Task 7: Prove incremental parity and MCP graph behavior

**Files:**

- Modify: `tests/e2e.rs`

**Interfaces:**

No production interface. Add only test helpers.

**Steps:**

- [ ] Add a feature-local fresh-oracle helper. The oracle fixture is a normal test repository created by `init_git`, so deleting only its fixture-local `.git/graphr` directory is safe and guarantees every oracle build starts without a prior graph:

```rust
fn assert_script_graph_matches_fresh(incremental: &Path, oracle: &Path) {
    index_repository(incremental);
    let oracle_cache = oracle.join(".git/graphr");
    if oracle_cache.exists() {
        fs::remove_dir_all(&oracle_cache).unwrap();
    }
    index_repository(oracle);
    assert_eq!(semantic_graph(incremental), semantic_graph(oracle));
}
```

- [ ] Add the path-sensitive support-count helper:

```rust
fn named_edge_support_count(
    path: &Path,
    source_path: &str,
    source: &str,
    target_path: &str,
    target: &str,
    kind: &str,
) -> i64 {
    Connection::open(graph_path(path))
        .unwrap()
        .query_row(
            "SELECT edge.support_count FROM edges edge
               JOIN nodes source ON source.id=edge.source_id
               JOIN files source_file ON source_file.id=source.file_id
               JOIN nodes target ON target.id=edge.target_id
               JOIN files target_file ON target_file.id=target.file_id
              WHERE source_file.path=?1 AND source.name=?2
                AND target_file.path=?3 AND target.name=?4 AND edge.kind=?5",
            [source_path, source, target_path, target, kind],
            |row| row.get(0),
        )
        .unwrap()
}
```

- [ ] Add an edited core fixture:

```rust
const EDITED_CORE: &str = r#"
    import type { Config } from "./types.js";
    function helper(config: Config) { return config.value; }
    export { helper as exposedHelper };
    export function run(config: Config) {
        helper(config);
        return helper(config);
    }
    export class Service {
        static create() { return new Service(); }
        dispatch(config: Config) { return this.finish(config); }
        finish(config: Config) { return run(config); }
    }
    export function makeService() { return Service.create(); }
    export function misuseType() { return Config(); }
    export function shadow(run: () => void) { run(); }
"#;
```

- [ ] Extend the e2e test with an edit and compare the incrementally rebuilt graph to the oracle:

```rust
for root in [&incremental.path, &oracle.path] {
    fs::write(root.join("src/core.ts"), EDITED_CORE).unwrap();
}
assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
```

Assert `run → helper` has support count `2`:

```rust
assert_eq!(
    named_edge_support_count(
        &incremental.path,
        "src/core.ts",
        "run",
        "src/core.ts",
        "helper",
        "CALLS",
    ),
    2
);
```

- [ ] Add a previously missing file in both repositories and prove the unresolved test edge appears:

```rust
for root in [&incremental.path, &oracle.path] {
    fs::write(
        root.join("src/future.ts"),
        "export function future() { return undefined; }\n",
    )
    .unwrap();
}
assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "future",
        "src/future.ts",
        "future",
        "TEST_CALLS",
    ),
    1
);
```

- [ ] Rename `src/future.ts` to `src/moved.ts` in both repositories, compare, then rename it back and compare again:

```rust
for root in [&incremental.path, &oracle.path] {
    fs::rename(root.join("src/future.ts"), root.join("src/moved.ts")).unwrap();
}
assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "future",
        "src/moved.ts",
        "future",
        "TEST_CALLS",
    ),
    0
);

for root in [&incremental.path, &oracle.path] {
    fs::rename(root.join("src/moved.ts"), root.join("src/future.ts")).unwrap();
}
assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "future",
        "src/future.ts",
        "future",
        "TEST_CALLS",
    ),
    1
);
```

- [ ] Delete and re-create `src/future.ts` in both repositories:

```rust
for root in [&incremental.path, &oracle.path] {
    fs::remove_file(root.join("src/future.ts")).unwrap();
}
assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "future",
        "src/future.ts",
        "future",
        "TEST_CALLS",
    ),
    0
);

for root in [&incremental.path, &oracle.path] {
    fs::write(
        root.join("src/future.ts"),
        "export function future() { return undefined; }\n",
    )
    .unwrap();
}
assert_script_graph_matches_fresh(&incremental.path, &oracle.path);
assert_eq!(
    named_edge_kind_count(
        &incremental.path,
        "tests/core.test.ts",
        "future",
        "src/future.ts",
        "future",
        "TEST_CALLS",
    ),
    1
);
```

These are ordinary worktree operations; do not add analyzer-specific incremental code unless an assertion exposes a real defect in the shared pipeline.

- [ ] Exercise the MCP server at the end of the same test:

```rust
let mut client = Client::start(&incremental.path);
let search = client.search("Service", Some("type"));
let search_text = response_text(&search);
let node_ref = search_text.split_whitespace().next().unwrap();
let view = client.view(node_ref, 2, 30);
assert!(view.contains("dispatch"), "{view}");
assert!(view.contains("finish"), "{view}");
let changes = client.changes(1, 50, None);
assert!(changes.contains("future"), "{changes}");
assert!(changes.contains("run"), "{changes}");
client.close();
```

- [ ] Run the e2e test. Expect every mutation to match the oracle and search/view/changes to expose script symbols.

- [ ] Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] Commit:

```text
test: verify script incremental and MCP behavior
```

---

### Task 8: Publish the supported-language contract and run final verification

**Files:**

- Modify: `src/mcp.rs`
- Modify: `README.md`
- Modify: `Cargo.toml`
- Modify: `.agents/skills/graphr-review/SKILL.md`

**Interfaces:**

No new interface. This task updates existing user-facing guidance and package metadata.

**Steps:**

- [ ] Add a failing assertion to `mcp::tests::tool_and_server_guidance_requires_the_explicit_snapshot_workflow`:

```rust
assert!(instructions.contains(
    "Rust, Python, JavaScript/JSX, and TypeScript/TSX"
));
```

Run that test and expect it to fail because the language sentence is absent.

- [ ] Update the server instructions with one compact language sentence. Keep all existing operational guidance and tool descriptions unchanged.

- [ ] Update `README.md`:

  - opening description and feature list name Rust, Python, JavaScript/JSX, and TypeScript/TSX;
  - supported file list names all nine script extensions;
  - graph semantics describe definitions, ESM/CommonJS imports, direct re-exports, resolvable calls, conventional tests, and JSX component calls;
  - limitations explicitly state relative repository-local resolution only, no package/tsconfig resolution, no type checker, and ambiguous module aliases produce no edge;
  - incremental indexing and rename detection state the new languages use the existing pipeline.

- [ ] Update `Cargo.toml` metadata:

```toml
description = "Fast, compact Rust, Python, JavaScript, and TypeScript code graphs for AI code review over MCP stdio"
keywords = ["mcp", "rust", "python", "javascript", "typescript"]
```

- [ ] Update `.agents/skills/graphr-review/SKILL.md` so its supported-language sentence matches the MCP guidance. Change no review workflow steps.

- [ ] Run the complete focused feature set:

```bash
cargo test javascript::
cargo test git::
cargo test mcp::tests::tool_and_server_guidance_requires_the_explicit_snapshot_workflow -- --exact
cargo test --test e2e javascript_typescript_index_search_view_and_incremental_changes_over_mcp -- --exact
```

- [ ] Run the required repository checks from a clean command invocation and inspect every exit status:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
git diff --check
```

- [ ] Inspect the remaining uncommitted scope before the final commit:

```bash
git status --short
git diff --stat
```

Expect only the four Task 8 files.

- [ ] Commit:

```text
docs: document JavaScript and TypeScript support
```

- [ ] Inspect the whole feature branch, including earlier commits:

```bash
git diff --check main...HEAD
git diff --stat main...HEAD
git diff main...HEAD -- src/store.rs
git status --short
```

Expect no `src/store.rs` output, no schema/version change beyond `GRAPH_ANALYZER_VERSION = 2`, no unplanned dependency, and an empty status before handing the branch back.

---

## Final Acceptance Checklist

- [ ] All nine extensions are source files, never artifact fallbacks.
- [ ] Stored language and parse-context values are exact and deterministic.
- [ ] Rust and Python tests remain unchanged and pass.
- [ ] JavaScript/TypeScript definitions, parent links, signatures, imports, direct re-exports, local/cross-file calls, tests, and JSX component calls are queryable.
- [ ] Type-only declarations cannot resolve runtime calls.
- [ ] Bare packages, aliases, unsafe specifiers, dynamic CommonJS, export-star closure, lowercase JSX, and ambiguous modules remain unresolved.
- [ ] Edit, add, rename, delete, and re-add produce the same semantic graph as the oracle.
- [ ] Search, view, and changes work over MCP without a protocol or schema change.
- [ ] Required formatting, lint, test, locked-release-build, and diff checks pass.
