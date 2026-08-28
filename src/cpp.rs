use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::git::Source;
use crate::parse::syntax_gaps;
use crate::store::{
    GapCategory, GapInput, GapReason, Graph, ModeledSiteInput, ModeledSiteKind, NodeInput,
    NodeKind, RefInput, RefKind, ResolutionState, TraitImplementationInput,
};

const CPP_QUERY: &str = include_str!("../queries/cpp.scm");
const CLASS_LOOKUP_LIMIT: usize = 4096;
const PATH_LIMIT: usize = 1024;
const SIGNATURE_LIMIT: usize = 200;

pub(crate) struct CppParser {
    parser: Parser,
    query: Query,
    cursor: QueryCursor,
    captures: Captures,
}

struct Captures {
    namespace: u32,
    type_: u32,
    function: u32,
    include: u32,
    call: u32,
    binding: u32,
    member: u32,
}

#[derive(Clone)]
struct Scope {
    end_byte: usize,
    container: String,
    parent_key: String,
    call_source: Option<String>,
    type_path: Option<String>,
}

struct Binding {
    name: String,
    start_byte: usize,
    end_byte: usize,
}

#[derive(Default)]
struct ClassLookup {
    members: HashSet<String>,
    bases: HashMap<String, Vec<String>>,
    incomplete_types: HashSet<String>,
    cache: HashMap<(String, String), MemberLookup>,
}

impl CppParser {
    pub(crate) fn new() -> Result<Self, String> {
        let language = tree_sitter_cpp::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|error| error.to_string())?;
        let query = Query::new(&language, CPP_QUERY).map_err(|error| error.to_string())?;
        let captures = Captures {
            namespace: capture(&query, "namespace")?,
            type_: capture(&query, "type")?,
            function: capture(&query, "function")?,
            include: capture(&query, "include")?,
            call: capture(&query, "call")?,
            binding: capture(&query, "binding")?,
            member: capture(&query, "member")?,
        };
        Ok(Self {
            parser,
            query,
            cursor: QueryCursor::new(),
            captures,
        })
    }
}

