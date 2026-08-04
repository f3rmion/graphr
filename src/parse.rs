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
}

#[derive(Debug, Eq, PartialEq)]
pub struct Call {
    pub source: usize,
    pub target: String,
    pub line: usize,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct ParsedFile {
    pub definitions: Vec<Definition>,
    pub modules: Vec<Module>,
    pub imports: Vec<Import>,
    pub bindings: Vec<ValueBinding>,
    pub calls: Vec<Call>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ValueBinding {
    pub source: usize,
    pub name: String,
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
            let current_method_container = scopes.last().and_then(|scope| scope.method_container);
            if capture.index == self.captures.type_ {
                if let Some(name) = field_text(node, "name", source) {
                    let definition = parsed.definitions.len();
                    parsed.definitions.push(Definition {
                        kind: DefinitionKind::Type,
                        name: name.to_owned(),
                        parent: current_parent,
                        impl_target: None,
                        line_start: line_start(node),
                        line_end: line_end(node),
                        signature: signature(node, source),
                        module: current_module,
                    });
                    type_parents
                        .entry((current_module, name))
                        .and_modify(|candidate| *candidate = None)
                        .or_insert(Some(definition));
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
                    if target.len() > PATH_LIMIT {
                        return Err("Rust qualified path exceeds 1024 bytes".into());
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
                    let is_test = attributes.as_ref().is_some_and(|attributes| {
                        attributes.has_test
                            && node.parent().map(|parent| parent.id()) == Some(attributes.parent)
                            && previous_item(node).map(|sibling| sibling.id())
                                == Some(attributes.last)
                    });
                    attributes = None;
                    parsed.definitions.push(Definition {
                        kind: if is_test { DefinitionKind::Test } else { kind },
                        name: name.to_owned(),
                        parent,
                        impl_target,
                        line_start: line_start(node),
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
                        },
                    });
                }
            } else if capture.index == self.captures.import {
                let mut paths = Vec::new();
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
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "identifier" | "shorthand_field_identifier") {
            bindings.push(ValueBinding {
                source: source_definition,
                name: text(node, source).to_owned(),
            });
            continue;
        }
        let type_child = node.child_by_field_name("type").map(|child| child.id());
        let condition = node
            .child_by_field_name("condition")
            .map(|child| child.id());
        let mut cursor = node.walk();
        pending.extend(
            node.named_children(&mut cursor)
                .filter(|child| Some(child.id()) != type_child && Some(child.id()) != condition),
        );
    }
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
            "use_wildcard" => {}
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
                r#"trait Runner {
    fn declared(&self);
    #[test]
    // comments do not detach an outer attribute
    #[ignore]
    fn run() { helper(); }
}
#[test]
// comments may sit directly between an attribute and its item
fn comment_test() {}
"#,
            )
            .unwrap();
        let second = parser.parse("fn plain() { other(); }").unwrap();

        assert_eq!(first.definitions[1].kind, DefinitionKind::Method);
        assert_eq!(first.definitions[1].signature, "fn declared(&self)");
        assert_eq!(first.definitions[2].kind, DefinitionKind::Test);
        assert_eq!(first.definitions[2].parent, Some(0));
        assert_eq!(first.definitions[3].kind, DefinitionKind::Test);
        assert_eq!(first.calls[0].source, 2);
        assert_eq!(second.definitions[0].kind, DefinitionKind::Function);
        assert_eq!(second.calls[0].source, 0);
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
    fn skips_receiver_calls_that_cannot_be_resolved() {
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
            ["local", "Item::make::<u8>", "self.work::<u8>", "Item::make"]
        );
    }
}
