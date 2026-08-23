use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::git::Source;
use crate::parse::{ParseGap, parser_no_tree_gaps, syntax_gaps};
use crate::store::{
    GapCategory, GapInput, GapReason, Graph, NodeInput, NodeKind, RefInput, RefKind,
    ResolutionState,
};

const PYTHON_QUERY: &str = include_str!("../queries/python.scm");
const PATH_LIMIT: usize = 1024;
const SIGNATURE_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DefinitionKind {
    Type,
    Function,
    Test,
}

struct Definition {
    kind: DefinitionKind,
    name: String,
    parent: Option<usize>,
    line_start: usize,
    line_end: usize,
    signature: String,
}

struct Import {
    source: Option<usize>,
    module: String,
    name: Option<String>,
    alias: Option<String>,
    line: usize,
}

struct Call {
    source: Option<usize>,
    target: String,
    bare: bool,
    line: usize,
}

struct ValueBinding {
    source: Option<usize>,
    name: String,
}

#[derive(Default)]
struct ParsedFile {
    definitions: Vec<Definition>,
    imports: Vec<Import>,
    bindings: Vec<ValueBinding>,
    calls: Vec<Call>,
    parse_gaps: Vec<ParseGap>,
    parser_no_tree: bool,
}

struct Scope {
    end_byte: usize,
    definition: usize,
}

pub struct PythonParser {
    parser: Parser,
    query: Query,
    cursor: QueryCursor,
    captures: Captures,
}

struct Captures {
    type_: u32,
    function: u32,
    import: u32,
    binding: u32,
    call: u32,
}

impl PythonParser {
    pub fn new() -> Result<Self, String> {
        let language = tree_sitter_python::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|error| error.to_string())?;
        let query = Query::new(&language, PYTHON_QUERY).map_err(|error| error.to_string())?;
        let captures = Captures {
            type_: capture(&query, "type")?,
            function: capture(&query, "function")?,
            import: capture(&query, "import")?,
            binding: capture(&query, "binding")?,
            call: capture(&query, "call")?,
        };
        Ok(Self {
            parser,
            query,
            cursor: QueryCursor::new(),
            captures,
        })
    }

    fn parse(&mut self, source: &str) -> Result<ParsedFile, String> {
        let Some(tree) = self.parser.parse(source, None) else {
            return Ok(ParsedFile {
                parse_gaps: parser_no_tree_gaps(source.lines().count().max(1)),
                parser_no_tree: true,
                ..ParsedFile::default()
            });
        };
        let mut parsed = ParsedFile {
            parse_gaps: syntax_gaps(tree.root_node()),
            ..ParsedFile::default()
        };
        let mut scopes = Vec::<Scope>::new();
        let mut captures = self
            .cursor
            .captures(&self.query, tree.root_node(), source.as_bytes());

        while let Some((query_match, capture_index)) = captures.next() {
            let capture = query_match.captures[*capture_index];
            let node = capture.node;
            while scopes
                .last()
                .is_some_and(|scope| scope.end_byte <= node.start_byte())
            {
                scopes.pop();
            }
            let parent = scopes.last().map(|scope| scope.definition);

            if capture.index == self.captures.type_ {
                if let Some(name) = field_text(node, "name", source) {
                    let coverage = definition_coverage(node);
                    let definition = parsed.definitions.len();
                    parsed.definitions.push(Definition {
                        kind: DefinitionKind::Type,
                        name: name.to_owned(),
                        parent,
                        line_start: line_start(coverage),
                        line_end: line_end(coverage),
                        signature: signature(node, source),
                    });
                    scopes.push(Scope {
                        end_byte: node.end_byte(),
                        definition,
                    });
                }
            } else if capture.index == self.captures.function {
                if let Some(name) = field_text(node, "name", source) {
                    let coverage = definition_coverage(node);
                    let kind = if name.starts_with("test_") {
                        DefinitionKind::Test
                    } else {
                        DefinitionKind::Function
                    };
                    let definition = parsed.definitions.len();
                    parsed.definitions.push(Definition {
                        kind,
                        name: name.to_owned(),
                        parent,
                        line_start: line_start(coverage),
                        line_end: line_end(coverage),
                        signature: signature(node, source),
                    });
                    scopes.push(Scope {
                        end_byte: node.end_byte(),
                        definition,
                    });
                }
            } else if capture.index == self.captures.import {
                parse_import(node, source, parent, &mut parsed.imports)?;
            } else if capture.index == self.captures.binding {
                collect_bindings(node, source, parent, &mut parsed.bindings);
            } else if capture.index == self.captures.call
                && let Some(function) = node.child_by_field_name("function")
            {
                parsed.calls.push(Call {
                    source: parent,
                    target: text(function, source).to_owned(),
                    bare: function.kind() == "identifier",
                    line: line_start(node),
                });
            }
        }
        drop(captures);
        if self.cursor.did_exceed_match_limit() {
            return Err("Python query exceeded Tree-sitter's match limit".into());
        }
        Ok(parsed)
    }
}