pub(crate) fn add_file(
    graph: &mut Graph,
    source: &Source,
    parser: &mut CppParser,
) -> Result<(), String> {
    let file_key = identity(&source.path, "file", &source.path, 1, 0);
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
        keys: include_aliases(&source.path),
    });
    let Some(tree) = parser.parser.parse(&source.text, None) else {
        add_parse_gap(
            graph,
            source,
            1,
            source.text.lines().count().max(1),
            GapReason::ParserNoTree,
        )?;
        return Ok(());
    };
    for gap in syntax_gaps(tree.root_node()) {
        add_parse_gap(
            graph,
            source,
            gap.line_start,
            gap.line_end,
            GapReason::ParserError,
        )?;
    }
    let mut class_lookup = collect_class_lookup(parser, tree.root_node(), source)?;

    let mut scopes = Vec::<Scope>::new();
    let mut bindings = Vec::<Binding>::new();
    let mut anonymous_scopes = HashSet::new();
    let mut observed_relation_sites = 0_u32;
    let mut ordinal = 0;
    let mut captures =
        parser
            .cursor
            .captures(&parser.query, tree.root_node(), source.text.as_bytes());
    while let Some((query_match, capture_index)) = captures.next() {
        let capture = query_match.captures[*capture_index];
        let node = capture.node;
        while scopes
            .last()
            .is_some_and(|scope| scope.end_byte <= node.start_byte())
        {
            scopes.pop();
        }
        let current = scopes.last().cloned().unwrap_or_else(|| Scope {
            end_byte: tree.root_node().end_byte(),
            container: String::new(),
            parent_key: file_key.clone(),
            call_source: None,
            type_path: None,
        });

        if capture.index == parser.captures.namespace {
            let name = if let Some(name) = node.child_by_field_name("name") {
                normalize_qualified(text(name, &source.text))
            } else {
                anonymous_scopes.insert(current.container.clone());
                anonymous_namespace(&source.path)
            };
            if name.is_empty() {
                continue;
            }
            scopes.push(Scope {
                end_byte: node.end_byte(),
                container: checked_join(&current.container, &name)?,
                ..current
            });
        } else if capture.index == parser.captures.type_ {
            let (Some(name), Some(_)) = (
                node.child_by_field_name("name"),
                node.child_by_field_name("body"),
            ) else {
                continue;
            };
            let name = normalize_qualified(text(name, &source.text));
            if name.is_empty() {
                continue;
            }
            let path = checked_join(&current.container, &name)?;
            let key = identity(&source.path, "type", &path, line_start(node), ordinal);
            ordinal += 1;
            graph.nodes.push(NodeInput {
                key: key.clone(),
                file_key: source.path.clone(),
                kind: NodeKind::Type,
                name,
                qualified_name: key.clone(),
                parent_key: Some(current.parent_key.clone()),
                owner_key: None,
                line_start: to_u32(line_start(node))?,
                line_end: to_u32(line_end(node))?,
                signature: signature(node, &source.text),
                keys: vec![item_key(&path)],
            });
            add_base_relations(graph, source, node, &current.container, &path)?;
            scopes.push(Scope {
                end_byte: node.end_byte(),
                container: path.clone(),
                parent_key: key,
                type_path: Some(path),
                ..current
            });
        } else if capture.index == parser.captures.function {
            let Some((raw_name, declarator)) = function_name(node, &source.text) else {
                continue;
            };
            let test_name = google_test_name(
                node,
                &raw_name,
                declarator,
                current.type_path.is_some(),
                &source.text,
            );
            let name = test_name
                .as_deref()
                .unwrap_or_else(|| function_leaf(&raw_name));
            let path = if test_name.is_some() {
                checked_join(&current.container, name)?
            } else {
                definition_path(&current.container, &raw_name)?
            };
            let kind = if test_name.is_some() {
                NodeKind::Test
            } else {
                NodeKind::Function
            };
            let key = identity(
                &source.path,
                kind_name(kind),
                &path,
                line_start(node),
                ordinal,
            );
            ordinal += 1;
            let explicit_owner = raw_name
                .strip_suffix(name)
                .and_then(|prefix| prefix.strip_suffix("::"))
                .filter(|owner| !owner.is_empty())
                .map(|owner| definition_path(&current.container, owner))
                .transpose()?;
            let owner = current.type_path.clone().or(explicit_owner.clone());
            graph.nodes.push(NodeInput {
                key: key.clone(),
                file_key: source.path.clone(),
                kind,
                name: name.to_owned(),
                qualified_name: key.clone(),
                parent_key: Some(current.parent_key.clone()),
                owner_key: owner.as_deref().map(item_key),
                line_start: to_u32(line_start(node))?,
                line_end: to_u32(line_end(node))?,
                signature: signature(node, &source.text),
                keys: vec![item_key(&path)],
            });
            if let Some(test_name) = test_name {
                graph.modeled_sites.push(ModeledSiteInput {
                    file_key: source.path.clone(),
                    source_key: Some(key.clone()),
                    kind: ModeledSiteKind::TestRegistration,
                    line_start: to_u32(line_start(node))?,
                    line_end: to_u32(line_end(node))?,
                    target_hint: Some(test_name.to_owned()),
                    parse_context: Some("cpp".into()),
                });
                observed_relation_sites += 1;
            }
            scopes.push(Scope {
                end_byte: node.end_byte(),
                container: explicit_owner.unwrap_or(current.container),
                call_source: Some(key),
                type_path: owner,
                ..current
            });
        } else if capture.index == parser.captures.include {
            let Some(path) = node.child_by_field_name("path") else {
                add_relation_gap(
                    graph,
                    source,
                    &file_key,
                    node,
                    GapCategory::Parse,
                    GapReason::ParserError,
                    String::new(),
                )?;
                observed_relation_sites += 1;
                continue;
            };
            if path.kind() != "string_literal" {
                if path.kind() != "call_expression" {
                    add_relation_gap(
                        graph,
                        source,
                        &file_key,
                        node,
                        if path.kind() == "system_lib_string" {
                            GapCategory::Boundary
                        } else {
                            GapCategory::Macro
                        },
                        if path.kind() == "system_lib_string" {
                            GapReason::ExternalDependency
                        } else {
                            GapReason::MacroExpansionUnavailable
                        },
                        bounded_text(text(path, &source.text)),
                    )?;
                    observed_relation_sites += 1;
                }
                continue;
            }
            let Some(raw) = string_content(path, &source.text) else {
                add_relation_gap(
                    graph,
                    source,
                    &file_key,
                    node,
                    GapCategory::Parse,
                    GapReason::ParserError,
                    bounded_text(text(path, &source.text)),
                )?;
                observed_relation_sites += 1;
                continue;
            };
            let keys = include_keys(&source.path, raw);
            if !keys.is_empty() {
                graph.refs.push(RefInput {
                    source_key: file_key.clone(),
                    kind: RefKind::Imports,
                    line: to_u32(line_start(node))?,
                    keys,
                    alias_key: None,
                    resolved_target_key: None,
                    resolution: ResolutionState::Pending,
                });
            } else {
                add_relation_gap(
                    graph,
                    source,
                    &file_key,
                    node,
                    GapCategory::Boundary,
                    GapReason::ExternalDependency,
                    bounded_text(raw),
                )?;
            }
            observed_relation_sites += 1;
        } else if capture.index == parser.captures.binding {
            let Some(name) = declarator_name(node) else {
                continue;
            };
            bindings.push(Binding {
                name: text(name, &source.text).to_owned(),
                start_byte: binding_scope_start(node),
                end_byte: binding_scope_end(node),
            });
        } else if capture.index == parser.captures.call {
            if is_preprocessor_operand(node) {
                let function = node.child_by_field_name("function").unwrap_or(node);
                let source_key = current.call_source.as_ref().unwrap_or(&file_key);
                add_relation_gap(
                    graph,
                    source,
                    source_key,
                    node,
                    GapCategory::Macro,
                    GapReason::MacroExpansionUnavailable,
                    bounded_text(text(function, &source.text)),
                )?;
                observed_relation_sites += 1;
                continue;
            }
            if current.call_source.is_none()
                && let Some((name, body)) = catch_test(node, &source.text)
            {
                let path = checked_join(&current.container, &name)?;
                let key = identity(&source.path, "test", &path, line_start(node), ordinal);
                ordinal += 1;
                graph.nodes.push(NodeInput {
                    key: key.clone(),
                    file_key: source.path.clone(),
                    kind: NodeKind::Test,
                    name: name.clone(),
                    qualified_name: key.clone(),
                    parent_key: Some(current.parent_key.clone()),
                    owner_key: None,
                    line_start: to_u32(line_start(node))?,
                    line_end: to_u32(line_end(body))?,
                    signature: signature(node, &source.text),
                    keys: vec![item_key(&path)],
                });
                graph.modeled_sites.push(ModeledSiteInput {
                    file_key: source.path.clone(),
                    source_key: Some(key.clone()),
                    kind: ModeledSiteKind::TestRegistration,
                    line_start: to_u32(line_start(node))?,
                    line_end: to_u32(line_end(body))?,
                    target_hint: Some(name),
                    parse_context: Some("cpp".into()),
                });
                observed_relation_sites += 1;
                scopes.push(Scope {
                    end_byte: body.end_byte(),
                    call_source: Some(key),
                    ..current
                });
                continue;
            }
            let source_key = current.call_source.as_ref().unwrap_or(&file_key);
            let keys = call_keys(
                node,
                &source.text,
                &source.path,
                &current,
                &bindings,
                &anonymous_scopes,
                &mut class_lookup,
            )?;
            if !keys.is_empty() {
                graph.refs.push(RefInput {
                    source_key: source_key.clone(),
                    kind: RefKind::Calls,
                    line: to_u32(line_start(node))?,
                    keys,
                    alias_key: None,
                    resolved_target_key: None,
                    resolution: ResolutionState::Pending,
                });
            } else {
                let function = node.child_by_field_name("function").unwrap_or(node);
                add_relation_gap(
                    graph,
                    source,
                    source_key,
                    node,
                    GapCategory::Relation,
                    GapReason::DynamicOrUnsupportedDispatch,
                    bounded_text(text(function, &source.text)),
                )?;
            }
            observed_relation_sites += 1;
        }
    }
    drop(captures);
    if parser.cursor.did_exceed_match_limit() {
        return Err("C++ query exceeded Tree-sitter's match limit".into());
    }
    if let Some(file) = graph.files.iter_mut().find(|file| file.path == source.path) {
        file.observed_relation_sites = observed_relation_sites;
    }
    Ok(())
}

