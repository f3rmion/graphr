#![allow(dead_code)]

use std::collections::HashSet;

use tree_sitter::{
    Language as TreeSitterLanguage, Node, Parser, Query, QueryCursor, StreamingIterator,
};

use crate::git::{Language, Source};
use crate::store::{Graph, NodeInput, NodeKind, RefInput, RefKind};

const ECMASCRIPT_QUERY: &str = include_str!("../queries/ecmascript.scm");
const TYPESCRIPT_QUERY: &str = include_str!("../queries/typescript.scm");
const JSX_QUERY: &str = include_str!("../queries/jsx.scm");
const SIGNATURE_LIMIT: usize = 200;
const QUALIFIED_PATH_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScriptDialect {
    JavaScript,
    TypeScript,
    Tsx,
}

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

    const fn language(self) -> Language {
        match self {
            Self::JavaScript => Language::JavaScript,
            Self::TypeScript | Self::Tsx => Language::TypeScript,
        }
    }
}

pub(crate) fn parse_context(path: &str) -> Option<&'static str> {
    ScriptDialect::for_path(path).map(ScriptDialect::parse_context)
}

fn query_source(dialect: ScriptDialect) -> String {
    match dialect {
        ScriptDialect::JavaScript => format!("{ECMASCRIPT_QUERY}\n{JSX_QUERY}"),
        ScriptDialect::TypeScript => format!("{ECMASCRIPT_QUERY}\n{TYPESCRIPT_QUERY}"),
        ScriptDialect::Tsx => format!("{ECMASCRIPT_QUERY}\n{TYPESCRIPT_QUERY}\n{JSX_QUERY}"),
    }
}

struct ScriptParser {
    parser: Parser,
    query: Query,
    cursor: QueryCursor,
}

impl ScriptParser {
    fn new(dialect: ScriptDialect) -> Result<Self, String> {
        let language: TreeSitterLanguage = match dialect {
            ScriptDialect::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            ScriptDialect::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ScriptDialect::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        };
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|error| format!("{} parser: {error}", dialect.parse_context()))?;
        let query = Query::new(&language, &query_source(dialect))
            .map_err(|error| format!("{} query: {error}", dialect.parse_context()))?;
        for name in required_captures(dialect) {
            if query.capture_index_for_name(name).is_none() {
                return Err(format!(
                    "{} query is missing @{name}",
                    dialect.parse_context()
                ));
            }
        }
        Ok(Self {
            parser,
            query,
            cursor: QueryCursor::new(),
        })
    }

    fn parse(
        &mut self,
        dialect: ScriptDialect,
        path: &str,
        source: &str,
    ) -> Result<ParsedFile, String> {
        let tree = self.parser.parse(source, None).ok_or_else(|| {
            format!(
                "{} parser did not return a syntax tree",
                dialect.parse_context()
            )
        })?;
        let mut captured = Vec::new();
        let mut matches = self
            .cursor
            .captures(&self.query, tree.root_node(), source.as_bytes());
        while let Some((query_match, capture_index)) = matches.next() {
            let capture = query_match.captures[*capture_index];
            captured.push(CapturedNode {
                node: capture.node,
                name: self.query.capture_names()[capture.index as usize].to_owned(),
            });
        }
        drop(matches);
        if self.cursor.did_exceed_match_limit() {
            return Err(format!(
                "{} query exceeded Tree-sitter's match limit",
                dialect.parse_context()
            ));
        }
        captured.sort_unstable_by(|left, right| {
            (left.node.start_byte(), left.node.end_byte(), &left.name).cmp(&(
                right.node.start_byte(),
                right.node.end_byte(),
                &right.name,
            ))
        });
        captured.dedup_by(|left, right| {
            (
                left.node.start_byte(),
                left.node.end_byte(),
                left.name.as_str(),
            ) == (
                right.node.start_byte(),
                right.node.end_byte(),
                right.name.as_str(),
            )
        });

        let mut parsed = ParsedFile::default();
        for capture in &captured {
            match capture.name.as_str() {
                "definition" | "typescript_definition" => {
                    if let Some(definition) = definition(capture.node, path, source) {
                        parsed.definitions.push(definition);
                    }
                }
                "module" | "typescript_module" => collect_module(capture.node, source, &mut parsed),
                "binding" => collect_binding(capture.node, source, &mut parsed.bindings),
                _ => {}
            }
        }
        parsed
            .definitions
            .sort_unstable_by_key(|definition| definition.range.start);
        for child in 0..parsed.definitions.len() {
            parsed.definitions[child].parent = parsed
                .definitions
                .iter()
                .enumerate()
                .filter(|(parent, definition)| {
                    *parent != child
                        && definition.structure.start <= parsed.definitions[child].structure.start
                        && parsed.definitions[child].structure.end <= definition.structure.end
                })
                .min_by_key(|(_, definition)| definition.structure.end - definition.structure.start)
                .map(|(parent, _)| parent);
        }
        collect_definition_bindings(&parsed.definitions, &mut parsed.bindings);
        parsed.bindings.sort_unstable_by(|left, right| {
            (left.byte, left.range.start, &left.name).cmp(&(
                right.byte,
                right.range.start,
                &right.name,
            ))
        });
        parsed.bindings.dedup_by(|left, right| {
            left.name == right.name
                && left.byte == right.byte
                && left.range.start == right.range.start
                && left.range.end == right.range.end
        });
        for capture in &captured {
            if capture.name == "call"
                && let Some(call) = call(capture.node, source)
            {
                parsed.calls.push(call);
            }
            if capture.name == "jsx"
                && let Some(call) = jsx_call(capture.node, source)
            {
                parsed.calls.push(call);
            }
        }
        parsed.calls.sort_unstable_by_key(|call| call.byte);
        for call in &mut parsed.calls {
            call.source = parsed
                .definitions
                .iter()
                .enumerate()
                .filter(|(_, definition)| {
                    definition.body.start <= call.byte && call.byte < definition.body.end
                })
                .min_by_key(|(_, definition)| definition.body.end - definition.body.start)
                .map(|(index, _)| index);
        }
        Ok(parsed)
    }
}