struct PythonTarget {
    module: String,
    package: String,
    exports: bool,
}

impl PythonTarget {
    fn for_path(path: &str) -> Option<Self> {
        let path = path.strip_suffix(".py")?;
        let path = path.strip_prefix("src/").unwrap_or(path);
        let init = path == "__init__" || path.ends_with("/__init__");
        let module = if init {
            path.strip_suffix("/__init__").unwrap_or_default()
        } else {
            path
        }
        .replace('/', "::");
        let package = if init {
            module.clone()
        } else {
            module
                .rsplit_once("::")
                .map_or_else(String::new, |(package, _)| package.to_owned())
        };
        Some(Self {
            module,
            package,
            exports: init,
        })
    }
}

pub fn add_file(
    graph: &mut Graph,
    source: &Source,
    parser: &mut PythonParser,
) -> Result<(), String> {
    let target = PythonTarget::for_path(&source.path)
        .ok_or_else(|| "Python source path is invalid".to_owned())?;
    let parsed = parser.parse(&source.text)?;
    let mut observed_relation_sites = 0_u32;
    let file_key = identity(&source.path, "file", &source.path, 0, 0);
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
        keys: vec![item_key(&target.module)],
    });
    for gap in &parsed.parse_gaps {
        graph.gaps.push(GapInput {
            file_key: Some(source.path.clone()),
            source_key: None,
            run_key: None,
            path: Some(source.path.clone()),
            line_start: Some(to_u32(gap.line_start)?),
            line_end: Some(to_u32(gap.line_end)?),
            category: GapCategory::Parse,
            reason: if parsed.parser_no_tree {
                GapReason::ParserNoTree
            } else {
                GapReason::ParserError
            },
            target_hint: None,
            occurrences: 1,
            relation_site: false,
        });
    }

    let mut paths = Vec::with_capacity(parsed.definitions.len());
    let mut node_keys = Vec::with_capacity(parsed.definitions.len());
    for (local, definition) in parsed.definitions.iter().enumerate() {
        let parent = definition
            .parent
            .and_then(|parent| paths.get(parent))
            .map_or(target.module.as_str(), String::as_str);
        let path = checked_join(parent, &definition.name)?;
        let kind = node_kind(definition.kind);
        node_keys.push(identity(
            &source.path,
            kind_name(kind),
            &path,
            definition.line_start,
            local,
        ));
        paths.push(path);
    }

    let mut available = HashSet::new();
    for (local, definition) in parsed.definitions.iter().enumerate() {
        let key = node_keys[local].clone();
        let keys = vec![item_key(&paths[local])];
        available.extend(keys.iter().cloned());
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
            keys,
        });
    }

    let mut values = HashMap::<Option<usize>, HashSet<String>>::new();
    for binding in &parsed.bindings {
        values
            .entry(binding.source)
            .or_default()
            .insert(binding.name.clone());
    }
    let bindings = Bindings {
        imports: import_bindings(&parsed, &target)?,
        values,
    };
    for import in &parsed.imports {
        let source_key = import
            .source
            .and_then(|source| node_keys.get(source).cloned())
            .unwrap_or_else(|| file_key.clone());
        let Some(path) = import_path(import, &target)? else {
            graph.gaps.push(GapInput {
                file_key: Some(source.path.clone()),
                source_key: Some(source_key),
                run_key: None,
                path: Some(source.path.clone()),
                line_start: Some(to_u32(import.line)?),
                line_end: Some(to_u32(import.line)?),
                category: GapCategory::Relation,
                reason: GapReason::DynamicOrUnsupportedDispatch,
                target_hint: Some(import.module.clone()),
                occurrences: 1,
                relation_site: true,
            });
            observed_relation_sites += 1;
            continue;
        };
        graph.refs.push(RefInput {
            source_key,
            kind: RefKind::Imports,
            line: to_u32(import.line)?,
            keys: vec![item_key(&path)],
            alias_key: export_key(import, &target),
            resolved_target_key: None,
            resolution: ResolutionState::Pending,
        });
        observed_relation_sites += 1;
    }
    for call in &parsed.calls {
        let source_key = call
            .source
            .and_then(|source| node_keys.get(source).cloned())
            .unwrap_or_else(|| file_key.clone());
        let keys = if call.bare {
            call_keys(
                call,
                &parsed.definitions,
                &paths,
                &target,
                &bindings,
                &available,
            )
        } else {
            Vec::new()
        };
        if !keys.is_empty() {
            graph.refs.push(RefInput {
                source_key,
                kind: RefKind::Calls,
                line: to_u32(call.line)?,
                keys,
                alias_key: None,
                resolved_target_key: None,
                resolution: ResolutionState::Pending,
            });
        } else {
            graph.gaps.push(GapInput {
                file_key: Some(source.path.clone()),
                source_key: Some(source_key),
                run_key: None,
                path: Some(source.path.clone()),
                line_start: Some(to_u32(call.line)?),
                line_end: Some(to_u32(call.line)?),
                category: GapCategory::Relation,
                reason: GapReason::DynamicOrUnsupportedDispatch,
                target_hint: Some(call.target.clone()),
                occurrences: 1,
                relation_site: true,
            });
        }
        observed_relation_sites += 1;
    }
    graph
        .files
        .iter_mut()
        .find(|file| file.path == source.path)
        .ok_or_else(|| "Python graph file is missing".to_owned())?
        .observed_relation_sites = observed_relation_sites;
    Ok(())
}