fn collect_class_lookup(
    parser: &mut CppParser,
    root: Node<'_>,
    source: &Source,
) -> Result<ClassLookup, String> {
    let mut lookup = ClassLookup::default();
    let mut captures = parser
        .cursor
        .captures(&parser.query, root, source.text.as_bytes());
    while let Some((query_match, capture_index)) = captures.next() {
        let capture = query_match.captures[*capture_index];
        if capture.index == parser.captures.member {
            if has_preprocessor_ancestor(capture.node) {
                continue;
            }
            let Some(name) = declarator_name(capture.node) else {
                continue;
            };
            let Some(owner) = member_owner(capture.node, source)? else {
                continue;
            };
            let name = normalize_qualified(text(name, &source.text));
            if !name.is_empty() {
                lookup
                    .members
                    .insert(item_key(&checked_join(&owner, &name)?));
            }
        } else if capture.index == parser.captures.function {
            if has_preprocessor_ancestor(capture.node) {
                continue;
            }
            let Some((raw_name, _)) = function_name(capture.node, &source.text) else {
                continue;
            };
            let path = if let Some(owner) = member_owner(capture.node, source)? {
                definition_path(&owner, &raw_name)?
            } else {
                let name = function_leaf(&raw_name);
                if raw_name
                    .strip_suffix(name)
                    .and_then(|prefix| prefix.strip_suffix("::"))
                    .is_none_or(str::is_empty)
                {
                    continue;
                }
                definition_path(&namespace_owner(capture.node, source)?, &raw_name)?
            };
            lookup.members.insert(item_key(&path));
        } else if capture.index == parser.captures.type_
            && let Some(body) = capture.node.child_by_field_name("body")
            && let Some(path) = member_owner(body, source)?
        {
            let container = path
                .rsplit_once("::")
                .map_or("", |(container, _)| container);
            let bases = base_paths(capture.node, source, container)?;
            if has_preprocessor_ancestor(capture.node) || !class_body_is_complete(body) {
                lookup.incomplete_types.insert(path.clone());
            }
            lookup.bases.insert(path, bases);
        }
    }
    drop(captures);
    if parser.cursor.did_exceed_match_limit() {
        return Err("C++ query exceeded Tree-sitter's match limit".into());
    }
    Ok(lookup)
}

fn class_body_is_complete(body: Node<'_>) -> bool {
    if body.has_error() {
        return false;
    }
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .all(|child| match child.kind() {
            "access_specifier"
            | "comment"
            | "function_definition"
            | "static_assert_declaration" => true,
            "field_declaration" => declarator_name(child).is_some() && !contains_nested_type(child),
            _ => false,
        })
}

fn contains_nested_type(node: Node<'_>) -> bool {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if matches!(
                child.kind(),
                "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
            ) && child.child_by_field_name("body").is_some()
            {
                return true;
            }
            pending.push(child);
        }
    }
    false
}

fn has_preprocessor_ancestor(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if parent.kind().starts_with("preproc_") {
            return true;
        }
        ancestor = parent.parent();
    }
    false
}

fn namespace_owner(node: Node<'_>, source: &Source) -> Result<String, String> {
    ancestor_owner(node, source, false).map(|(owner, _)| owner)
}

fn member_owner(node: Node<'_>, source: &Source) -> Result<Option<String>, String> {
    let (owner, has_type) = ancestor_owner(node, source, true)?;
    Ok((has_type && !owner.is_empty()).then_some(owner))
}

fn ancestor_owner(
    node: Node<'_>,
    source: &Source,
    include_types: bool,
) -> Result<(String, bool), String> {
    let mut parts = Vec::new();
    let mut has_type = false;
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        match parent.kind() {
            "namespace_definition" => parts.push(parent.child_by_field_name("name").map_or_else(
                || anonymous_namespace(&source.path),
                |name| normalize_qualified(text(name, &source.text)),
            )),
            "class_specifier" | "struct_specifier" | "union_specifier" if include_types => {
                if let Some(name) = parent.child_by_field_name("name") {
                    parts.push(normalize_qualified(text(name, &source.text)));
                    has_type = true;
                }
            }
            _ => {}
        }
        ancestor = parent.parent();
    }
    parts.reverse();
    let mut owner = String::new();
    for part in parts.into_iter().filter(|part| !part.is_empty()) {
        owner = checked_join(&owner, &part)?;
    }
    Ok((owner, has_type))
}

fn add_relation_gap(
    graph: &mut Graph,
    source: &Source,
    source_key: &str,
    node: Node<'_>,
    category: GapCategory,
    reason: GapReason,
    target_hint: String,
) -> Result<(), String> {
    graph.gaps.push(GapInput {
        file_key: Some(source.path.clone()),
        source_key: Some(source_key.to_owned()),
        run_key: None,
        path: Some(source.path.clone()),
        line_start: Some(to_u32(line_start(node))?),
        line_end: Some(to_u32(line_end(node))?),
        category,
        reason,
        target_hint: (!target_hint.is_empty()).then_some(target_hint),
        occurrences: 1,
        relation_site: true,
    });
    Ok(())
}

fn add_parse_gap(
    graph: &mut Graph,
    source: &Source,
    line_start: usize,
    line_end: usize,
    reason: GapReason,
) -> Result<(), String> {
    graph.gaps.push(GapInput {
        file_key: Some(source.path.clone()),
        source_key: None,
        run_key: None,
        path: Some(source.path.clone()),
        line_start: Some(to_u32(line_start)?),
        line_end: Some(to_u32(line_end)?),
        category: GapCategory::Parse,
        reason,
        target_hint: None,
        occurrences: 1,
        relation_site: false,
    });
    Ok(())
}

fn is_preprocessor_operand(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        match parent.kind() {
            "preproc_include" | "preproc_def" | "preproc_function_def" | "preproc_call" => {
                return true;
            }
            "preproc_if" | "preproc_elif"
                if parent
                    .child_by_field_name("condition")
                    .is_some_and(|condition| {
                        condition.start_byte() <= node.start_byte()
                            && node.end_byte() <= condition.end_byte()
                    }) =>
            {
                return true;
            }
            _ => {}
        }
        ancestor = parent.parent();
    }
    false
}