fn required_captures(dialect: ScriptDialect) -> &'static [&'static str] {
    match dialect {
        ScriptDialect::JavaScript => &["definition", "module", "binding", "call", "jsx"],
        ScriptDialect::TypeScript => &[
            "definition",
            "module",
            "binding",
            "call",
            "typescript_definition",
            "typescript_module",
        ],
        ScriptDialect::Tsx => &[
            "definition",
            "module",
            "binding",
            "call",
            "typescript_definition",
            "typescript_module",
            "jsx",
        ],
    }
}

struct CapturedNode<'tree> {
    node: Node<'tree>,
    name: String,
}

#[derive(Default)]
pub(crate) struct ScriptParsers {
    javascript: Option<ScriptParser>,
    typescript: Option<ScriptParser>,
    tsx: Option<ScriptParser>,
}

impl ScriptParsers {
    fn parse(&mut self, path: &str, source: &str) -> Result<ParsedFile, String> {
        let dialect =
            ScriptDialect::for_path(path).ok_or_else(|| "unsupported script path".to_owned())?;
        let parser = match dialect {
            ScriptDialect::JavaScript => self.javascript.get_or_insert(ScriptParser::new(dialect)?),
            ScriptDialect::TypeScript => self.typescript.get_or_insert(ScriptParser::new(dialect)?),
            ScriptDialect::Tsx => self.tsx.get_or_insert(ScriptParser::new(dialect)?),
        };
        parser.parse(dialect, path, source)
    }
}

