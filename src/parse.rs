use std::collections::HashMap;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

const RUST_QUERY: &str = include_str!("../queries/rust.scm");
const PATH_LIMIT: usize = 1024;
const SIGNATURE_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefinitionKind {
    Type,
    Function,
    Method,
    Test,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Definition {
    pub kind: DefinitionKind,
    pub name: String,
    pub parent: Option<usize>,
    pub impl_target: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: String,
    pub module: Option<usize>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Import {
    pub source: Option<usize>,
    pub path: String,
    pub line: usize,
    pub module: Option<usize>,
    pub block_local: bool,
    pub exported: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Call {
    pub source: usize,
    pub target: String,
    pub line: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Implementation {
    pub type_target: String,
    pub trait_target: String,
    pub module: Option<usize>,
    pub line_start: usize,
    pub line_end: usize,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct ParsedFile {
    pub definitions: Vec<Definition>,
    pub implementations: Vec<Implementation>,
    pub modules: Vec<Module>,
    pub imports: Vec<Import>,
    pub bindings: Vec<ValueBinding>,
    pub calls: Vec<Call>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ValueBinding {
    pub source: usize,
    pub name: String,
    pub type_target: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Module {
    pub name: String,
    pub parent: Option<usize>,
}

#[derive(Clone, Copy)]
enum MethodContainer<'source> {
    Definition(usize),
    Impl {
        target: &'source str,
        local_name: Option<&'source str>,
        fallback_parent: Option<usize>,
    },
}

struct Scope<'source> {
    end_byte: usize,
    parent: Option<usize>,
    method_container: Option<MethodContainer<'source>>,
    module: Option<usize>,
}

struct PendingParent<'source> {
    definition: usize,
    local_name: &'source str,
    module: Option<usize>,
}

struct AttributeChain {
    parent: usize,
    last: usize,
    has_test: bool,
    line_start: usize,
}

pub struct RustParser {
    parser: Parser,
    query: Query,
    cursor: QueryCursor,
    captures: Captures,
}

struct Captures {
    type_: u32,
    module: u32,
    implementation: u32,
    function: u32,
    attribute: u32,
    import: u32,
    binding: u32,
    call: u32,
}

impl RustParser {
    pub fn new() -> Result<Self, String> {
        let language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|error| error.to_string())?;
        let query = Query::new(&language, RUST_QUERY).map_err(|error| error.to_string())?;
        let captures = Captures {
            type_: capture(&query, "type")?,
            module: capture(&query, "module")?,
            implementation: capture(&query, "implementation")?,
            function: capture(&query, "function")?,
            attribute: capture(&query, "attribute")?,
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

    pub fn parse(&mut self, source: &str) -> Result<ParsedFile, String> {
        let Some(tree) = self.parser.parse(source, None) else {
            return Ok(ParsedFile::default());
        };

        let mut parsed = ParsedFile::default();
        let mut pending_parents = Vec::new();
        let mut type_parents = HashMap::new();
        let mut scopes = Vec::<Scope<'_>>::new();
        let mut attributes = None::<AttributeChain>;
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

            let current_parent = scopes.last().and_then(|scope| scope.parent);
            let current_module = scopes.last().and_then(|scope| scope.module);
            let current_method_container = scopes
                .last()
                .and_then(|scope| scope.method_container)
                .filter(|_| {
                    node.parent()
                        .and_then(|parent| parent.parent())
                        .is_some_and(|parent| matches!(parent.kind(), "impl_item" | "trait_item"))
                });
            if capture.index == self.captures.type_ {
                if let Some(name) = field_text(node, "name", source) {
                    let line_start = take_attached_attributes(&mut attributes, node)
                        .map_or_else(|| line_start(node), |attributes| attributes.line_start);
                    let definition = parsed.definitions.len();
                    let (parent, impl_target) = match current_method_container {
                        Some(MethodContainer::Impl {
                            target,
                            local_name,
                            fallback_parent,
                        }) => {
                            if let Some(local_name) = local_name {
                                pending_parents.push(PendingParent {
                                    definition,
                                    local_name,
                                    module: current_module,
                                });
                            }
                            (fallback_parent, Some(target.to_owned()))
                        }
                        _ => (current_parent, None),
                    };
                    parsed.definitions.push(Definition {
                        kind: DefinitionKind::Type,
                        name: name.to_owned(),
                        parent,
                        impl_target,
                        line_start,
                        line_end: line_end(node),
                        signature: signature(node, source),
                        module: current_module,
                    });
                    if current_method_container.is_none() {
                        type_parents
                            .entry((current_module, name))
                            .and_modify(|candidate| *candidate = None)
                            .or_insert(Some(definition));
                    }
                    scopes.push(Scope {
                        end_byte: node.end_byte(),
                        parent: Some(definition),
                        method_container: Some(MethodContainer::Definition(definition)),
                        module: current_module,
                    });
                }
            } else if capture.index == self.captures.module {
                if let Some(name) = field_text(node, "name", source) {
                    let module = parsed.modules.len();
                    parsed.modules.push(Module {
                        name: name.to_owned(),
                        parent: current_module,
                    });
                    scopes.push(Scope {
                        end_byte: node.end_byte(),
                        parent: current_parent,
                        method_container: current_method_container,
                        module: Some(module),
                    });
                }
            } else if capture.index == self.captures.implementation {
                if let Some(target_node) = node.child_by_field_name("type") {
                    let target = text(target_node, source).trim();
                    let trait_target = node
                        .child_by_field_name("trait")
                        .map(|trait_node| text(trait_node, source).trim());
                    if target.len() > PATH_LIMIT
                        || trait_target.is_some_and(|target| target.len() > PATH_LIMIT)
                    {
                        return Err("Rust qualified path exceeds 1024 bytes".into());
                    }
                    let attached = take_attached_attributes(&mut attributes, node);
                    let mut children = node.walk();
                    let negative = node
                        .children(&mut children)
                        .any(|child| child.kind() == "!");
                    if let Some(trait_target) = trait_target.filter(|_| !negative) {
                        parsed.implementations.push(Implementation {
                            type_target: target.to_owned(),
                            trait_target: trait_target.to_owned(),
                            module: current_module,
                            line_start: attached.map_or_else(
                                || line_start(node),
                                |attributes| attributes.line_start,
                            ),
                            line_end: node
                                .child_by_field_name("body")
                                .map_or_else(|| line_end(node), line_start),
                        });
                    }
                    scopes.push(Scope {
                        end_byte: node.end_byte(),
                        parent: current_parent,
                        method_container: Some(MethodContainer::Impl {
                            target,
                            local_name: local_type_name(target_node, source),
                            fallback_parent: current_parent,
                        }),
                        module: current_module,
                    });
                }
            } else if capture.index == self.captures.function {
                if let Some(name) = field_text(node, "name", source) {
                    let definition = parsed.definitions.len();
                    let (kind, parent, impl_target) = match current_method_container {
                        Some(MethodContainer::Definition(parent)) => (
                            DefinitionKind::Method,
                            Some(parent),
                            Some(parsed.definitions[parent].name.clone()),
                        ),
                        Some(MethodContainer::Impl {
                            target,
                            local_name,
                            fallback_parent,
                        }) => {
                            if let Some(local_name) = local_name {
                                pending_parents.push(PendingParent {
                                    definition,
                                    local_name,
                                    module: current_module,
                                });
                            }
                            (
                                DefinitionKind::Method,
                                fallback_parent,
                                Some(target.to_owned()),
                            )
                        }
                        None => (DefinitionKind::Function, current_parent, None),
                    };
                    let attached = take_attached_attributes(&mut attributes, node);
                    let is_test = attached
                        .as_ref()
                        .is_some_and(|attributes| attributes.has_test);
                    let line_start = attached
                        .map_or_else(|| line_start(node), |attributes| attributes.line_start);
                    parsed.definitions.push(Definition {
                        kind: if is_test { DefinitionKind::Test } else { kind },
                        name: name.to_owned(),
                        parent,
                        impl_target,
                        line_start,
                        line_end: line_end(node),
                        signature: signature(node, source),
                        module: current_module,
                    });
                    scopes.push(Scope {
                        end_byte: node.end_byte(),
                        parent: Some(definition),
                        method_container: None,
                        module: current_module,
                    });
                }
            } else if capture.index == self.captures.attribute {
                if let Some(parent) = node.parent() {
                    let previous = previous_item(node).map(|sibling| sibling.id());
                    let test = is_test_attribute(text(node, source));
                    attributes = Some(match attributes.take() {
                        Some(mut chain)
                            if chain.parent == parent.id() && previous == Some(chain.last) =>
                        {
                            chain.last = node.id();
                            chain.has_test |= test;
                            chain
                        }
                        _ => AttributeChain {
                            parent: parent.id(),
                            last: node.id(),
                            has_test: test,
                            line_start: line_start(node),
                        },
                    });
                }
            } else if capture.index == self.captures.import {
                let mut paths = Vec::new();
                let mut children = node.walk();
                let exported = node
                    .named_children(&mut children)
                    .any(|child| child.kind() == "visibility_modifier");
                if let Some(argument) = node.child_by_field_name("argument") {
                    flatten_use(argument, "", source, &mut paths)?;
                }
                for path in paths {
                    parsed.imports.push(Import {
                        source: current_parent,
                        path,
                        line: line_start(node),
                        module: current_module,
                        block_local: node.parent().is_some_and(|parent| parent.kind() == "block"),
                        exported,
                    });
                }
            } else if capture.index == self.captures.binding {
                if let Some(source_definition) = current_parent {
                    collect_binding_names(node, source, source_definition, &mut parsed.bindings);
                }
            } else if capture.index == self.captures.call
                && let Some(source_definition) = current_parent
            {
                parsed.calls.push(Call {
                    source: source_definition,
                    target: text(node, source).to_owned(),
                    line: line_start(node),
                });
            }
        }
        drop(captures);

        if self.cursor.did_exceed_match_limit() {
            return Err("Rust query exceeded Tree-sitter's match limit".into());
        }
        for pending in pending_parents {
            if let Some(Some(parent)) = type_parents.get(&(pending.module, pending.local_name)) {
                parsed.definitions[pending.definition].parent = Some(*parent);
            }
        }
        Ok(parsed)
    }
}

fn capture(query: &Query, name: &str) -> Result<u32, String> {
    query
        .capture_index_for_name(name)
        .ok_or_else(|| format!("Rust query is missing @{name}"))
}

fn is_test_attribute(raw: &str) -> bool {
    let inner = raw
        .trim()
        .strip_prefix("#[")
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or_default();
    let path = inner.split_once('(').map_or(inner, |(path, _)| path).trim();
    path.rsplit("::").next() == Some("test")
}

fn take_attached_attributes(
    attributes: &mut Option<AttributeChain>,
    node: Node<'_>,
) -> Option<AttributeChain> {
    let attributes = attributes.take()?;
    (node.parent().map(|parent| parent.id()) == Some(attributes.parent)
        && previous_item(node).map(|sibling| sibling.id()) == Some(attributes.last))
    .then_some(attributes)
}

fn previous_item(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = node.prev_named_sibling()?;
        if !matches!(node.kind(), "line_comment" | "block_comment") {
            return Some(node);
        }
    }
}

fn collect_binding_names(
    node: Node<'_>,
    source: &str,
    source_definition: usize,
    bindings: &mut Vec<ValueBinding>,
) {
    let pattern = node.child_by_field_name("pattern").unwrap_or(node);
    let type_target = simple_binding_pattern(pattern)
        .then(|| node.child_by_field_name("type"))
        .flatten()
        .and_then(|type_| binding_type(type_, source));
    let mut pending = vec![pattern];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "identifier" | "shorthand_field_identifier") {
            bindings.push(ValueBinding {
                source: source_definition,
                name: text(node, source).to_owned(),
                type_target: type_target.clone(),
            });
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
}

fn simple_binding_pattern(mut node: Node<'_>) -> bool {
    while node.kind() == "mut_pattern" {
        let Some(child) = node.named_child(0) else {
            return false;
        };
        node = child;
    }
    matches!(node.kind(), "identifier" | "shorthand_field_identifier")
}

fn binding_type(mut node: Node<'_>, source: &str) -> Option<String> {
    while node.kind() == "reference_type" {
        node = node.child_by_field_name("type")?;
    }
    matches!(
        node.kind(),
        "type_identifier" | "scoped_type_identifier" | "generic_type"
    )
    .then(|| text(node, source).trim())
    .filter(|target| !target.is_empty() && target.len() <= PATH_LIMIT)
    .map(str::to_owned)
}

fn flatten_use(
    node: Node<'_>,
    prefix: &str,
    source: &str,
    paths: &mut Vec<String>,
) -> Result<(), String> {
    let mut pending = vec![(node, prefix.to_owned())];
    while let Some((node, prefix)) = pending.pop() {
        match node.kind() {
            "scoped_use_list" => {
                let Some(path) = node.child_by_field_name("path") else {
                    continue;
                };
                let prefix = join_use(&prefix, text(path, source))?;
                if let Some(list) = node.child_by_field_name("list") {
                    pending.push((list, prefix));
                }
            }
            "use_list" => {
                let mut cursor = node.walk();
                let children = node.named_children(&mut cursor).collect::<Vec<_>>();
                pending.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|child| (child, prefix.clone())),
                );
            }
            "use_as_clause" => {
                if let (Some(path), Some(alias)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("alias"),
                ) {
                    let path = if path.kind() == "self" && !prefix.is_empty() {
                        prefix
                    } else {
                        join_use(&prefix, text(path, source))?
                    };
                    let alias = text(alias, source);
                    let length = path
                        .len()
                        .checked_add(4)
                        .and_then(|length| length.checked_add(alias.len()))
                        .filter(|length| *length <= PATH_LIMIT)
                        .ok_or_else(|| "Rust import path exceeds 1024 bytes".to_owned())?;
                    let mut binding = String::with_capacity(length);
                    binding.push_str(&path);
                    binding.push_str(" as ");
                    binding.push_str(alias);
                    paths.push(binding);
                }
            }
            "use_wildcard" => paths.push(join_use(&prefix, text(node, source))?),
            "self" if !prefix.is_empty() => paths.push(prefix),
            _ => paths.push(join_use(&prefix, text(node, source))?),
        }
    }
    Ok(())
}

fn join_use(prefix: &str, path: &str) -> Result<String, String> {
    let separator = usize::from(!prefix.is_empty()) * 2;
    let length = prefix
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(path.len()))
        .filter(|length| *length <= PATH_LIMIT)
        .ok_or_else(|| "Rust import path exceeds 1024 bytes".to_owned())?;
    let mut output = String::with_capacity(length);
    if !prefix.is_empty() {
        output.push_str(prefix);
        output.push_str("::");
    }
    output.push_str(path);
    Ok(output)
}