fn add_base_relations(
    graph: &mut Graph,
    source: &Source,
    node: Node<'_>,
    container: &str,
    implementor: &str,
) -> Result<(), String> {
    for base in base_paths(node, source, container)? {
        graph.trait_implementations.push(TraitImplementationInput {
            file_key: source.path.clone(),
            line_start: to_u32(line_start(node))?,
            line_end: to_u32(line_end(node))?,
            implementor_key: item_key(implementor),
            trait_key: item_key(&base),
        });
    }
    Ok(())
}

fn base_paths(node: Node<'_>, source: &Source, container: &str) -> Result<Vec<String>, String> {
    let mut cursor = node.walk();
    let Some(clause) = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "base_class_clause")
    else {
        return Ok(Vec::new());
    };
    let mut bases = Vec::new();
    let mut cursor = clause.walk();
    for base in clause
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "access_specifier")
    {
        let raw = normalize_qualified(text(base, &source.text));
        if raw.is_empty() {
            continue;
        }
        bases.push(definition_path(container, &raw)?);
    }
    Ok(bases)
}

fn function_name<'tree>(node: Node<'tree>, source: &str) -> Option<(String, Node<'tree>)> {
    let mut declarator = node.child_by_field_name("declarator")?;
    if let Some(name) = conversion_operator_name(declarator, source) {
        return Some((name, declarator));
    }
    while declarator.kind() != "function_declarator" {
        declarator = declarator.child_by_field_name("declarator")?;
    }
    let name = declarator.child_by_field_name("declarator")?;
    let name = declarator_name(name)?;
    let raw = normalize_qualified(text(name, source));
    (!raw.is_empty()).then_some((raw, declarator))
}

fn conversion_operator_name(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "qualified_identifier" {
        let scope = node.child_by_field_name("scope")?;
        let name = conversion_operator_name(node.child_by_field_name("name")?, source)?;
        return Some(format!(
            "{}::{name}",
            normalize_qualified(text(scope, source))
        ));
    }
    if node.kind() != "operator_cast" {
        return None;
    }
    let mut declarator = node.child_by_field_name("declarator")?;
    let parameters = loop {
        if let Some(parameters) = declarator.child_by_field_name("parameters") {
            break parameters;
        }
        declarator = declarator.child_by_field_name("declarator")?;
    };
    let name = source.get(node.start_byte()..parameters.start_byte())?;
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(normalize_qualified(&name))
}

fn declarator_name(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        match node.kind() {
            "identifier"
            | "field_identifier"
            | "qualified_identifier"
            | "destructor_name"
            | "operator_name" => return Some(node),
            "template_function" | "template_method" => {
                return node
                    .child_by_field_name("name")
                    .or_else(|| node.named_child(0));
            }
            _ => node = node.child_by_field_name("declarator")?,
        }
    }
}

fn google_test_name(
    node: Node<'_>,
    raw: &str,
    declarator: Node<'_>,
    inside_type: bool,
    source: &str,
) -> Option<String> {
    if inside_type
        || node.child_by_field_name("type").is_some()
        || !matches!(
            raw,
            "TEST" | "TEST_F" | "TEST_P" | "TYPED_TEST" | "TYPED_TEST_P"
        )
    {
        return None;
    }
    let parameters = declarator.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let mut parts = parameters
        .named_children(&mut cursor)
        .filter_map(|parameter| {
            parameter
                .child_by_field_name("type")
                .map(|type_| text(type_, source).trim().to_owned())
        });
    let suite = parts.next()?;
    let test = parts.next()?;
    (!suite.is_empty() && !test.is_empty()).then(|| format!("{suite}.{test}"))
}

fn catch_test<'tree>(node: Node<'tree>, source: &str) -> Option<(String, Node<'tree>)> {
    let function = node.child_by_field_name("function")?;
    if text(function, source) != "TEST_CASE" {
        return None;
    }
    let statement = node
        .parent()
        .filter(|parent| parent.kind() == "expression_statement")?;
    let body = statement
        .next_named_sibling()
        .filter(|sibling| sibling.kind() == "compound_statement")?;
    let arguments = node.child_by_field_name("arguments")?;
    let literal = arguments.named_child(0)?;
    let name = string_content(literal, source)?.to_owned();
    Some((name, body))
}

#[derive(Clone)]
enum MemberLookup {
    Found(String),
    Absent,
    Unknown,
}

fn member_lookup(
    owner: &str,
    name: &str,
    classes: &mut ClassLookup,
) -> Result<MemberLookup, String> {
    let cache_key = (owner.to_owned(), name.to_owned());
    if let Some(result) = classes.cache.get(&cache_key) {
        return Ok(result.clone());
    }

    let result = (|| -> Result<MemberLookup, String> {
        let mut pending = vec![owner];
        let mut visited = HashSet::new();
        let mut found = None;
        while let Some(owner) = pending.pop() {
            if visited.len() == CLASS_LOOKUP_LIMIT || !visited.insert(owner) {
                return Ok(MemberLookup::Unknown);
            }
            let key = item_key(&checked_join(owner, name)?);
            if classes.members.contains(&key) {
                if found.is_some() {
                    return Ok(MemberLookup::Unknown);
                }
                found = Some(key);
                continue;
            }
            if classes.incomplete_types.contains(owner) {
                return Ok(MemberLookup::Unknown);
            }
            let Some(bases) = classes.bases.get(owner) else {
                return Ok(MemberLookup::Unknown);
            };
            if pending.len().saturating_add(bases.len()) > CLASS_LOOKUP_LIMIT {
                return Ok(MemberLookup::Unknown);
            }
            pending.extend(bases.iter().map(String::as_str));
        }
        Ok(found.map_or(MemberLookup::Absent, MemberLookup::Found))
    })()?;
    classes.cache.insert(cache_key, result.clone());
    Ok(result)
}

fn scoped_member_lookup(
    owner: &str,
    name: &str,
    classes: &mut ClassLookup,
) -> Result<MemberLookup, String> {
    let mut current = owner;
    loop {
        match member_lookup(current, name, classes)? {
            MemberLookup::Absent => {}
            result => return Ok(result),
        }
        let Some(enclosing) = enclosing_type(current, classes) else {
            return Ok(MemberLookup::Absent);
        };
        current = enclosing;
    }
}

fn enclosing_type<'a>(owner: &'a str, classes: &ClassLookup) -> Option<&'a str> {
    let mut candidate = owner;
    while let Some((parent, _)) = candidate.rsplit_once("::") {
        if classes.bases.contains_key(parent) {
            return Some(parent);
        }
        candidate = parent;
    }
    None
}