#[derive(Clone, Copy)]
struct ByteRange {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
enum DefinitionKind {
    Type { runtime_value: bool },
    Function,
    Method,
    Test,
}

struct Definition {
    kind: DefinitionKind,
    name: String,
    parent: Option<usize>,
    line_start: usize,
    line_end: usize,
    signature: String,
    body: ByteRange,
    range: ByteRange,
    structure: ByteRange,
    binding: Option<ByteRange>,
}

struct ModuleStatement {
    module: String,
}

struct LexicalBinding {
    name: String,
    range: ByteRange,
    byte: usize,
}

enum CallTarget {
    Identifier(String),
    Member { object: String, property: String },
    ThisMethod(String),
    Jsx(String),
}

struct Call {
    source: Option<usize>,
    byte: usize,
    target: CallTarget,
    line: usize,
}

#[derive(Default)]
struct ParsedFile {
    definitions: Vec<Definition>,
    modules: Vec<ModuleStatement>,
    bindings: Vec<LexicalBinding>,
    calls: Vec<Call>,
    exports: Vec<String>,
}

pub(crate) fn add_file(
    graph: &mut Graph,
    source: &Source,
    language: Language,
    parsers: &mut ScriptParsers,
) -> Result<(), String> {
    let dialect = ScriptDialect::for_path(&source.path)
        .ok_or_else(|| "unsupported script path".to_owned())?;
    if dialect.language() != language {
        return Err(format!(
            "{} path requires stored language {}",
            dialect.parse_context(),
            dialect.language().as_str()
        ));
    }
    let stem = module_stem(&source.path)?;
    let parsed = parsers.parse(&source.path, &source.text)?;
    let file_key = identity(language, &source.path, "file", &source.path, 0, 0);
    graph.nodes.push(NodeInput {
        key: file_key.clone(),
        file_key: source.path.clone(),
        kind: NodeKind::File,
        name: source.path.clone(),
        qualified_name: file_key.clone(),
        parent_key: None,
        owner_key: None,
        line_start: 1,
        line_end: to_u32(source.text.lines().count().max(1))?,
        signature: String::new(),
        keys: vec![module_key(&stem)],
    });

    let mut paths = Vec::with_capacity(parsed.definitions.len());
    let mut node_keys = Vec::with_capacity(parsed.definitions.len());
    for (ordinal, definition) in parsed.definitions.iter().enumerate() {
        let parent = definition
            .parent
            .and_then(|parent| paths.get(parent))
            .map_or("", String::as_str);
        let path = checked_lexical_path(&stem, parent, &definition.name)?;
        let kind = node_kind(definition.kind);
        node_keys.push(identity(
            language,
            &source.path,
            kind_name(kind),
            &path,
            definition.line_start,
            ordinal,
        ));
        paths.push(path);
    }

    let exports = parsed
        .exports
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    for (local, definition) in parsed.definitions.iter().enumerate() {
        let key = node_keys[local].clone();
        graph.nodes.push(NodeInput {
            key: key.clone(),
            file_key: source.path.clone(),
            kind: node_kind(definition.kind),
            name: definition.name.clone(),
            qualified_name: key,
            parent_key: Some(
                definition
                    .parent
                    .and_then(|parent| node_keys.get(parent).cloned())
                    .unwrap_or_else(|| file_key.clone()),
            ),
            owner_key: None,
            line_start: to_u32(definition.line_start)?,
            line_end: to_u32(definition.line_end)?,
            signature: definition.signature.clone(),
            keys: definition_keys(
                definition,
                &paths[local],
                &stem,
                definition.parent.is_none() && exports.contains(definition.name.as_str()),
            ),
        });
    }

    for call in &parsed.calls {
        let keys = call_keys(call, &parsed, &paths, &stem);
        if keys.is_empty() {
            continue;
        }
        graph.refs.push(RefInput {
            source_key: call
                .source
                .and_then(|source| node_keys.get(source).cloned())
                .unwrap_or_else(|| file_key.clone()),
            kind: RefKind::Calls,
            line: to_u32(call.line)?,
            keys,
            alias_key: None,
            resolved_target_key: None,
        });
    }
    Ok(())
}

fn definition_keys(definition: &Definition, path: &str, stem: &str, exported: bool) -> Vec<String> {
    let mut keys = match definition.kind {
        DefinitionKind::Type {
            runtime_value: true,
        } => vec![local_type_key(stem, path), local_value_key(stem, path)],
        DefinitionKind::Type {
            runtime_value: false,
        } => vec![local_type_key(stem, path)],
        DefinitionKind::Method => definition.parent.map_or_else(
            || vec![local_value_key(stem, path)],
            |_| {
                let (owner, _) = path.rsplit_once("::").unwrap_or(("", path));
                vec![method_key(stem, owner, &definition.name)]
            },
        ),
        DefinitionKind::Function => vec![local_value_key(stem, path)],
        DefinitionKind::Test => Vec::new(),
    };
    if exported {
        match definition.kind {
            DefinitionKind::Type {
                runtime_value: true,
            } => {
                keys.push(export_type_key(stem, &definition.name));
                keys.push(export_value_key(stem, &definition.name));
            }
            DefinitionKind::Type {
                runtime_value: false,
            } => keys.push(export_type_key(stem, &definition.name)),
            DefinitionKind::Function => {
                keys.push(export_value_key(stem, &definition.name));
            }
            DefinitionKind::Method | DefinitionKind::Test => {}
        }
    }
    keys
}

fn call_keys(call: &Call, parsed: &ParsedFile, paths: &[String], stem: &str) -> Vec<String> {
    let key = match &call.target {
        CallTarget::Identifier(name) => visible_value_definition(call, name, parsed)
            .map(|target| local_value_key(stem, paths.get(target).map_or(name, String::as_str))),
        CallTarget::ThisMethod(name) => containing_class(call.source, &parsed.definitions)
            .and_then(|owner| paths.get(owner))
            .map(|owner| method_key(stem, owner, name)),
        CallTarget::Member { object, property } => visible_value_definition(call, object, parsed)
            .filter(|target| {
                matches!(
                    parsed.definitions[*target].kind,
                    DefinitionKind::Type {
                        runtime_value: true
                    }
                )
            })
            .and_then(|owner| paths.get(owner))
            .map(|owner| method_key(stem, owner, property)),
        CallTarget::Jsx(name) => {
            let root = name.split('.').next().unwrap_or(name);
            visible_value_definition(call, root, parsed)
                .map(|target| local_value_key(stem, paths.get(target).map_or(root, String::as_str)))
        }
    };
    key.into_iter().collect()
}

fn visible_value_definition(call: &Call, name: &str, parsed: &ParsedFile) -> Option<usize> {
    let bindings = parsed
        .bindings
        .iter()
        .filter(|binding| {
            binding.name == name
                && binding.range.start <= call.byte
                && call.byte < binding.range.end
        })
        .collect::<Vec<_>>();
    if let Some(width) = bindings
        .iter()
        .map(|binding| binding.range.end - binding.range.start)
        .min()
    {
        let mut target = None;
        for binding in bindings
            .into_iter()
            .filter(|binding| binding.range.end - binding.range.start == width)
        {
            let candidate =
                parsed
                    .definitions
                    .iter()
                    .enumerate()
                    .find_map(|(index, definition)| {
                        (definition.name == name
                            && definition.range.start == binding.byte
                            && is_runtime_value(definition.kind))
                        .then_some(index)
                    })?;
            if target
                .replace(candidate)
                .is_some_and(|current| current != candidate)
            {
                return None;
            }
        }
        return target;
    }

    let mut owner = call.source;
    loop {
        let mut candidates = parsed
            .definitions
            .iter()
            .enumerate()
            .filter(|(_, definition)| {
                definition.parent == owner
                    && definition.name == name
                    && is_runtime_value(definition.kind)
            });
        let candidate = candidates.next().map(|(index, _)| index);
        if candidate.is_some() && candidates.next().is_none() {
            return candidate;
        }
        if candidate.is_some() {
            return None;
        }
        owner = owner.and_then(|scope| parsed.definitions.get(scope)?.parent);
        if owner.is_none() {
            let mut top_level = parsed
                .definitions
                .iter()
                .enumerate()
                .filter(|(_, definition)| {
                    definition.parent.is_none()
                        && definition.name == name
                        && is_runtime_value(definition.kind)
                });
            let candidate = top_level.next().map(|(index, _)| index);
            return candidate.filter(|_| top_level.next().is_none());
        }
    }
}

fn is_runtime_value(kind: DefinitionKind) -> bool {
    matches!(
        kind,
        DefinitionKind::Function
            | DefinitionKind::Type {
                runtime_value: true
            }
    )
}

fn containing_class(source: Option<usize>, definitions: &[Definition]) -> Option<usize> {
    let mut current = source;
    while let Some(index) = current {
        let definition = definitions.get(index)?;
        if matches!(
            definition.kind,
            DefinitionKind::Type {
                runtime_value: true
            }
        ) {
            return Some(index);
        }
        current = definition.parent;
    }
    None
}

fn module_stem(path: &str) -> Result<String, String> {
    let stem = path.strip_suffix(".d.ts").or_else(|| {
        ScriptDialect::for_path(path).and_then(|_| path.rsplit_once('.').map(|(stem, _)| stem))
    });
    let stem = stem
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| "script module path is invalid".to_owned())?;
    if stem.len() > QUALIFIED_PATH_LIMIT {
        return Err("Script qualified path exceeds 1024 bytes".to_owned());
    }
    Ok(stem.to_owned())
}