#[derive(Clone)]
enum Binding {
    Unique(String),
    Ambiguous,
}

type ImportBindings = HashMap<Option<usize>, HashMap<String, Binding>>;

struct Bindings {
    imports: ImportBindings,
    values: HashMap<Option<usize>, HashSet<String>>,
}

fn import_bindings(parsed: &ParsedFile, target: &PythonTarget) -> Result<ImportBindings, String> {
    let mut bindings = HashMap::<Option<usize>, HashMap<String, Binding>>::new();
    for import in &parsed.imports {
        let Some(path) = import_path(import, target)? else {
            continue;
        };
        let (name, bound) = if let Some(alias) = &import.alias {
            (alias.clone(), path)
        } else if let Some(name) = &import.name {
            (last_component(name).to_owned(), path)
        } else {
            let first = path.split("::").next().unwrap_or_default().to_owned();
            (first.clone(), first)
        };
        if name.is_empty() || bound.is_empty() {
            continue;
        }
        bindings
            .entry(import.source)
            .or_default()
            .entry(name)
            .and_modify(|binding| {
                if !matches!((&*binding, &bound), (Binding::Unique(current), next) if current == next)
                {
                    *binding = Binding::Ambiguous;
                }
            })
            .or_insert(Binding::Unique(bound));
    }
    Ok(bindings)
}