fn call_keys(
    node: Node<'_>,
    source: &str,
    source_path: &str,
    scope: &Scope,
    bindings: &[Binding],
    anonymous_scopes: &HashSet<String>,
    class_lookup: &mut ClassLookup,
) -> Result<Vec<String>, String> {
    let Some(function) = node.child_by_field_name("function") else {
        return Ok(Vec::new());
    };
    match function.kind() {
        "identifier" | "template_function" => {
            let Some(name) = call_name(function, source) else {
                return Ok(Vec::new());
            };
            if bindings.iter().any(|binding| {
                binding.name == name
                    && binding.start_byte <= node.start_byte()
                    && node.start_byte() < binding.end_byte
            }) {
                Ok(Vec::new())
            } else if let Some(owner) = scope.type_path.as_deref() {
                match scoped_member_lookup(owner, &name, class_lookup)? {
                    MemberLookup::Found(key) => Ok(vec![key]),
                    MemberLookup::Absent => {
                        unqualified_keys(&scope.container, &name, source_path, anonymous_scopes)
                    }
                    // ponytail: cross-file class lookup stays unresolved; add a project-wide C++
                    // symbol table when call precision requires it.
                    MemberLookup::Unknown => Ok(Vec::new()),
                }
            } else {
                unqualified_keys(&scope.container, &name, source_path, anonymous_scopes)
            }
        }
        "qualified_identifier" => call_name(function, source)
            .map(|name| qualified_keys(&scope.container, &name))
            .unwrap_or_else(|| Ok(Vec::new())),
        "field_expression" => {
            let Some(argument) = function.child_by_field_name("argument") else {
                return Ok(Vec::new());
            };
            if argument.kind() != "this" {
                return Ok(Vec::new());
            }
            let Some(field) = function.child_by_field_name("field") else {
                return Ok(Vec::new());
            };
            let Some(owner) = scope.type_path.as_deref() else {
                return Ok(Vec::new());
            };
            let Some(field) = call_name(field, source) else {
                return Ok(Vec::new());
            };
            Ok(vec![item_key(&checked_join(owner, &field)?)])
        }
        _ => Ok(Vec::new()),
    }
}

fn binding_scope_start(node: Node<'_>) -> usize {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if parent.kind() == "for_range_loop"
            && parent
                .child_by_field_name("declarator")
                .is_some_and(|declarator| {
                    declarator.start_byte() <= node.start_byte()
                        && node.end_byte() <= declarator.end_byte()
                })
        {
            return parent
                .child_by_field_name("body")
                .map_or(node.end_byte(), |body| body.start_byte());
        }
        ancestor = parent.parent();
    }
    node.start_byte()
}

fn binding_scope_end(node: Node<'_>) -> usize {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if matches!(
            parent.kind(),
            "compound_statement"
                | "for_range_loop"
                | "for_statement"
                | "if_statement"
                | "switch_statement"
                | "while_statement"
                | "catch_clause"
                | "lambda_expression"
                | "function_definition"
                | "namespace_definition"
                | "class_specifier"
                | "struct_specifier"
                | "union_specifier"
                | "translation_unit"
        ) {
            return parent.end_byte();
        }
        ancestor = parent.parent();
    }
    node.end_byte()
}

fn call_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "operator_name" => Some(text(node, source).to_owned()),
        "template_function" | "template_method" => {
            call_name(node.child_by_field_name("name")?, source)
        }
        "qualified_identifier" => {
            let name = call_name(node.child_by_field_name("name")?, source)?;
            let absolute = text(node, source).trim_start().starts_with("::");
            let Some(scope) = node.child_by_field_name("scope") else {
                return absolute.then(|| format!("::{name}"));
            };
            let scope = normalize_qualified(text(scope, source));
            Some(format!(
                "{}{}::{name}",
                if absolute { "::" } else { "" },
                scope
            ))
        }
        _ => None,
    }
}

fn unqualified_keys(
    container: &str,
    raw: &str,
    source_path: &str,
    anonymous_scopes: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let name = normalize_qualified(raw);
    if name.is_empty() || name.contains("::") {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    let mut scope = container;
    let anonymous = anonymous_namespace(source_path);
    loop {
        let path = checked_join(scope, &name)?;
        keys.push(item_key(&path));
        if anonymous_scopes.contains(scope) {
            let anonymous_path = checked_join(scope, &anonymous)?;
            keys.push(item_key(&checked_join(&anonymous_path, &name)?));
        }
        if scope.is_empty() {
            break;
        }
        scope = scope.rsplit_once("::").map_or("", |(parent, _)| parent);
    }
    keys.dedup();
    Ok(keys)
}

fn qualified_keys(container: &str, raw: &str) -> Result<Vec<String>, String> {
    let absolute = raw.trim_start().starts_with("::");
    let name = normalize_qualified(raw);
    if name.is_empty() {
        return Ok(Vec::new());
    }
    if absolute || container.is_empty() {
        return Ok(vec![item_key(&name)]);
    }
    let mut keys = Vec::new();
    let mut scope = container;
    loop {
        keys.push(item_key(&checked_join(scope, &name)?));
        let Some((parent, _)) = scope.rsplit_once("::") else {
            break;
        };
        scope = parent;
    }
    keys.push(item_key(&name));
    keys.dedup();
    Ok(keys)
}

fn definition_path(container: &str, raw: &str) -> Result<String, String> {
    let absolute = raw.trim_start().starts_with("::");
    let name = normalize_qualified(raw);
    if absolute || container.is_empty() || first_component(container) == first_component(&name) {
        check_path(name)
    } else {
        checked_join(container, &name)
    }
}

fn include_keys(source_path: &str, raw: &str) -> Vec<String> {
    let base = Path::new(source_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    normalize_path(base.join(raw))
        .map(|path| format!("cpp:include:{path}"))
        .into_iter()
        .collect()
}

fn include_aliases(path: &str) -> Vec<String> {
    normalize_path(path)
        .map(|path| format!("cpp:include:{path}"))
        .into_iter()
        .collect()
}

fn normalize_path(path: impl AsRef<Path>) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.as_ref().components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let path = parts.join("/");
    (!path.is_empty() && path.len() <= PATH_LIMIT).then_some(path)
}

fn string_content<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "string_content" | "raw_string_content") {
            return Some(text(node, source));
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    None
}