fn checked_lexical_path(stem: &str, parent: &str, name: &str) -> Result<String, String> {
    let path = join_path(parent, name);
    checked_join_path(stem, &path)?;
    Ok(path)
}

fn checked_join_path(left: &str, right: &str) -> Result<String, String> {
    let separator = usize::from(!left.is_empty() && !right.is_empty()) * 2;
    left.len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(right.len()))
        .filter(|length| *length <= QUALIFIED_PATH_LIMIT)
        .ok_or_else(|| "Script qualified path exceeds 1024 bytes".to_owned())?;
    Ok(join_path(left, right))
}

fn join_path(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.to_owned()
    } else if right.is_empty() {
        left.to_owned()
    } else {
        format!("{left}::{right}")
    }
}

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

fn method_key(stem: &str, owner_path: &str, name: &str) -> String {
    format!("script:method:{stem}::{owner_path}::{name}")
}

fn node_kind(kind: DefinitionKind) -> NodeKind {
    match kind {
        DefinitionKind::Type { .. } => NodeKind::Type,
        DefinitionKind::Function | DefinitionKind::Method => NodeKind::Function,
        DefinitionKind::Test => NodeKind::Test,
    }
}

fn kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "file",
        NodeKind::Type => "type",
        NodeKind::Function => "function",
        NodeKind::Test => "test",
    }
}

fn to_u32(value: usize) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| "source line exceeds supported range".to_owned())
}

fn definition(node: Node<'_>, path: &str, source: &str) -> Option<Definition> {
    let (kind, name, start, structure_start) = if let Some(name) = test_name(node, path, source) {
        (
            DefinitionKind::Test,
            name,
            definition_start(node),
            node.start_byte(),
        )
    } else {
        match node.kind() {
            "class_declaration" | "abstract_class_declaration" => (
                DefinitionKind::Type {
                    runtime_value: true,
                },
                field_text(node, "name", source)
                    .map(str::to_owned)
                    .or_else(|| default_export(node).then(|| "default".to_owned()))?,
                definition_start(node),
                node.start_byte(),
            ),
            "interface_declaration" | "type_alias_declaration" => (
                DefinitionKind::Type {
                    runtime_value: false,
                },
                field_text(node, "name", source)?.to_owned(),
                definition_start(node),
                node.start_byte(),
            ),
            "enum_declaration" | "internal_module" | "module" => (
                DefinitionKind::Type {
                    runtime_value: true,
                },
                field_text(node, "name", source)?.to_owned(),
                definition_start(node),
                node.start_byte(),
            ),
            "function_declaration" | "generator_function_declaration" | "function_signature" => (
                DefinitionKind::Function,
                field_text(node, "name", source)
                    .map(str::to_owned)
                    .or_else(|| default_export(node).then(|| "default".to_owned()))?,
                definition_start(node),
                node.start_byte(),
            ),
            "method_definition" | "method_signature" | "abstract_method_signature" => (
                DefinitionKind::Method,
                field_text(node, "name", source)?.to_owned(),
                definition_start(node),
                node.start_byte(),
            ),
            "function_expression" | "generator_function" | "arrow_function" | "class" => {
                stable_initializer(node, source)?
            }
            _ => return None,
        }
    };
    let body = node.child_by_field_name("body");
    let signature_end = body.map_or(node.end_byte(), |body| body.start_byte());
    let body = body.unwrap_or(node);
    let binding = definition_binding(node, &kind, &name, source);
    Some(Definition {
        binding,
        kind,
        name,
        parent: None,
        line_start: line_at(source, start),
        line_end: line_end(node),
        signature: signature(start, signature_end, source),
        body: ByteRange {
            start: body.start_byte(),
            end: body.end_byte(),
        },
        range: ByteRange {
            start,
            end: node.end_byte(),
        },
        structure: ByteRange {
            start: structure_start,
            end: node.end_byte(),
        },
    })
}