fn call_keys(
    call: &Call,
    definitions: &[Definition],
    paths: &[String],
    target: &PythonTarget,
    bindings: &Bindings,
    available: &HashSet<String>,
) -> Vec<String> {
    // ponytail: bare calls cover the measured Python corpus; add attribute
    // resolution only when static receiver evidence can keep it precise.
    let name = call.target.as_str();
    for scope in call
        .source
        .into_iter()
        .flat_map(|source| lexical_scopes(source, definitions))
    {
        if bindings
            .values
            .get(&Some(scope))
            .is_some_and(|bindings| bindings.contains(name))
        {
            return Vec::new();
        }
        let candidate = paths
            .get(scope)
            .map(|path| item_key(&join(path, name)))
            .filter(|key| available.contains(key));
        let binding = bindings
            .imports
            .get(&Some(scope))
            .and_then(|imports| imports.get(name));
        if let Some(candidate) = candidate {
            return if binding.is_none() {
                vec![candidate]
            } else {
                Vec::new()
            };
        }
        if let Some(binding) = binding {
            let Binding::Unique(path) = binding else {
                return Vec::new();
            };
            return vec![item_key(path)];
        }
    }
    if bindings
        .values
        .get(&None)
        .is_some_and(|bindings| bindings.contains(name))
    {
        return Vec::new();
    }
    let candidate = {
        let key = item_key(&join(&target.module, name));
        available.contains(&key).then_some(key)
    };
    let binding = bindings
        .imports
        .get(&None)
        .and_then(|imports| imports.get(name));
    if let Some(candidate) = candidate {
        return if binding.is_none() {
            vec![candidate]
        } else {
            Vec::new()
        };
    }
    if let Some(binding) = binding {
        let Binding::Unique(path) = binding else {
            return Vec::new();
        };
        return vec![item_key(path)];
    }
    vec![item_key(&join(&target.module, name))]
}

fn lexical_scopes(source: usize, definitions: &[Definition]) -> impl Iterator<Item = usize> + '_ {
    std::iter::successors(Some((source, true)), move |(scope, _)| {
        definitions
            .get(*scope)
            .and_then(|definition| definition.parent)
            .map(|parent| (parent, false))
    })
    .filter_map(move |(scope, source)| {
        let definition = definitions.get(scope)?;
        (source || definition.kind != DefinitionKind::Type).then_some(scope)
    })
}

fn export_key(import: &Import, target: &PythonTarget) -> Option<String> {
    // ponytail: explicit package exports cover the measured corpus; add
    // ordinary-module reexports only when a real repository needs them.
    if !target.exports
        || import.source.is_some()
        || (import.name.is_none() && import.alias.is_none())
    {
        return None;
    }
    let binding = import
        .alias
        .as_deref()
        .or_else(|| import.name.as_deref().map(last_component))?;
    Some(item_key(&join(&target.module, binding)))
}

fn import_path(import: &Import, target: &PythonTarget) -> Result<Option<String>, String> {
    let Some(module) = normalize_module(&import.module, target)? else {
        return Ok(None);
    };
    match import.name.as_deref() {
        Some(name) => checked_join(&module, name).map(Some),
        None => Ok(Some(module)),
    }
}

fn normalize_module(raw: &str, target: &PythonTarget) -> Result<Option<String>, String> {
    let raw = raw.trim();
    let dots = raw.bytes().take_while(|byte| *byte == b'.').count();
    let suffix = raw[dots..].replace('.', "::");
    if dots == 0 {
        return Ok((!suffix.is_empty()).then_some(suffix));
    }
    let mut base = target
        .package
        .split("::")
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    for _ in 1..dots {
        if base.pop().is_none() {
            return Ok(None);
        }
    }
    let base = base.join("::");
    if suffix.is_empty() {
        Ok((!base.is_empty()).then_some(base))
    } else {
        checked_join(&base, &suffix).map(Some)
    }
}