fn field_text<'source>(node: Node<'_>, field: &str, source: &'source str) -> Option<&'source str> {
    let value = text(node.child_by_field_name(field)?, source);
    (!value.is_empty()).then_some(value)
}

fn text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    source.get(node.byte_range()).unwrap_or_default()
}

fn local_type_name<'source>(mut node: Node<'_>, source: &'source str) -> Option<&'source str> {
    loop {
        match node.kind() {
            "type_identifier" | "identifier" => return Some(text(node, source)),
            "generic_type" | "reference_type" => {
                node = node.child_by_field_name("type")?;
            }
            _ => return None,
        }
    }
}

fn signature(node: Node<'_>, source: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    let signature = source
        .get(node.start_byte()..end)
        .unwrap_or_default()
        .trim_end();
    let signature = if node.kind() == "function_signature_item" {
        signature.strip_suffix(';').unwrap_or(signature)
    } else {
        signature
    };
    let mut end = signature.len().min(SIGNATURE_LIMIT);
    while !signature.is_char_boundary(end) {
        end -= 1;
    }
    signature[..end].to_owned()
}

fn line_start(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

fn line_end(node: Node<'_>) -> usize {
    node.end_position().row + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_graph_evidence() {
        let parsed = RustParser::new()
            .unwrap()
            .parse(
                r#"use crate::transport::{Client, send as deliver};

pub struct Mailer;

impl Mailer {
    pub fn dispatch(&self) {
        Client::connect();
        self.flush();
    }
}

fn register() {
    Mailer::dispatch();
}

#[cfg(test)]
#[test]
fn register_dispatches() {
    register();
}
"#,
            )
            .unwrap();

        assert_eq!(
            parsed
                .definitions
                .iter()
                .map(|definition| (definition.kind, definition.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (DefinitionKind::Type, "Mailer"),
                (DefinitionKind::Method, "dispatch"),
                (DefinitionKind::Function, "register"),
                (DefinitionKind::Test, "register_dispatches"),
            ]
        );
        assert!(parsed.imports.iter().all(|import| !import.exported));
        assert_eq!(parsed.definitions[1].parent, Some(0));
        assert_eq!(parsed.definitions[1].impl_target.as_deref(), Some("Mailer"));
        assert_eq!(parsed.definitions[1].line_start, 6);
        assert_eq!(parsed.definitions[1].signature, "pub fn dispatch(&self)");
        assert_eq!(
            parsed
                .imports
                .iter()
                .map(|import| import.path.as_str())
                .collect::<Vec<_>>(),
            [
                "crate::transport::Client",
                "crate::transport::send as deliver"
            ]
        );
        assert_eq!(
            parsed
                .calls
                .iter()
                .map(|call| (call.source, call.target.as_str()))
                .collect::<Vec<_>>(),
            [
                (1, "Client::connect"),
                (1, "self.flush"),
                (2, "Mailer::dispatch"),
                (3, "register"),
            ]
        );
    }

    #[test]
    fn scopes_impl_parents_and_parses_malformed_source() {
        let parsed = RustParser::new()
            .unwrap()
            .parse("struct Item; mod nested { struct Item; } impl Item { fn run(&self) { broken( }")
            .unwrap();

        let method = parsed
            .definitions
            .iter()
            .find(|definition| definition.name == "run")
            .unwrap();
        assert_eq!(method.kind, DefinitionKind::Method);
        assert_eq!(method.parent, Some(0));
        assert_eq!(method.impl_target.as_deref(), Some("Item"));

        let scoped = RustParser::new()
            .unwrap()
            .parse("struct Item; impl external::Item { fn run() {} }")
            .unwrap();
        assert_eq!(scoped.definitions[1].parent, None);
        assert_eq!(
            scoped.definitions[1].impl_target.as_deref(),
            Some("external::Item")
        );
    }

    #[test]
    fn reuses_parser_and_keeps_attribute_and_nested_contexts() {
        let mut parser = RustParser::new().unwrap();
        let first = parser
            .parse(
                r#"#[allow(dead_code)]
trait Runner {
    #[must_use]
    fn declared(&self);
    #[test]
    // comments do not detach an outer attribute
    #[ignore]
    fn run() { helper(); }
}
#[inline]
// comments may sit directly between an attribute and its item
fn helper() {}
#[test]
// comments may sit directly between an attribute and its item
fn comment_test() {}
#[allow(dead_code)]
const ATTRIBUTE_OWNER: usize = 0;
fn detached() {}
"#,
            )
            .unwrap();
        let second = parser.parse("fn plain() { other(); }").unwrap();

        assert_eq!(first.definitions[1].kind, DefinitionKind::Method);
        assert_eq!(first.definitions[1].signature, "fn declared(&self)");
        assert_eq!(first.definitions[2].kind, DefinitionKind::Test);
        assert_eq!(first.definitions[2].parent, Some(0));
        assert_eq!(first.definitions[3].kind, DefinitionKind::Function);
        assert_eq!(first.definitions[4].kind, DefinitionKind::Test);
        assert_eq!(first.definitions[5].kind, DefinitionKind::Function);
        assert_eq!(
            first
                .definitions
                .iter()
                .map(|definition| definition.line_start)
                .collect::<Vec<_>>(),
            [1, 3, 5, 10, 13, 18]
        );
        assert_eq!(first.calls[0].source, 2);
        assert_eq!(second.definitions[0].kind, DefinitionKind::Function);
        assert_eq!(second.calls[0].source, 0);
    }

    #[test]
    fn records_only_positive_trait_implementations() {
        let parsed = RustParser::new()
            .unwrap()
            .parse(
                r#"mod nested {
    #[cfg(unix)]
    // comments remain part of the implementation range
    impl crate::Marker
        for super::Thing
    {}
    impl !crate::Denied for super::Thing {}
    impl super::Thing {}
}
"#,
            )
            .unwrap();

        assert_eq!(
            parsed.implementations,
            [Implementation {
                type_target: "super::Thing".into(),
                trait_target: "crate::Marker".into(),
                module: Some(0),
                line_start: 2,
                line_end: 6,
            }]
        );
    }

    #[test]
    fn records_inline_module_scopes_without_fake_definitions() {
        let parsed = RustParser::new()
            .unwrap()
            .parse("mod a { fn run() {} mod nested { fn work() {} } } mod b { fn run() {} }")
            .unwrap();

        assert_eq!(
            parsed
                .modules
                .iter()
                .map(|module| (module.name.as_str(), module.parent))
                .collect::<Vec<_>>(),
            [("a", None), ("nested", Some(0)), ("b", None)]
        );
        assert_eq!(
            parsed
                .definitions
                .iter()
                .map(|definition| (definition.name.as_str(), definition.module))
                .collect::<Vec<_>>(),
            [("run", Some(0)), ("work", Some(1)), ("run", Some(2))]
        );
    }

    #[test]
    fn truncates_signatures_on_utf8_boundaries() {
        let full = format!("fn run({}: u8)", "é".repeat(120));
        let parsed = RustParser::new()
            .unwrap()
            .parse(&format!("{full} {{}}"))
            .unwrap();
        let mut end = SIGNATURE_LIMIT;
        while !full.is_char_boundary(end) {
            end -= 1;
        }

        assert_eq!(parsed.definitions[0].signature, full[..end]);
    }

    #[test]
    fn bounds_grouped_import_expansion() {
        let source = format!("use {}::{{leaf}};", "m".repeat(PATH_LIMIT));
        assert_eq!(
            RustParser::new().unwrap().parse(&source).unwrap_err(),
            "Rust import path exceeds 1024 bytes"
        );
    }

    #[test]
    fn retains_module_and_block_wildcard_import_paths() {
        let parsed = RustParser::new()
            .unwrap()
            .parse(
                r#"
mod tests {
    use super::*;
    fn check() {
        use crate::support::*;
    }
}
"#,
            )
            .unwrap();

        assert_eq!(
            parsed
                .imports
                .iter()
                .map(|import| (import.path.as_str(), import.block_local))
                .collect::<Vec<_>>(),
            [("super::*", false), ("crate::support::*", true)]
        );
    }

    #[test]
    fn captures_only_receiver_calls_with_supported_shapes() {
        let parsed = RustParser::new()
            .unwrap()
            .parse(
                "impl Item { fn run(&self, other: Item) { local(); Item::make::<u8>(); self.work::<u8>(); other.work(); Item::make().work(); } }",
            )
            .unwrap();

        assert_eq!(
            parsed
                .calls
                .iter()
                .map(|call| call.target.as_str())
                .collect::<Vec<_>>(),
            [
                "local",
                "Item::make::<u8>",
                "self.work::<u8>",
                "other.work",
                "Item::make"
            ]
        );
        assert!(parsed.bindings.iter().any(|binding| {
            binding.name == "other" && binding.type_target.as_deref() == Some("Item")
        }));
    }
}