fn stable_initializer(
    node: Node<'_>,
    source: &str,
) -> Option<(DefinitionKind, String, usize, usize)> {
    let parent = node.parent()?;
    if parent.kind() == "variable_declarator"
        && parent
            .child_by_field_name("value")
            .is_some_and(|value| value.id() == node.id())
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.kind() == "identifier")
    {
        return Some((
            definition_kind(node),
            field_text(parent, "name", source)?.to_owned(),
            declarator_start(parent),
            parent.start_byte(),
        ));
    }
    if matches!(
        parent.kind(),
        "field_definition" | "public_field_definition"
    ) && parent
        .child_by_field_name("value")
        .is_some_and(|value| value.id() == node.id())
        && direct_field_name(parent)
            .is_some_and(|name| name.kind() == "property_identifier" || name.kind() == "identifier")
    {
        return Some((
            DefinitionKind::Method,
            text(direct_field_name(parent)?, source).to_owned(),
            definition_start(parent),
            parent.start_byte(),
        ));
    }
    if parent.kind() == "assignment_expression"
        && parent
            .child_by_field_name("right")
            .is_some_and(|right| right.id() == node.id())
        && parent
            .child_by_field_name("left")
            .is_some_and(|left| left.kind() == "identifier")
    {
        return Some((
            definition_kind(node),
            text(parent.child_by_field_name("left")?, source).to_owned(),
            definition_start(parent),
            parent.start_byte(),
        ));
    }
    if default_export(node) {
        return Some((
            definition_kind(node),
            "default".to_owned(),
            definition_start(node),
            node.start_byte(),
        ));
    }
    None
}

fn definition_kind(node: Node<'_>) -> DefinitionKind {
    if node.kind() == "class" {
        DefinitionKind::Type {
            runtime_value: true,
        }
    } else {
        DefinitionKind::Function
    }
}

fn direct_field_name(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("property")
        .or_else(|| node.child_by_field_name("name"))
}

fn definition_start(node: Node<'_>) -> usize {
    let mut start = node.start_byte();
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "export_statement" {
            start = parent.start_byte();
            current = parent;
            continue;
        }
        let mut cursor = parent.walk();
        for sibling in parent.named_children(&mut cursor) {
            if sibling.end_byte() <= start && sibling.kind() == "decorator" {
                start = sibling.start_byte();
            }
        }
        break;
    }
    start
}

fn declarator_start(node: Node<'_>) -> usize {
    let declaration = node.parent().filter(|parent| {
        matches!(
            parent.kind(),
            "lexical_declaration" | "variable_declaration"
        )
    });
    let Some(declaration) = declaration else {
        return definition_start(node);
    };
    let mut cursor = declaration.walk();
    let first = declaration
        .named_children(&mut cursor)
        .find(|child| child.kind() == "variable_declarator");
    if first.is_some_and(|first| first.id() == node.id()) {
        definition_start(declaration)
    } else {
        node.start_byte()
    }
}

fn default_export(node: Node<'_>) -> bool {
    export_statement(node).is_some_and(has_default_token)
}

fn export_statement(node: Node<'_>) -> Option<Node<'_>> {
    node.parent()
        .filter(|parent| parent.kind() == "export_statement")
}

fn has_default_token(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == "default")
}

fn definition_binding(
    node: Node<'_>,
    kind: &DefinitionKind,
    name: &str,
    source: &str,
) -> Option<ByteRange> {
    if name == "default" || matches!(kind, DefinitionKind::Method | DefinitionKind::Test) {
        return None;
    }
    matches!(
        node.kind(),
        "class_declaration"
            | "abstract_class_declaration"
            | "function_declaration"
            | "generator_function_declaration"
            | "enum_declaration"
            | "internal_module"
            | "module"
    )
    .then(|| definition_scope(node, source))
}

fn collect_module(node: Node<'_>, source: &str, parsed: &mut ParsedFile) {
    match node.kind() {
        "import_statement" => {
            collect_relative_module(node, source, &mut parsed.modules);
            collect_import_bindings(node, source, &mut parsed.bindings);
        }
        "export_statement" => {
            collect_relative_module(node, source, &mut parsed.modules);
            collect_exports(node, source, &mut parsed.exports);
        }
        "assignment_expression" => collect_assignment_export(node, source, &mut parsed.exports),
        _ => {}
    }
}

fn collect_binding(node: Node<'_>, source: &str, bindings: &mut Vec<LexicalBinding>) {
    let target = match node.kind() {
        "formal_parameters" => node,
        "variable_declarator" => node.child_by_field_name("name").unwrap_or(node),
        "catch_clause" => node.child_by_field_name("parameter").unwrap_or(node),
        _ => return,
    };
    let range = match node.kind() {
        "formal_parameters" => function_scope(node, source),
        "variable_declarator" if is_var(node) => function_scope(node, source),
        "variable_declarator" => block_scope(node, source),
        "catch_clause" => node
            .child_by_field_name("body")
            .map(range)
            .unwrap_or_else(|| module_scope(source)),
        _ => module_scope(source),
    };
    for identifier in identifiers(target) {
        bindings.push(LexicalBinding {
            name: text(identifier, source).to_owned(),
            range,
            byte: identifier.start_byte(),
        });
    }
}

fn collect_relative_module(node: Node<'_>, source: &str, modules: &mut Vec<ModuleStatement>) {
    let Some(module) = node
        .child_by_field_name("source")
        .map(|source_node| text(source_node, source).trim_matches(['\'', '"']))
    else {
        return;
    };
    if module.starts_with("./") || module.starts_with("../") {
        modules.push(ModuleStatement {
            module: module.to_owned(),
        });
    }
}