fn signature(node: Node<'_>, source: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    bounded_text(
        source
            .get(node.start_byte()..end)
            .unwrap_or_default()
            .trim_end(),
    )
}

fn bounded_text(value: &str) -> String {
    let value = value.trim();
    let mut end = value.len().min(SIGNATURE_LIMIT);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn capture(query: &Query, name: &str) -> Result<u32, String> {
    query
        .capture_index_for_name(name)
        .ok_or_else(|| format!("C++ query is missing @{name}"))
}

fn identity(path: &str, kind: &str, scope: &str, line: usize, ordinal: usize) -> String {
    format!(
        "cpp:{}#{path}:{}#{kind}:{}#{scope}:{line}:{ordinal}",
        path.len(),
        kind.len(),
        scope.len()
    )
}

fn item_key(path: &str) -> String {
    format!("cpp:item:{path}")
}

fn anonymous_namespace(path: &str) -> String {
    format!("@anonymous:{}", blake3::hash(path.as_bytes()).to_hex())
}

fn checked_join(left: &str, right: &str) -> Result<String, String> {
    let separator = usize::from(!left.is_empty() && !right.is_empty()) * 2;
    left.len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(right.len()))
        .filter(|length| *length <= PATH_LIMIT)
        .ok_or_else(|| "C++ qualified path exceeds 1024 bytes".to_owned())?;
    Ok(if left.is_empty() {
        right.to_owned()
    } else if right.is_empty() {
        left.to_owned()
    } else {
        format!("{left}::{right}")
    })
}

fn check_path(path: String) -> Result<String, String> {
    (path.len() <= PATH_LIMIT)
        .then_some(path)
        .ok_or_else(|| "C++ qualified path exceeds 1024 bytes".to_owned())
}

fn normalize_qualified(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("::")
        .split("::")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("::")
}

fn first_component(path: &str) -> &str {
    path.split("::").next().unwrap_or_default()
}

fn last_component(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn function_leaf(path: &str) -> &str {
    if path.starts_with("operator") {
        path
    } else if let Some(operator) = path.rfind("::operator") {
        &path[operator + 2..]
    } else {
        last_component(path)
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

fn text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    source.get(node.byte_range()).unwrap_or_default()
}

fn line_start(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

fn line_end(node: Node<'_>) -> usize {
    node.end_position().row + 1
}

fn to_u32(value: usize) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| "C++ source line exceeds supported range".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::git::Source;
    use crate::store::{Graph, NodeKind};

    #[test]
    fn emits_cpp_graph_evidence() {
        let source = Source {
            path: "tests/worker.cpp".into(),
            text: r#"#include "base.hpp"
namespace app {
struct Base {};
class Worker : public Base {
public:
  Worker() {}
  ~Worker() {}
  void run() { helper(); this->flush(); }
  void flush() {}
};
int helper() { return 0; }
int execute() { Worker worker; worker.run(); app::helper(); }
}
TEST(WorkerTest, Runs) { app::execute(); }
TEST_CASE("worker runs") { app::execute(); }
"#
            .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|node| (node.kind, node.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (NodeKind::File, "tests/worker.cpp"),
                (NodeKind::Type, "Base"),
                (NodeKind::Type, "Worker"),
                (NodeKind::Function, "Worker"),
                (NodeKind::Function, "~Worker"),
                (NodeKind::Function, "run"),
                (NodeKind::Function, "flush"),
                (NodeKind::Function, "helper"),
                (NodeKind::Function, "execute"),
                (NodeKind::Test, "WorkerTest.Runs"),
                (NodeKind::Test, "worker runs"),
            ]
        );
        assert_eq!(
            graph
                .nodes
                .iter()
                .flat_map(|node| node.keys.iter())
                .filter(|key| key.starts_with("cpp:item:"))
                .cloned()
                .collect::<Vec<_>>(),
            [
                "cpp:item:app::Base",
                "cpp:item:app::Worker",
                "cpp:item:app::Worker::Worker",
                "cpp:item:app::Worker::~Worker",
                "cpp:item:app::Worker::run",
                "cpp:item:app::Worker::flush",
                "cpp:item:app::helper",
                "cpp:item:app::execute",
                "cpp:item:WorkerTest.Runs",
                "cpp:item:worker runs",
            ]
        );

        let names = graph
            .nodes
            .iter()
            .map(|node| (node.key.as_str(), node.name.as_str()))
            .collect::<HashMap<_, _>>();
        let mut relations = graph
            .refs
            .iter()
            .map(|reference| {
                format!(
                    "{:?}:{}:{}:{}",
                    reference.kind,
                    names[reference.source_key.as_str()],
                    reference.line,
                    reference.keys.join("|")
                )
            })
            .collect::<Vec<_>>();
        relations.sort();
        assert_eq!(
            relations,
            [
                "Calls:WorkerTest.Runs:14:cpp:item:app::execute",
                "Calls:execute:12:cpp:item:app::app::helper|cpp:item:app::helper",
                "Calls:run:8:cpp:item:app::Worker::flush",
                "Calls:run:8:cpp:item:app::Worker::helper|cpp:item:app::helper|cpp:item:helper",
                "Calls:worker runs:15:cpp:item:app::execute",
                "Imports:tests/worker.cpp:1:cpp:include:tests/base.hpp",
            ]
        );
        assert_eq!(
            graph
                .trait_implementations
                .iter()
                .map(|relation| (
                    relation.implementor_key.as_str(),
                    relation.trait_key.as_str(),
                ))
                .collect::<Vec<_>>(),
            [("cpp:item:app::Worker", "cpp:item:app::Base")]
        );
        assert_eq!(
            graph
                .modeled_sites
                .iter()
                .filter_map(|site| site.target_hint.as_deref())
                .collect::<Vec<_>>(),
            ["WorkerTest.Runs", "worker runs"]
        );
    }

    #[test]
    fn indexes_conversion_operators() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"struct Worker {
  operator bool() const { return true; }
  operator int() const;
};
Worker::operator int() const { return 1; }"#
                .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        assert!(graph.nodes.iter().any(|node| {
            node.kind == NodeKind::Function
                && node.name == "operator bool"
                && node.keys == ["cpp:item:Worker::operator bool"]
        }));
        assert!(graph.nodes.iter().any(|node| {
            node.kind == NodeKind::Function
                && node.name == "operator int"
                && node.keys == ["cpp:item:Worker::operator int"]
        }));
    }

    #[test]
    fn keeps_qualified_conversion_types_in_the_method_name() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"struct Worker { operator std::string() const; };
Worker::operator std::string() const { return {}; }"#
                .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let method = graph
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::Function)
            .unwrap();
        assert_eq!(method.name, "operator std::string");
        assert_eq!(method.keys, ["cpp:item:Worker::operator std::string"]);
        assert_eq!(method.owner_key.as_deref(), Some("cpp:item:Worker"));
    }

    #[test]
    fn keeps_quoted_includes_directory_local_without_include_paths() {
        let mut graph = Graph::default();
        let mut parser = CppParser::new().unwrap();
        for source in [
            Source {
                path: "src/a/main.cpp".into(),
                text: "#include \"foo.h\"\n#include <vector>".into(),
            },
            Source {
                path: "src/a/foo.h".into(),
                text: String::new(),
            },
            Source {
                path: "src/b/foo.h".into(),
                text: String::new(),
            },
        ] {
            add_file(&mut graph, &source, &mut parser).unwrap();
        }

        assert_eq!(graph.refs[0].keys, ["cpp:include:src/a/foo.h"]);
        assert_eq!(
            graph
                .gaps
                .iter()
                .filter(|gap| gap.reason == GapReason::ExternalDependency)
                .count(),
            1
        );
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| node.kind == NodeKind::File)
                .map(|node| node.keys.as_slice())
                .collect::<Vec<_>>(),
            [
                &["cpp:include:src/a/main.cpp"][..],
                &["cpp:include:src/a/foo.h"][..],
                &["cpp:include:src/b/foo.h"][..],
            ]
        );
    }

    #[test]
    fn strips_template_arguments_from_call_keys() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"namespace app {