fn parse_import(
    node: Node<'_>,
    source: &str,
    owner: Option<usize>,
    imports: &mut Vec<Import>,
) -> Result<(), String> {
    match node.kind() {
        "import_statement" => {
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let (module, alias) = import_part(name, source)?;
                imports.push(Import {
                    source: owner,
                    module,
                    name: None,
                    alias,
                    line: line_start(node),
                });
            }
        }
        "import_from_statement" => {
            let Some(module) = field_text(node, "module_name", source) else {
                return Ok(());
            };
            if module.len() > PATH_LIMIT {
                return Err("Python import path exceeds 1024 bytes".into());
            }
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let (name, alias) = import_part(name, source)?;
                imports.push(Import {
                    source: owner,
                    module: module.to_owned(),
                    name: Some(name),
                    alias,
                    line: line_start(node),
                });
            }
        }
        _ => {}
    }
    Ok(())
}

fn import_part(node: Node<'_>, source: &str) -> Result<(String, Option<String>), String> {
    let (name, alias) = if node.kind() == "aliased_import" {
        (
            field_text(node, "name", source).unwrap_or_default(),
            field_text(node, "alias", source).map(str::to_owned),
        )
    } else {
        (text(node, source), None)
    };
    if name.len() > PATH_LIMIT {
        return Err("Python import path exceeds 1024 bytes".into());
    }
    if alias.as_ref().is_some_and(|alias| alias.len() > PATH_LIMIT) {
        return Err("Python import alias exceeds 1024 bytes".into());
    }
    Ok((name.replace('.', "::"), alias))
}

fn collect_bindings(
    node: Node<'_>,
    source: &str,
    owner: Option<usize>,
    bindings: &mut Vec<ValueBinding>,
) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier" {
            bindings.push(ValueBinding {
                source: owner,
                name: text(node, source).to_owned(),
            });
            continue;
        }
        if matches!(node.kind(), "attribute" | "subscript") {
            continue;
        }
        let type_child = node.child_by_field_name("type").map(|child| child.id());
        let value_child = node.child_by_field_name("value").map(|child| child.id());
        let mut cursor = node.walk();
        pending.extend(
            node.named_children(&mut cursor)
                .filter(|child| Some(child.id()) != type_child && Some(child.id()) != value_child),
        );
    }
}

fn capture(query: &Query, name: &str) -> Result<u32, String> {
    query
        .capture_index_for_name(name)
        .ok_or_else(|| format!("Python query is missing @{name}"))
}

fn signature(node: Node<'_>, source: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    let signature = source
        .get(node.start_byte()..end)
        .unwrap_or_default()
        .trim_end();
    let mut end = signature.len().min(SIGNATURE_LIMIT);
    while !signature.is_char_boundary(end) {
        end -= 1;
    }
    signature[..end].to_owned()
}

fn definition_coverage(node: Node<'_>) -> Node<'_> {
    node.parent()
        .filter(|parent| parent.kind() == "decorated_definition")
        .unwrap_or(node)
}

fn field_text<'source>(node: Node<'_>, field: &str, source: &'source str) -> Option<&'source str> {
    let value = text(node.child_by_field_name(field)?, source);
    (!value.is_empty()).then_some(value)
}

fn text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    source.get(node.byte_range()).unwrap_or_default()
}

fn identity(path: &str, kind: &str, scope: &str, line: usize, ordinal: usize) -> String {
    format!(
        "python:{}#{path}:{}#{kind}:{}#{scope}:{line}:{ordinal}",
        path.len(),
        kind.len(),
        scope.len()
    )
}

fn item_key(path: &str) -> String {
    format!("python:item:{path}")
}