fn collect_import_bindings(node: Node<'_>, source: &str, bindings: &mut Vec<LexicalBinding>) {
    let module = module_scope(source);
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "import_specifier" => {
                let name = node
                    .child_by_field_name("alias")
                    .or_else(|| node.child_by_field_name("name"));
                if let Some(name) = name.filter(|name| name.kind() == "identifier") {
                    bindings.push(LexicalBinding {
                        name: text(name, source).to_owned(),
                        range: module,
                        byte: name.start_byte(),
                    });
                }
                continue;
            }
            "namespace_import" => {
                if let Some(name) = identifiers(node).into_iter().next() {
                    bindings.push(LexicalBinding {
                        name: text(name, source).to_owned(),
                        range: module,
                        byte: name.start_byte(),
                    });
                }
                continue;
            }
            "identifier"
                if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "import_clause") =>
            {
                bindings.push(LexicalBinding {
                    name: text(node, source).to_owned(),
                    range: module,
                    byte: node.start_byte(),
                });
                continue;
            }
            "string" | "comment" => continue,
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
}

fn collect_exports(node: Node<'_>, source: &str, exports: &mut Vec<String>) {
    if has_default_token(node) {
        add_export(exports, "default");
        return;
    }
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "export_specifier" => {
                if let Some(name) = node
                    .child_by_field_name("alias")
                    .or_else(|| node.child_by_field_name("name"))
                {
                    add_export(exports, text(name, source));
                }
                continue;
            }
            "class_declaration"
            | "function_declaration"
            | "generator_function_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "internal_module"
            | "module" => {
                if let Some(name) = field_text(node, "name", source) {
                    add_export(exports, name);
                }
                continue;
            }
            "variable_declarator" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .filter(|name| name.kind() == "identifier")
                {
                    add_export(exports, text(name, source));
                }
                continue;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
}

fn collect_assignment_export(node: Node<'_>, source: &str, exports: &mut Vec<String>) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    if left.kind() == "member_expression" {
        let object = left.child_by_field_name("object");
        let property = left.child_by_field_name("property");
        if object.is_some_and(|object| text(object, source) == "exports") {
            if let Some(property) =
                property.filter(|property| property.kind() == "property_identifier")
            {
                add_export(exports, text(property, source));
            }
        } else if object.is_some_and(|object| text(object, source) == "module.exports")
            && let Some(property) =
                property.filter(|property| property.kind() == "property_identifier")
        {
            add_export(exports, text(property, source));
        }
    } else if text(left, source) == "module.exports" {
        add_export(exports, "default");
    }
}

fn add_export(exports: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !exports.iter().any(|export| export == name) {
        exports.push(name.to_owned());
    }
}

fn collect_definition_bindings(definitions: &[Definition], bindings: &mut Vec<LexicalBinding>) {
    for definition in definitions {
        let Some(parent) = definition.binding else {
            continue;
        };
        bindings.push(LexicalBinding {
            name: definition.name.clone(),
            range: parent,
            byte: definition.range.start,
        });
        if parent.start != definition.body.start || parent.end != definition.body.end {
            bindings.push(LexicalBinding {
                name: definition.name.clone(),
                range: definition.body,
                byte: definition.range.start,
            });
        }
    }
}

fn is_var(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|declaration| declaration.kind() == "variable_declaration")
}

fn function_scope(node: Node<'_>, source: &str) -> ByteRange {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "function_expression"
                | "generator_function"
                | "arrow_function"
                | "method_definition"
        ) {
            return parent
                .child_by_field_name("body")
                .map(range)
                .unwrap_or_else(|| range(parent));
        }
        current = parent;
    }
    module_scope(source)
}

fn block_scope(node: Node<'_>, source: &str) -> ByteRange {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(parent.kind(), "statement_block" | "class_body" | "program") {
            return range(parent);
        }
        current = parent;
    }
    module_scope(source)
}

fn definition_scope(node: Node<'_>, source: &str) -> ByteRange {
    let parent = node.parent();
    if parent.is_some_and(|parent| parent.kind() == "variable_declarator" && is_var(parent)) {
        function_scope(node, source)
    } else {
        block_scope(node, source)
    }
}

fn module_scope(source: &str) -> ByteRange {
    ByteRange {
        start: 0,
        end: source.len(),
    }
}

fn range(node: Node<'_>) -> ByteRange {
    ByteRange {
        start: node.start_byte(),
        end: node.end_byte(),
    }
}