template <typename T> void helper() {}
struct Worker {
  template <typename T> void flush() {}
  void run() { app::helper<int>(); this->flush<int>(); }
};
}"#
            .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        assert_eq!(
            graph
                .refs
                .iter()
                .filter(|reference| reference.source_key == run.key)
                .map(|reference| reference.keys.as_slice())
                .collect::<Vec<_>>(),
            [
                &[
                    "cpp:item:app::Worker::app::helper",
                    "cpp:item:app::app::helper",
                    "cpp:item:app::helper",
                ][..],
                &["cpp:item:app::Worker::flush"][..],
            ]
        );
    }

    #[test]
    fn ignores_calls_inside_preprocessor_directives() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"void real() {}
void run() {
#if CHECK(flag)
  real();
#endif
#include HEADER(foo.h)
}"#
            .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        assert_eq!(
            graph
                .refs
                .iter()
                .filter(|reference| reference.source_key == run.key)
                .flat_map(|reference| reference.keys.iter().map(String::as_str))
                .collect::<Vec<_>>(),
            ["cpp:item:real"]
        );
        assert_eq!(
            graph
                .gaps
                .iter()
                .filter(|gap| gap.reason == GapReason::MacroExpansionUnavailable)
                .count(),
            2
        );
    }

    #[test]
    fn distinguishes_google_test_macros_from_functions_named_test() {
        let source = Source {
            path: "worker_test.cpp".into(),
            text: r#"void TEST(Foo suite, Bar test) {}
struct Holder { void TEST(Foo suite, Bar test) {} };
TEST(WorkerTest, Runs) {}"#
                .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| node.name == "TEST" || node.name == "WorkerTest.Runs")
                .map(|node| (node.kind, node.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (NodeKind::Function, "TEST"),
                (NodeKind::Function, "TEST"),
                (NodeKind::Test, "WorkerTest.Runs"),
            ]
        );
    }

    #[test]
    fn keeps_relative_qualified_calls_lexical() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"namespace app {
void work() {}
namespace inner {
namespace app { void work() {} }
void run() { app::work(); }
}
}"#
            .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        let call = graph
            .refs
            .iter()
            .find(|reference| reference.source_key == run.key)
            .unwrap();
        assert_eq!(
            call.keys,
            [
                "cpp:item:app::inner::app::work",
                "cpp:item:app::app::work",
                "cpp:item:app::work",
            ]
        );
    }

    #[test]
    fn resolves_absolute_calls_with_and_without_template_arguments() {
        let source = Source {
            path: "worker.cpp".into(),
            text: "template <typename T = int> void target() {} void run() { ::target(); ::target<int>(); }".into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        assert_eq!(
            graph
                .refs
                .iter()
                .filter(|reference| reference.source_key == run.key)
                .map(|reference| reference.keys.as_slice())
                .collect::<Vec<_>>(),
            [&["cpp:item:target"][..], &["cpp:item:target"][..]]
        );
    }

    #[test]
    fn scopes_anonymous_namespaces_to_the_translation_unit() {
        let mut graph = Graph::default();
        let mut parser = CppParser::new().unwrap();
        for path in ["src/a.cpp", "src/b.cpp"] {
            add_file(
                &mut graph,
                &Source {
                    path: path.into(),
                    text: "namespace { void helper() {} void run() { helper(); } }".into(),
                },
                &mut parser,
            )
            .unwrap();
        }

        let function = |path: &str, name: &str| {
            graph
                .nodes
                .iter()
                .find(|node| node.file_key == path && node.name == name)
                .unwrap()
        };
        let a_helper = function("src/a.cpp", "helper");
        let b_helper = function("src/b.cpp", "helper");
        assert_ne!(a_helper.keys, b_helper.keys);
        for path in ["src/a.cpp", "src/b.cpp"] {
            let run = function(path, "run");
            let reference = graph
                .refs
                .iter()
                .find(|reference| reference.source_key == run.key)
                .unwrap();
            assert_eq!(
                reference.keys.first(),
                function(path, "helper").keys.first()
            );
            let other = if path == "src/a.cpp" {
                &b_helper.keys[0]
            } else {
                &a_helper.keys[0]
            };
            assert!(!reference.keys.contains(other));
        }
    }

    #[test]
    fn resolves_anonymous_namespace_members_from_the_enclosing_scope() {
        let mut graph = Graph::default();
        let mut parser = CppParser::new().unwrap();
        for path in ["src/a.cpp", "src/b.cpp"] {
            add_file(
                &mut graph,
                &Source {
                    path: path.into(),
                    text: "namespace { void helper() {} } void run() { helper(); }".into(),
                },
                &mut parser,
            )
            .unwrap();
        }

        for path in ["src/a.cpp", "src/b.cpp"] {
            let function = |name: &str| {
                graph
                    .nodes
                    .iter()
                    .find(|node| node.file_key == path && node.name == name)
                    .unwrap()
            };
            let reference = graph
                .refs
                .iter()
                .find(|reference| reference.source_key == function("run").key)
                .unwrap();
            let own_helper = &function("helper").keys[0];
            assert!(reference.keys.contains(own_helper));
            let other_path = if path == "src/a.cpp" {
                "src/b.cpp"
            } else {
                "src/a.cpp"
            };
            let other_helper = graph
                .nodes
                .iter()
                .find(|node| node.file_key == other_path && node.name == "helper")
                .unwrap();
            assert!(!reference.keys.contains(&other_helper.keys[0]));
        }
    }

    #[test]
    fn omits_direct_calls_shadowed_by_local_values() {
        let source = Source {
            path: "worker.cpp".into(),
            text: "void target() {} void run() { auto target = [] {}; target(); }".into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        assert!(
            graph
                .refs
                .iter()
                .all(|reference| reference.source_key != run.key)
        );
        assert_eq!(
            graph
                .gaps
                .iter()
                .filter(|gap| gap.reason == GapReason::DynamicOrUnsupportedDispatch)
                .count(),
            1
        );
    }

    #[test]
    fn class_members_block_global_fallback_without_hiding_method_definitions() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"void target() {}
struct Data {
  Callable target;
  void run() { target(); }
};
struct Method {
  void target();
  void run() { target(); }
};
void Method::target() {}"#
                .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        for owner in ["Data", "Method"] {
            let run = graph
                .nodes
                .iter()
                .find(|node| node.keys == [format!("cpp:item:{owner}::run")])
                .unwrap();
            let reference = graph
                .refs
                .iter()
                .find(|reference| reference.source_key == run.key)
                .unwrap();
            assert_eq!(reference.keys, [format!("cpp:item:{owner}::target")]);
        }
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.keys == ["cpp:item:Method::target"])
        );
    }

    #[test]
    fn resolves_inherited_members_without_global_fallback() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"void target() {}