fn checked_join(left: &str, right: &str) -> Result<String, String> {
    let separator = usize::from(!left.is_empty() && !right.is_empty()) * 2;
    left.len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(right.len()))
        .filter(|length| *length <= PATH_LIMIT)
        .ok_or_else(|| "Python qualified path exceeds 1024 bytes".to_owned())?;
    Ok(join(left, right))
}

fn join(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.to_owned()
    } else if right.is_empty() {
        left.to_owned()
    } else {
        format!("{left}::{right}")
    }
}

fn last_component(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn node_kind(kind: DefinitionKind) -> NodeKind {
    match kind {
        DefinitionKind::Type => NodeKind::Type,
        DefinitionKind::Function => NodeKind::Function,
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

fn line_start(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

fn line_end(node: Node<'_>) -> usize {
    node.end_position().row + 1
}

fn to_u32(value: usize) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| "source line exceeds supported range".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_every_python_relation_site() {
        let source = Source {
            path: "pkg/app.py".into(),
            text: r#"import os
from .worker import run
from ...outside import impossible

def dispatch(value):
    run()
    value.method()
    value[0]()
"#
            .into(),
        };
        let mut graph = Graph::default();
        graph.files.push(crate::store::FileInput {
            path: source.path.clone(),
            language: crate::git::Language::Python,
            git_oid: None,
            content_hash: [0; 32],
            parse_context: "python".into(),
            byte_size: source.text.len() as u64,
            replace: true,
            observed_relation_sites: 0,
        });

        add_file(&mut graph, &source, &mut PythonParser::new().unwrap()).unwrap();

        assert_eq!(
            graph.refs.len(),
            3,
            "two classifiable imports and one bare call"
        );
        assert_eq!(
            graph
                .gaps
                .iter()
                .filter(|gap| gap.relation_site)
                .map(|gap| gap.reason)
                .collect::<Vec<_>>(),
            [
                crate::store::GapReason::DynamicOrUnsupportedDispatch,
                crate::store::GapReason::DynamicOrUnsupportedDispatch,
                crate::store::GapReason::DynamicOrUnsupportedDispatch,
            ]
        );
        assert_eq!(graph.files[0].observed_relation_sites, 6);
    }

    #[test]
    fn reports_parse_gaps_for_malformed_python() {
        let parsed = PythonParser::new()
            .unwrap()
            .parse("def first(:\n  pass\ndef second():\n  value =\n")
            .unwrap();

        assert!(!parsed.parse_gaps.is_empty());
        assert!(parsed.parse_gaps.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn extracts_python_graph_evidence_from_incomplete_source() {
        let parsed = PythonParser::new()
            .unwrap()
            .parse(
                r#"from .worker import run as execute

class Service:
    def dispatch(self):
        self.flush()

    def flush(self):
        execute()

def helper(callback):
    callback()

def test_dispatch():
    Service().dispatch()

def broken(
"#,
            )
            .unwrap();

        assert_eq!(
            parsed
                .definitions
                .iter()
                .map(|definition| (definition.kind, definition.name.as_str(), definition.parent))
                .collect::<Vec<_>>(),
            [
                (DefinitionKind::Type, "Service", None),
                (DefinitionKind::Function, "dispatch", Some(0)),
                (DefinitionKind::Function, "flush", Some(0)),
                (DefinitionKind::Function, "helper", None),
                (DefinitionKind::Test, "test_dispatch", None),
            ]
        );
        assert_eq!(parsed.imports[0].module, ".worker");
        assert_eq!(parsed.imports[0].name.as_deref(), Some("run"));
        assert_eq!(parsed.imports[0].alias.as_deref(), Some("execute"));
        assert!(parsed.calls.iter().any(|call| call.target == "execute"));
        assert!(
            parsed
                .bindings
                .iter()
                .any(|binding| binding.source == Some(3) && binding.name == "callback")
        );
    }
}