fn identifiers(node: Node<'_>) -> Vec<Node<'_>> {
    let mut identifiers = Vec::new();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier" {
            identifiers.push(node);
            continue;
        }
        if matches!(node.kind(), "type_annotation" | "comment") {
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    identifiers
}

fn test_name(node: Node<'_>, path: &str, source: &str) -> Option<String> {
    if !is_test_path(path) {
        return None;
    }
    let mut current = node;
    let call = loop {
        let parent = current.parent()?;
        if parent.kind() == "call_expression" {
            break parent;
        }
        if !matches!(parent.kind(), "arguments" | "parenthesized_expression") {
            return None;
        }
        current = parent;
    };
    let function = call.child_by_field_name("function")?;
    if !test_callee(function, source) {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let title = arguments
        .named_children(&mut cursor)
        .find(|child| child.id() != node.id())?;
    let title = text(title, source);
    if (title.starts_with(['\'', '"']) && title.ends_with(['\'', '"']))
        || (title.starts_with('`') && title.ends_with('`') && !title.contains("${"))
    {
        return Some(title[1..title.len() - 1].to_owned());
    }
    Some(format!("test@{}", line_start(call)))
}

fn is_test_path(path: &str) -> bool {
    path.split('/').any(|component| component == "__tests__")
        || [".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts"]
            .iter()
            .any(|extension| {
                path.strip_suffix(extension)
                    .is_some_and(|stem| stem.ends_with(".test") || stem.ends_with(".spec"))
            })
}

fn test_callee(node: Node<'_>, source: &str) -> bool {
    if matches!(text(node, source), "test" | "it") {
        return true;
    }
    node.kind() == "member_expression"
        && node
            .child_by_field_name("object")
            .is_some_and(|object| matches!(text(object, source), "test" | "it"))
        && node
            .child_by_field_name("property")
            .is_some_and(|property| matches!(text(property, source), "only" | "skip"))
}

fn call(node: Node<'_>, source: &str) -> Option<Call> {
    let target = if node.kind() == "new_expression" {
        node.child_by_field_name("constructor")?
    } else {
        node.child_by_field_name("function")?
    };
    let target = match target.kind() {
        "identifier" => CallTarget::Identifier(text(target, source).to_owned()),
        "member_expression" => {
            let object = target.child_by_field_name("object")?;
            let property = target.child_by_field_name("property")?;
            if object.kind() == "this" {
                CallTarget::ThisMethod(text(property, source).to_owned())
            } else if object.kind() == "identifier" && property.kind() == "property_identifier" {
                CallTarget::Member {
                    object: text(object, source).to_owned(),
                    property: text(property, source).to_owned(),
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(Call {
        source: None,
        byte: node.start_byte(),
        target,
        line: line_start(node),
    })
}

fn jsx_call(node: Node<'_>, source: &str) -> Option<Call> {
    if node.kind() == "identifier" || node.kind() == "member_expression" {
        let name = text(node, source);
        if name.chars().next().is_some_and(char::is_uppercase) {
            return Some(Call {
                source: None,
                byte: node.start_byte(),
                target: CallTarget::Jsx(name.to_owned()),
                line: line_start(node),
            });
        }
    }
    None
}

fn signature(start: usize, end: usize, source: &str) -> String {
    let text = source.get(start..end).unwrap_or_default().trim_end();
    let mut end = text.len().min(SIGNATURE_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn field_text<'source>(node: Node<'_>, field: &str, source: &'source str) -> Option<&'source str> {
    let value = text(node.child_by_field_name(field)?, source);
    (!value.is_empty()).then_some(value)
}

fn text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    source.get(node.byte_range()).unwrap_or_default()
}

fn line_start(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

fn line_at(source: &str, byte: usize) -> usize {
    source.get(..byte).map_or(1, |prefix| {
        prefix.bytes().filter(|byte| *byte == b'\n').count() + 1
    })
}

fn line_end(node: Node<'_>) -> usize {
    node.end_position().row + 1
}

#[cfg(test)]
impl ParsedFile {
    fn definition_names(&self) -> Vec<&str> {
        self.definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect()
    }

    fn relative_modules(&self) -> Vec<&str> {
        self.modules
            .iter()
            .map(|module| module.module.as_str())
            .collect()
    }

    fn call_names(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter_map(|call| match &call.target {
                CallTarget::Identifier(name) => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    fn export_names(&self) -> Vec<&str> {
        self.exports.iter().map(String::as_str).collect()
    }

    fn test_names(&self) -> Vec<&str> {
        self.definitions
            .iter()
            .filter_map(|definition| {
                matches!(definition.kind, DefinitionKind::Test).then_some(definition.name.as_str())
            })
            .collect()
    }

    fn jsx_component_names(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter_map(|call| match &call.target {
                CallTarget::Jsx(name) => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{Language as StoredLanguage, Source};
    use crate::store::{Graph, NodeKind};

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

        for (path, context) in [
            ("a.js", "javascript"),
            ("a.jsx", "javascript"),
            ("a.mjs", "javascript"),
            ("a.cjs", "javascript"),
            ("a.ts", "typescript"),
            ("a.d.ts", "typescript"),
            ("a.mts", "typescript"),
            ("a.cts", "typescript"),
            ("a.tsx", "tsx"),
        ] {
            assert_eq!(parse_context(path), Some(context), "{path}");
        }

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
            StoredLanguage::TypeScript,
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
        assert!(has_ref(&graph, "run", "script:value:src/service::helper"));
        assert!(!has_ref(&graph, "shadow", "script:value:src/service::run"));
        assert!(
            add_file(
                &mut Graph::default(),
                &source,
                StoredLanguage::JavaScript,
                &mut ScriptParsers::default(),
            )
            .is_err()
        );
    }

    fn has_node(graph: &Graph, kind: NodeKind, name: &str) -> bool {
        graph
            .nodes
            .iter()
            .any(|node| node.kind == kind && node.name == name)
    }

    fn has_ref(graph: &Graph, source: &str, key: &str) -> bool {
        graph.refs.iter().any(|reference| {
            graph
                .nodes
                .iter()
                .any(|node| node.key == reference.source_key && node.name == source)
                && reference.keys.iter().any(|candidate| candidate == key)
        })
    }

    #[test]
    fn retains_script_scopes_exports_tests_stable_initializers_and_jsx_calls() {
        let source = r#"
import main, { value as alias } from "./dep";
export default function () {}
export const stable = () => {};
assigned = () => {};
const [unstable] = [() => {}];
thing.member = () => {};
class Box {
    method = () => {};
    ['computed'] = () => {};
}
function outer(param) {
    var hoisted = () => {};
    { let local = () => {}; local(); }
    try {} catch (caught) { caught(); }
    function nested() { nested(); }
    param(); hoisted();
}
test.only("works", () => stable());
const panel = <View />;
"#;
        let parsed = ScriptParsers::default()
            .parse("src/item.test.tsx", source)
            .unwrap();

        assert_eq!(
            parsed.definition_names(),
            [
                "default", "stable", "assigned", "Box", "method", "outer", "hoisted", "local",
                "nested", "works"
            ]
        );
        assert_eq!(parsed.export_names(), ["default", "stable"]);
        assert_eq!(parsed.test_names(), ["works"]);
        assert_eq!(parsed.jsx_component_names(), ["View"]);
        assert!(binding_contains(&parsed, source, "main", "stable"));
        assert!(binding_contains(&parsed, source, "alias", "stable"));
        assert!(binding_contains(&parsed, source, "param", "param();"));
        assert!(binding_contains(&parsed, source, "hoisted", "hoisted();"));
        assert!(binding_contains(&parsed, source, "local", "local();"));
        assert!(binding_contains(&parsed, source, "caught", "caught();"));
        assert!(binding_contains(&parsed, source, "nested", "nested();"));
        assert!(!binding_contains(&parsed, source, "local", "param();"));
        assert!(!binding_contains(&parsed, source, "caught", "param();"));
        assert!(!parsed.definition_names().contains(&"unstable"));
        assert!(!parsed.definition_names().contains(&"member"));
        assert!(!parsed.definition_names().contains(&"computed"));
    }

    #[test]
    fn selects_every_supported_script_dialect() {
        for (path, dialect) in [
            ("a.js", ScriptDialect::JavaScript),
            ("a.jsx", ScriptDialect::JavaScript),
            ("a.mjs", ScriptDialect::JavaScript),
            ("a.cjs", ScriptDialect::JavaScript),
            ("a.ts", ScriptDialect::TypeScript),
            ("a.d.ts", ScriptDialect::TypeScript),
            ("a.mts", ScriptDialect::TypeScript),
            ("a.cts", ScriptDialect::TypeScript),
            ("a.tsx", ScriptDialect::Tsx),
        ] {
            assert_eq!(ScriptDialect::for_path(path), Some(dialect), "{path}");
        }
    }

    #[test]
    fn keeps_declaration_coverage_structural_defaults_and_runtime_bindings() {
        let source = r#"
export /* legal comment */ default
function () {}
export const helper = () => {};
function run() { run(); }
class Service { method() {} }
enum State { Ready }
namespace API {}
interface Config {}
type Alias = string;
assigned = () => {};
"#;
        let parsed = ScriptParsers::default()
            .parse("src/contracts.ts", source)
            .unwrap();

        assert_eq!(parsed.export_names(), ["default", "helper"]);
        let helper = parsed
            .definitions
            .iter()
            .find(|definition| definition.name == "helper")
            .unwrap();
        assert!(helper.signature.starts_with("export const helper"));
        assert!(source[helper.range.start..].starts_with("export const helper"));
        assert_runtime_binding(&parsed, "run");
        assert_runtime_binding(&parsed, "Service");
        assert_runtime_binding(&parsed, "State");
        assert_runtime_binding(&parsed, "API");
        assert_eq!(binding_count(&parsed, "helper"), 1);
        assert_eq!(binding_count(&parsed, "assigned"), 0);
        assert_eq!(binding_count(&parsed, "method"), 0);
        assert_eq!(binding_count(&parsed, "Config"), 0);
        assert_eq!(binding_count(&parsed, "Alias"), 0);

        assert_eq!(
            ScriptParsers::default()
                .parse("__tests__/root.ts", "test('root', () => {});")
                .unwrap()
                .test_names(),
            ["root"]
        );
        assert!(
            ScriptParsers::default()
                .parse("src/not__tests__/root.ts", "test('root', () => {});")
                .unwrap()
                .test_names()
                .is_empty()
        );
    }

    #[test]
    fn keeps_comma_declarator_definitions_as_siblings() {
        let source = "const a = () => {}, b = () => {};\nexport const c = () => {}, d = () => {};";
        let parsed = ScriptParsers::default()
            .parse("src/siblings.ts", source)
            .unwrap();

        assert_eq!(parsed.definition_names(), ["a", "b", "c", "d"]);
        for definition in &parsed.definitions {
            assert_eq!(definition.parent, None, "{}", definition.name);
        }
        let a = parsed
            .definitions
            .iter()
            .find(|definition| definition.name == "a")
            .unwrap();
        let b = parsed
            .definitions
            .iter()
            .find(|definition| definition.name == "b")
            .unwrap();
        let c = parsed
            .definitions
            .iter()
            .find(|definition| definition.name == "c")
            .unwrap();
        let d = parsed
            .definitions
            .iter()
            .find(|definition| definition.name == "d")
            .unwrap();
        assert!(a.signature.starts_with("const a"));
        assert!(b.signature.starts_with("b ="));
        assert!(!b.signature.contains("a ="));
        assert!(c.signature.starts_with("export const c"));
        assert!(d.signature.starts_with("d ="));
        assert!(!d.signature.contains("c ="));
    }

    fn binding_contains(parsed: &ParsedFile, source: &str, name: &str, text: &str) -> bool {
        let offset = source.find(text).unwrap();
        parsed.bindings.iter().any(|binding| {
            binding.name == name && binding.range.start <= offset && offset < binding.range.end
        })
    }

    fn binding_count(parsed: &ParsedFile, name: &str) -> usize {
        parsed
            .bindings
            .iter()
            .filter(|binding| binding.name == name)
            .count()
    }

    fn assert_runtime_binding(parsed: &ParsedFile, name: &str) {
        assert!(binding_count(parsed, name) >= 2, "{name}");
    }
}