struct Base { Callable target; };
struct Derived : Base { void run() { target(); } };"#
                .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        let reference = graph
            .refs
            .iter()
            .find(|reference| reference.source_key == run.key)
            .unwrap();
        assert_eq!(reference.keys, ["cpp:item:Base::target"]);
    }

    #[test]
    fn omits_global_fallback_when_header_class_lookup_is_incomplete() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"#include "worker.hpp"
void target() {}
void Worker::run() { target(); }"#
                .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        assert!(
            graph
                .refs
                .iter()
                .all(|reference| reference.source_key != run.key)
        );
        assert!(graph.gaps.iter().any(|gap| {
            gap.source_key.as_deref() == Some(&run.key)
                && gap.reason == GapReason::DynamicOrUnsupportedDispatch
        }));
    }

    #[test]
    fn resolves_enclosing_class_members_before_global_fallback() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"void target() {}
struct Outer {
  inline static Callable target;
  struct Inner { void run() { target(); } };
};"#
            .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        let reference = graph
            .refs
            .iter()
            .find(|reference| reference.source_key == run.key)
            .unwrap();
        assert_eq!(reference.keys, ["cpp:item:Outer::target"]);
    }

    #[test]
    fn uses_known_out_of_class_methods_when_header_lookup_is_incomplete() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"#include "worker.hpp"
void target() {}
void Worker::target() {}
void Worker::run() { target(); }"#
                .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        let reference = graph
            .refs
            .iter()
            .find(|reference| reference.source_key == run.key)
            .unwrap();
        assert_eq!(reference.keys, ["cpp:item:Worker::target"]);
    }

    #[test]
    fn gaps_uninventoried_class_scopes_instead_of_falling_back_globally() {
        for text in [
            r#"void target() {}
struct AliasOwner { using target = Callable; void run() { target(); } };"#,
            r#"void target() {}
struct TypeOwner { struct target {}; void run() { target(); } };"#,
            r#"void target() {}
struct MacroOwner {
#ifdef FEATURE
  void feature();
#endif
  void run() { target(); }
};"#,
        ] {
            let source = Source {
                path: "worker.cpp".into(),
                text: text.into(),
            };
            let mut graph = Graph::default();

            add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

            let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
            assert!(
                graph
                    .refs
                    .iter()
                    .all(|reference| reference.source_key != run.key)
            );
            assert!(graph.gaps.iter().any(|gap| {
                gap.source_key.as_deref() == Some(&run.key)
                    && gap.reason == GapReason::DynamicOrUnsupportedDispatch
            }));
        }
    }

    #[test]
    fn comments_do_not_block_class_calls_to_namespace_functions() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"namespace app {
void helper() {}
struct Worker {
  // Lookup-neutral.
  void run() { helper(); }
};
}"#
            .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        let reference = graph
            .refs
            .iter()
            .find(|reference| reference.source_key == run.key)
            .unwrap();
        assert!(reference.keys.contains(&"cpp:item:app::helper".into()));
    }

    #[test]
    fn scopes_range_and_structured_bindings_after_the_range_expression() {
        let source = Source {
            path: "worker.cpp".into(),
            text: r#"void target() {}
void run() {
  for (auto target : target()) target();
  for (auto [target, value] : pairs) target();
  auto [target, value] = pair;
  target();
}"#
            .into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        let run = graph.nodes.iter().find(|node| node.name == "run").unwrap();
        assert_eq!(
            graph
                .refs
                .iter()
                .filter(|reference| reference.source_key == run.key)
                .map(|reference| reference.keys.as_slice())
                .collect::<Vec<_>>(),
            [&["cpp:item:target"][..]]
        );
    }

    #[test]
    fn indexes_raw_string_test_case_names() {
        let source = Source {
            path: "worker_test.cpp".into(),
            text: "TEST_CASE(R\"case(worker runs)case\") {}".into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::Test && node.name == "worker runs")
        );
    }

    #[test]
    fn reports_parse_gaps_for_malformed_cpp() {
        let source = Source {
            path: "broken.cpp".into(),
            text: "void run( { target();".into(),
        };
        let mut graph = Graph::default();

        add_file(&mut graph, &source, &mut CppParser::new().unwrap()).unwrap();

        assert!(graph.gaps.iter().any(|gap| {
            gap.category == GapCategory::Parse && gap.reason == GapReason::ParserError
        }));
    }
}
